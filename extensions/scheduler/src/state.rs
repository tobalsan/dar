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
    /// When the currently-executing run started (UTC epoch millis); `Some` only
    /// while a run is in flight, used to compute `running-for`.
    pub running_since_ms: Option<i64>,
    /// UTC epoch millis of the last scheduled fire that was overlap-skipped
    /// because a run was already in flight. Run-now reads this to decide whether
    /// to restore the pre-run next fire or keep the loop's recomputed one.
    pub last_skipped_at_ms: Option<i64>,
}

/// Shared, mutable scheduler state. Cheap to clone behind an `Arc`.
pub struct SchedulerState {
    inner: Mutex<Inner>,
    /// Notified whenever jobs change so the loop re-arms its timer.
    changed: Notify,
}

struct Inner {
    jobs: Vec<ScheduleJob>,
    runtime: HashMap<String, JobRuntime>,
}

impl SchedulerState {
    pub fn new(jobs: Vec<ScheduleJob>) -> Self {
        Self {
            inner: Mutex::new(Inner {
                jobs,
                runtime: HashMap::new(),
            }),
            changed: Notify::new(),
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

    /// Replace the job set wholesale (after a create/update/delete) and wake the
    /// loop. Runtime entries for removed jobs are pruned.
    pub fn set_jobs(&self, jobs: Vec<ScheduleJob>) {
        {
            let mut inner = self.lock();
            inner
                .runtime
                .retain(|id, _| jobs.iter().any(|j| &j.id == id));
            inner.jobs = jobs;
        }
        self.changed.notify_one();
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

    /// Atomically claim a run for this job: set `running_since_ms` and return
    /// `true` only if no run was already in flight. This is the single source of
    /// truth both the timer loop and the run-now handler use to gate overlap, so
    /// there is no publish gap between a "claimed" check and the mark — a
    /// concurrent scheduled fire and manual run-now (or two run-nows) can never
    /// both win the claim.
    pub fn try_claim_running(&self, job_id: &str, started_at_ms: i64) -> bool {
        let mut inner = self.lock();
        let rt = inner.runtime.entry(job_id.to_string()).or_default();
        if rt.running_since_ms.is_some() {
            return false;
        }
        rt.running_since_ms = Some(started_at_ms);
        rt.last_run_at_ms = Some(started_at_ms);
        true
    }

    /// True if a run of this job is currently in flight (scheduled or manual).
    /// Test-only: production overlap gating goes through `try_claim_running`.
    #[cfg(test)]
    pub fn is_running(&self, job_id: &str) -> bool {
        self.lock()
            .runtime
            .get(job_id)
            .is_some_and(|rt| rt.running_since_ms.is_some())
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
    pub fn last_skipped_at_ms(&self, job_id: &str) -> Option<i64> {
        self.lock()
            .runtime
            .get(job_id)
            .and_then(|rt| rt.last_skipped_at_ms)
    }

    /// Mark a run as finished with its status and, for an error run, the error
    /// message (cleared on a subsequent `ok` run).
    pub fn mark_finished(&self, job_id: &str, status: LastStatus, error: Option<String>) {
        let mut inner = self.lock();
        let rt = inner.runtime.entry(job_id.to_string()).or_default();
        rt.running_since_ms = None;
        rt.last_status = Some(status);
        rt.last_error = match status {
            LastStatus::Error => error,
            LastStatus::Ok => None,
        };
    }

    /// Release a run claim without recording a status. Idempotent: used by the
    /// fire task's drop guard so a panicking run still frees the overlap gate
    /// (a normal completion already cleared it via `mark_finished`).
    pub fn clear_running(&self, job_id: &str) {
        if let Some(rt) = self.lock().runtime.get_mut(job_id) {
            rt.running_since_ms = None;
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
                message: "x".to_string(),
            },
            timeout_ms: None,
        }
    }

    #[test]
    fn set_jobs_prunes_runtime_for_removed_jobs() {
        let state = SchedulerState::new(vec![job("a"), job("b")]);
        assert!(state.try_claim_running("a", 1));
        assert!(state.try_claim_running("b", 2));
        state.set_jobs(vec![job("a")]);
        assert!(state.runtime("a").last_run_at_ms.is_some());
        // b removed → runtime cleared.
        assert_eq!(state.runtime("b").last_run_at_ms, None);
    }

    #[test]
    fn running_then_finished_updates_status() {
        let state = SchedulerState::new(vec![job("a")]);
        assert!(state.try_claim_running("a", 100));
        assert_eq!(state.runtime("a").running_since_ms, Some(100));
        // A second claim while running is rejected.
        assert!(!state.try_claim_running("a", 200));
        state.mark_finished("a", LastStatus::Ok, None);
        let rt = state.runtime("a");
        assert_eq!(rt.running_since_ms, None);
        assert_eq!(rt.last_status, Some(LastStatus::Ok));
        assert_eq!(rt.last_run_at_ms, Some(100));
    }

    #[test]
    fn error_run_records_message_and_ok_run_clears_it() {
        let state = SchedulerState::new(vec![job("a")]);
        assert!(state.try_claim_running("a", 100));
        state.mark_finished("a", LastStatus::Error, Some("boom".to_string()));
        let rt = state.runtime("a");
        assert_eq!(rt.last_status, Some(LastStatus::Error));
        assert_eq!(rt.last_error.as_deref(), Some("boom"));
        // A later ok run clears the stale error.
        assert!(state.try_claim_running("a", 200));
        state.mark_finished("a", LastStatus::Ok, None);
        assert!(state.runtime("a").last_error.is_none());
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
