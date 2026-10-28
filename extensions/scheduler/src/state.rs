//! Shared scheduler state bridging the HTTP CRUD handlers and the timer loop.
//!
//! The loop ([`crate::service`]) and the HTTP API ([`crate::http`]) both hold an
//! `Arc<SchedulerState>`. Jobs (configuration) and per-job runtime state
//! (next/last run, last status, running-for) live behind a `Mutex`. A
//! [`tokio::sync::Notify`] lets a mutation (create/update/delete) wake the loop
//! so a sooner schedule takes effect immediately instead of after the current
//! sleep.
//!
//! Only the [`crate::store::ScheduleJob`] config is ever persisted; runtime
//! state stays here and is merged into list responses.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use tokio::sync::Notify;

use crate::store::ScheduleJob;

/// Coarse run status surfaced in list responses, mirroring aihub's last-status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LastStatus {
    Ok,
    Error,
}

impl LastStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            LastStatus::Ok => "ok",
            LastStatus::Error => "error",
        }
    }
}

/// Details published by the most recently completed run.
pub struct RunCompletion {
    pub status: LastStatus,
    pub error: Option<String>,
    pub exit_code: Option<i32>,
    pub run_kind: Option<String>,
    pub delivery: Vec<String>,
}

/// A claim identifies one run across completion, panic, and job replacement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RunToken(u64);

/// In-memory runtime state for one job. Never persisted.
#[derive(Clone, Debug, Default)]
pub struct JobRuntime {
    /// Next computed fire instant (UTC epoch millis), if armed.
    pub next_run_at_ms: Option<i64>,
    /// Last fire instant (UTC epoch millis), if it has ever run this process.
    pub last_run_at_ms: Option<i64>,
    /// Status of the last completed run.
    pub last_status: Option<LastStatus>,
    /// Error message of the last completed run, set only when that run ended in
    /// [`LastStatus::Error`]. Cleared on the next successful run. Surfaced by the
    /// dashboard Cron tab so a failed job shows *why* it failed.
    pub last_error: Option<String>,
    /// Script process exit code for the last script failure, when available.
    pub last_exit_code: Option<i32>,
    /// Script execution disposition: `silent_tick`, `woke_agent`, or
    /// `script_only`; absent for ordinary agent jobs.
    pub last_run_kind: Option<String>,
    /// Per-target delivery outcomes from the last run.
    pub last_delivery: Vec<String>,
    /// Active run claims and their UTC start times. Never persisted.
    active_runs: HashMap<RunToken, i64>,
    /// UTC epoch millis of the last scheduled fire skipped at capacity.
    pub last_skipped_at_ms: Option<i64>,
}

impl JobRuntime {
    pub fn running_count(&self) -> usize {
        self.active_runs.len()
    }

    pub fn oldest_active_start(&self) -> Option<i64> {
        self.active_runs.values().copied().min()
    }

    pub fn at_capacity(&self, max_concurrent: u32) -> bool {
        self.running_count() >= max_concurrent as usize
    }
}

/// Shared, mutable scheduler state. Cheap to clone behind an `Arc`.
pub struct SchedulerState {
    inner: Mutex<Inner>,
    pub history: Option<crate::history::History>,
    shutdown: Mutex<Option<host_api::ShutdownToken>>,
    manual_runs: AtomicUsize,
    /// Notified whenever jobs change so the loop re-arms its timer.
    changed: Notify,
}

struct Inner {
    jobs: Vec<ScheduleJob>,
    next_run_token: u64,
    runtime: HashMap<String, JobRuntime>,
}

impl SchedulerState {
    pub fn new(jobs: Vec<ScheduleJob>) -> Self {
        Self {
            history: None,
            shutdown: Mutex::new(None),
            manual_runs: AtomicUsize::new(0),
            inner: Mutex::new(Inner {
                jobs,
                next_run_token: 0,
                runtime: HashMap::new(),
            }),
            changed: Notify::new(),
        }
    }

    pub fn with_history(jobs: Vec<ScheduleJob>, history: crate::history::History) -> Self {
        Self {
            history: Some(history),
            ..Self::new(jobs)
        }
    }

    pub fn set_shutdown(&self, shutdown: host_api::ShutdownToken) {
        *self
            .shutdown
            .lock()
            .expect("scheduler shutdown mutex poisoned") = Some(shutdown);
    }

    pub fn shutdown(&self) -> Option<host_api::ShutdownToken> {
        self.shutdown
            .lock()
            .expect("scheduler shutdown mutex poisoned")
            .clone()
    }

    pub fn manual_started(&self) {
        self.manual_runs.fetch_add(1, Ordering::SeqCst);
    }

    pub fn manual_finished(&self) {
        self.manual_runs.fetch_sub(1, Ordering::SeqCst);
    }

    pub async fn wait_manual_runs(&self) {
        while self.manual_runs.load(Ordering::SeqCst) != 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Snapshot the current jobs (config only).
    pub fn jobs(&self) -> Vec<ScheduleJob> {
        self.lock().jobs.clone()
    }

    /// Snapshot one job's runtime state.
    pub fn runtime(&self, job_id: &str) -> JobRuntime {
        self.lock().runtime.get(job_id).cloned().unwrap_or_default()
    }

    /// Replace the job set wholesale and wake the loop. Used when file-backed
    /// configuration is reloaded; runtime entries for removed jobs are pruned.
    pub fn set_jobs(&self, jobs: Vec<ScheduleJob>) {
        self.replace_jobs(jobs);
        self.changed.notify_one();
    }

    /// Atomically derive, persist, and install a jobs mutation.
    ///
    /// The callback runs while the jobs lock is held and must persist its
    /// returned vector before returning. This serializes the complete
    /// read-modify-persist-install transaction across HTTP and tool callers,
    /// while leaving file I/O and caller-specific errors at those interfaces.
    pub fn mutate_jobs<T, E>(
        &self,
        mutation: impl FnOnce(&[ScheduleJob]) -> Result<(Vec<ScheduleJob>, T), E>,
    ) -> Result<T, E> {
        let result = {
            let mut inner = self.lock();
            let (jobs, result) = mutation(&inner.jobs)?;
            inner
                .runtime
                .retain(|id, _| jobs.iter().any(|j| &j.id == id));
            inner.jobs = jobs;
            result
        };
        self.changed.notify_one();
        Ok(result)
    }

    fn replace_jobs(&self, jobs: Vec<ScheduleJob>) {
        let mut inner = self.lock();
        inner
            .runtime
            .retain(|id, _| jobs.iter().any(|j| &j.id == id));
        inner.jobs = jobs;
    }

    /// Record a job's next computed fire instant (or clear it when disabled).
    ///
    /// Inserting a new runtime entry only happens when `next_run_at_ms` is
    /// `Some`: calling with `None` for an unknown job id is a no-op so that
    /// disabled jobs don't accumulate empty runtime entries.
    pub fn set_next_run(&self, job_id: &str, next_run_at_ms: Option<i64>) {
        let mut inner = self.lock();
        if let Some(rt) = inner.runtime.get_mut(job_id) {
            rt.next_run_at_ms = next_run_at_ms;
        } else if let Some(ms) = next_run_at_ms {
            inner.runtime.insert(
                job_id.to_string(),
                JobRuntime {
                    next_run_at_ms: Some(ms),
                    ..Default::default()
                },
            );
        }
        // Neither branch: job has no runtime entry and next_run_at_ms is None → no-op.
    }

    /// Atomically claim one slot below the cap shared by scheduled and manual runs.
    pub fn try_claim_running(
        &self,
        job_id: &str,
        started_at_ms: i64,
        max_concurrent: u32,
    ) -> Option<RunToken> {
        let mut inner = self.lock();
        if inner
            .runtime
            .get(job_id)
            .is_some_and(|rt| rt.at_capacity(max_concurrent))
            || max_concurrent == 0
        {
            return None;
        }
        let token = RunToken(inner.next_run_token);
        inner.next_run_token += 1;
        let rt = inner.runtime.entry(job_id.to_string()).or_default();
        rt.active_runs.insert(token, started_at_ms);
        rt.last_run_at_ms = Some(started_at_ms);
        Some(token)
    }

    /// True if any scheduled or manual run is in flight.
    #[cfg(test)]
    pub fn is_running(&self, job_id: &str) -> bool {
        self.runtime(job_id).running_count() > 0
    }

    /// Bookmark a scheduled fire that was overlap-skipped at `at_ms`.
    pub fn mark_skipped(&self, job_id: &str, at_ms: i64) {
        let mut inner = self.lock();
        inner
            .runtime
            .entry(job_id.to_string())
            .or_default()
            .last_skipped_at_ms = Some(at_ms);
    }

    /// The last overlap-skip bookmark for a job, if any.
    #[cfg(test)]
    pub fn last_skipped_at_ms(&self, job_id: &str) -> Option<i64> {
        self.lock()
            .runtime
            .get(job_id)
            .and_then(|rt| rt.last_skipped_at_ms)
    }

    /// Mark a run as finished with its status and, for an error run, the error
    /// message (cleared on a subsequent `ok` run).
    #[cfg(test)]
    pub fn mark_finished(
        &self,
        job_id: &str,
        token: Option<RunToken>,
        status: LastStatus,
        error: Option<String>,
    ) {
        self.mark_finished_details(
            job_id,
            token,
            RunCompletion {
                status,
                error,
                exit_code: None,
                run_kind: None,
                delivery: Vec::new(),
            },
        );
    }

    pub fn mark_finished_details(
        &self,
        job_id: &str,
        token: Option<RunToken>,
        completion: RunCompletion,
    ) {
        let RunCompletion {
            status,
            error,
            exit_code,
            run_kind,
            delivery,
        } = completion;
        let mut inner = self.lock();
        let Some(rt) = inner.runtime.get_mut(job_id) else {
            return;
        };
        let Some(token) = token else { return };
        if rt.active_runs.remove(&token).is_none() {
            return;
        }
        rt.last_status = Some(status);
        rt.last_error = match status {
            LastStatus::Error => error,
            LastStatus::Ok => None,
        };
        rt.last_exit_code = exit_code;
        rt.last_run_kind = run_kind;
        rt.last_delivery = delivery;
    }

    /// Release a run claim without recording a status. Idempotent: used by the
    /// fire task's drop guard so a panicking run still frees the overlap gate
    /// (a normal completion already cleared it via `mark_finished`).
    pub fn clear_running(&self, job_id: &str, token: RunToken) {
        if let Some(rt) = self.lock().runtime.get_mut(job_id) {
            rt.active_runs.remove(&token);
        }
    }

    /// Wait until the next `set_jobs` mutation. Used by the loop to re-arm.
    pub async fn changed(&self) {
        self.changed.notified().await;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("scheduler state mutex poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Payload, Schedule};

    fn job(id: &str) -> ScheduleJob {
        ScheduleJob {
            id: id.to_string(),
            name: String::new(),
            enabled: true,
            schedule: Schedule {
                cron: "* * * * *".to_string(),
                tz: "UTC".to_string(),
                start_at: None,
            },
            payload: Payload {
                message: Some("x".to_string()),
                script: None,
                no_agent: false,
                quiet_output: false,
            },
            timeout_ms: None,
            max_concurrent: None,
            deliver: Vec::new(),
        }
    }

    #[test]
    fn cap_two_accepts_two_claims() {
        let state = SchedulerState::new(vec![job("a")]);
        assert!(state.try_claim_running("a", 100, 2).is_some());
        assert!(state.try_claim_running("a", 200, 2).is_some());
    }

    #[test]
    fn releasing_a_claim_preserves_siblings_and_ignores_stale_completions() {
        let state = SchedulerState::new(vec![job("a")]);
        let first = state.try_claim_running("a", 200, 2).unwrap();
        let oldest = state.try_claim_running("a", 100, 2).unwrap();
        assert!(state.try_claim_running("a", 300, 2).is_none());
        assert_eq!(state.runtime("a").running_count(), 2);
        assert_eq!(state.runtime("a").oldest_active_start(), Some(100));
        assert_eq!(state.runtime("a").last_run_at_ms, Some(100));

        state.mark_finished("a", Some(first), LastStatus::Ok, None);
        assert_eq!(state.runtime("a").running_count(), 1);
        assert_eq!(state.runtime("a").oldest_active_start(), Some(100));
        let third = state.try_claim_running("a", 400, 2).unwrap();
        state.clear_running("a", first); // Drop after normal completion is idempotent.
        state.mark_finished("a", Some(first), LastStatus::Error, Some("stale".into()));
        state.mark_finished("a", None, LastStatus::Error, Some("absent".into()));
        assert_eq!(state.runtime("a").last_status, Some(LastStatus::Ok));
        assert!(state.try_claim_running("a", 500, 2).is_none());
        state.clear_running("a", oldest);
        state.clear_running("a", oldest);
        assert_eq!(state.runtime("a").oldest_active_start(), Some(400));
        assert!(state.try_claim_running("a", 500, 2).is_some());
        state.mark_finished("a", Some(third), LastStatus::Error, Some("latest".into()));
        assert_eq!(state.runtime("a").last_error.as_deref(), Some("latest"));
        assert_eq!(state.runtime("a").running_count(), 1);
    }

    #[test]
    fn concurrent_claims_never_exceed_cap() {
        let state = std::sync::Arc::new(SchedulerState::new(vec![job("a")]));
        let start = std::sync::Arc::new(std::sync::Barrier::new(16));
        let handles: Vec<_> = (0..16)
            .map(|i| {
                let state = state.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    state.try_claim_running("a", i, 2)
                })
            })
            .collect();
        let claims = handles
            .into_iter()
            .filter_map(|h| h.join().unwrap())
            .count();
        assert_eq!(claims, 2);
        assert_eq!(state.runtime("a").running_count(), 2);
    }

    #[test]
    fn edits_preserve_claims_and_deleted_runs_cannot_release_recreated_slots() {
        let state = SchedulerState::new(vec![job("a")]);
        let stale = state.try_claim_running("a", 100, 2).unwrap();
        assert!(state.try_claim_running("a", 200, 2).is_some());
        let mut edited = job("a");
        edited.max_concurrent = Some(1);
        state.set_jobs(vec![edited]);
        assert_eq!(state.runtime("a").running_count(), 2);
        assert!(state.try_claim_running("a", 300, 1).is_none());
        state.set_jobs(vec![]);
        state.mark_finished("a", Some(stale), LastStatus::Ok, None);
        assert_eq!(state.runtime("a").last_status, None);
        assert_eq!(state.runtime("a").running_count(), 0);
        state.set_jobs(vec![job("a")]);
        assert!(state.try_claim_running("a", 400, 1).is_some());
        state.clear_running("a", stale);
        state.mark_finished("a", Some(stale), LastStatus::Error, None);
        assert!(state.try_claim_running("a", 500, 1).is_none());
        assert_eq!(state.runtime("a").last_status, None);
    }

    #[test]
    fn set_jobs_prunes_runtime_for_removed_jobs() {
        let state = SchedulerState::new(vec![job("a"), job("b")]);
        assert!(state.try_claim_running("a", 1, 1).is_some());
        assert!(state.try_claim_running("b", 2, 1).is_some());
        state.set_jobs(vec![job("a")]);
        assert!(state.runtime("a").last_run_at_ms.is_some());
        // b removed → runtime cleared.
        assert_eq!(state.runtime("b").last_run_at_ms, None);
    }

    #[test]
    fn running_then_finished_updates_status() {
        let state = SchedulerState::new(vec![job("a")]);
        let token = state.try_claim_running("a", 100, 1).unwrap();
        assert_eq!(state.runtime("a").oldest_active_start(), Some(100));
        // A second claim while running is rejected.
        assert!(state.try_claim_running("a", 200, 1).is_none());
        state.mark_finished("a", Some(token), LastStatus::Ok, None);
        let rt = state.runtime("a");
        assert_eq!(rt.oldest_active_start(), None);
        assert_eq!(rt.last_status, Some(LastStatus::Ok));
        assert_eq!(rt.last_run_at_ms, Some(100));
    }

    #[test]
    fn error_run_records_message_and_ok_run_clears_it() {
        let state = SchedulerState::new(vec![job("a")]);
        let token = state.try_claim_running("a", 100, 1).unwrap();
        state.mark_finished(
            "a",
            Some(token),
            LastStatus::Error,
            Some("boom".to_string()),
        );
        let rt = state.runtime("a");
        assert_eq!(rt.last_status, Some(LastStatus::Error));
        assert_eq!(rt.last_error.as_deref(), Some("boom"));
        // A later ok run clears the stale error.
        let token = state.try_claim_running("a", 200, 1).unwrap();
        state.mark_finished("a", Some(token), LastStatus::Ok, None);
        assert!(state.runtime("a").last_error.is_none());
    }

    #[test]
    fn concurrent_mutations_preserve_both_updates() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::sync::Arc::new(dir.path().to_path_buf());
        let state = std::sync::Arc::new(SchedulerState::new(vec![]));
        let start = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut threads = Vec::new();
        for id in ["a", "b"] {
            let root = root.clone();
            let state = state.clone();
            let start = start.clone();
            threads.push(std::thread::spawn(move || {
                start.wait();
                state
                    .mutate_jobs(|current| {
                        let mut jobs = current.to_vec();
                        jobs.push(job(id));
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        crate::store::save_jobs(&root, &jobs).map(|()| (jobs, ()))
                    })
                    .unwrap();
            }));
        }
        start.wait();
        for thread in threads {
            thread.join().unwrap();
        }

        let mut memory_ids: Vec<_> = state.jobs().into_iter().map(|job| job.id).collect();
        memory_ids.sort();
        let mut disk_ids: Vec<_> = crate::store::load_jobs(&root, |_| {})
            .into_iter()
            .map(|job| job.id)
            .collect();
        disk_ids.sort();
        assert_eq!(memory_ids, ["a", "b"]);
        assert_eq!(disk_ids, memory_ids);
    }

    #[tokio::test]
    async fn changed_fires_on_set_jobs() {
        let state = std::sync::Arc::new(SchedulerState::new(vec![]));
        let waiter = {
            let state = state.clone();
            tokio::spawn(async move { state.changed().await })
        };
        // Give the waiter a moment to register.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        state.set_jobs(vec![job("a")]);
        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("changed should fire")
            .unwrap();
    }
}
