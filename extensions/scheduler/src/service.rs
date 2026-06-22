//! Scheduler runtime: read jobs from shared state, compute next fires, arm a
//! single timer for the earliest, and on each tick fire all due jobs
//! concurrently before re-arming. A create/update/delete over the HTTP API
//! wakes the loop ([`SchedulerState::changed`]) so a sooner schedule takes
//! effect immediately instead of after the current sleep.
//!
//! Execution guards (ALG-221):
//! - **Overlap-skip:** a scheduled fire of a job that is still running from a
//!   previous fire is skipped (warning logged), the skip is bookmarked
//!   (`last_skipped_at_ms`) so later run-now logic can tell the cases apart, and
//!   the job's next fire is recomputed so the timer re-arms forward.
//! - **Timeout:** every run is bounded by a timeout (per-job `timeoutMs`, else
//!   `extensions.scheduler.jobTimeoutMs`, else a 10-minute default). On timeout
//!   the runner child is killed and the run is recorded as an error.
//! - **Re-arm on every path:** the timer loop recomputes the next fire after
//!   every tick; a fired job runs in its own task so a hung/panicking/erroring
//!   job never wedges the loop, and a `RunningGuard` clears the overlap flag on
//!   drop so even a panicking fire releases the job for its next occurrence.
//!
//! Hot reload (ALG-223): in addition to HTTP mutations, the loop polls
//! `cron/jobs.json` on `poll_interval_ms`. When the file's fingerprint (mtime +
//! length) changes, the file is reloaded from disk and pushed into shared state
//! ([`SchedulerState::set_jobs`]), so an agent or a human editing the file gets
//! schedule changes applied within the poll interval without restarting the
//! host. The reload preserves per-job overlap guards: a job that is mid-run when
//! its definition is edited keeps its `running` guard (overlap-skip still
//! applies), and a job deleted while running is dropped from the armed set but
//! its in-flight run owns its own guard handle and completes normally (never
//! orphan-tracked twice). Per-job `enabled: false` inside the file is
//! live-reloaded; the boot-time `extensions.scheduler.enabled` kill switch is
//! not (it is read once at start).
//!
//! Tracked separately: dashboard tab (ALG-225).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use cap_runner::ExitKind;
use chrono::Utc;
use host_api::{ServiceRegistry, ShutdownToken};

use crate::output::{write_cron_run_output, CronRunOutput, RunStatus};
use crate::runner::fire_runner;
use crate::schedule::{compute_next_run_at_ms, format_schedule};
use crate::state::{LastStatus, SchedulerState};
use crate::store::{jobs_path, load_jobs, ScheduleJob};

/// Floor so a misconfigured tiny poll interval cannot spin the loop.
const MIN_POLL_INTERVAL_MS: u64 = 250;

/// Static configuration the runtime needs to fire runs, resolved once at start
/// from `agent.yaml`.
#[derive(Clone)]
pub struct SchedulerConfig {
    pub root: PathBuf,
    pub runner_kind: String,
    pub runner_command: String,
    pub max_run_timeout_ms: u64,
    /// How often (ms) to poll `cron/jobs.json` for external edits (hot reload).
    pub poll_interval_ms: u64,
    /// Default per-run execution timeout (ms) from
    /// `extensions.scheduler.jobTimeoutMs`, or the 10-minute fallback. A job's
    /// own `timeoutMs` overrides this.
    pub job_timeout_ms: u64,
}

impl SchedulerConfig {
    /// Effective timeout for a job: per-job `timeoutMs` overrides the
    /// extension/global default.
    fn timeout_for(&self, job: &ScheduleJob) -> Duration {
        Duration::from_millis(job.timeout_ms.unwrap_or(self.job_timeout_ms))
    }
}

/// RAII guard that clears a job's `running` flag on drop, so the overlap-skip
/// guard is released even if the fire task panics (a tokio task panic unwinds
/// through this drop but would skip any trailing statement).
struct RunningGuard(Arc<AtomicBool>);

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// A loaded job paired with its computed next-fire instant (UTC epoch millis)
/// and its runtime guard state.
struct ArmedJob {
    job: ScheduleJob,
    next_run_at_ms: i64,
    /// Set while a fire of this job is in flight. A scheduled fire that lands
    /// while this is true is skipped (overlap-skip). Shared with the spawned
    /// run task, which clears it on completion. Preserved across reloads/re-arms
    /// for surviving jobs so overlap-skip applies even to a job edited mid-run.
    running: Arc<AtomicBool>,
    /// UTC epoch millis of the last skipped fire (overlap-skip bookmark), or
    /// `None` if none has been skipped. Run-now logic (a later slice) reads
    /// this to distinguish a skip from a normal completion.
    last_skipped_at_ms: Option<i64>,
}

/// Fingerprint of `cron/jobs.json` used to detect edits cheaply: modified time
/// plus length. A change in either triggers a reload. `None` means the file is
/// currently absent.
///
/// Blind spot: a same-length edit landing within the same modified-time tick (on
/// filesystems with coarse mtime granularity) can be missed. For the
/// human/agent-paced self-service edit case this is acceptable; a content hash
/// would be the robust-but-heavier alternative.
#[derive(Clone, PartialEq, Eq)]
struct FileFingerprint {
    mtime_ms: i64,
    len: u64,
}

fn fingerprint(config: &SchedulerConfig) -> Option<FileFingerprint> {
    let meta = std::fs::metadata(jobs_path(&config.root)).ok()?;
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Some(FileFingerprint {
        mtime_ms,
        len: meta.len(),
    })
}

/// Run the scheduler loop until shutdown. Jobs are read from shared state, so a
/// create/update/delete over the HTTP API is picked up on the next re-arm, and
/// an external edit to `cron/jobs.json` is picked up by the poll tick. The loop
/// selects on a shutdown signal, a sleep until the earliest fire, a `changed`
/// notification that re-arms immediately when jobs are mutated, and a poll tick
/// that reloads the file when it changes on disk.
pub async fn run(
    config: SchedulerConfig,
    services: ServiceRegistry,
    state: Arc<SchedulerState>,
    mut shutdown: ShutdownToken,
) {
    let mut armed = arm_jobs(&state, &mut HashMap::new());
    let mut fingerprint_seen = fingerprint(&config);
    if armed.is_empty() {
        tracing::info!("[scheduler] No enabled jobs; idle (polling for edits)");
    } else {
        tracing::info!("[scheduler] Started with {} job(s)", armed.len());
    }

    let poll = Duration::from_millis(config.poll_interval_ms.max(MIN_POLL_INTERVAL_MS));

    loop {
        let next = armed.iter().map(|a| a.next_run_at_ms).min();
        let fire_delay = next.map(next_delay);
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = state.changed() => {
                // Jobs mutated over HTTP (or pushed by a reload below):
                // recompute fires so a sooner schedule takes effect immediately,
                // preserving in-flight overlap guards for surviving jobs.
                armed = arm_jobs(&state, &mut guards_of(&armed));
            }
            _ = tokio::time::sleep(poll) => {
                maybe_reload(&config, &state, &mut fingerprint_seen);
            }
            _ = sleep_opt(fire_delay), if fire_delay.is_some() => {
                run_due_jobs(&config, &services, &state, &mut armed).await;
            }
        }
    }
}

/// Sleep for `delay` if present, else never resolve (the `if` guard on the
/// `select!` arm keeps this branch inert when there is no armed fire).
async fn sleep_opt(delay: Option<Duration>) {
    match delay {
        Some(d) => tokio::time::sleep(d).await,
        None => std::future::pending().await,
    }
}

/// Snapshot the running guards of the currently-armed jobs, keyed by id, so a
/// re-arm (HTTP mutation or file reload) that keeps a job — even with an edited
/// schedule — keeps its in-flight overlap guard.
fn guards_of(armed: &[ArmedJob]) -> HashMap<String, Arc<AtomicBool>> {
    armed
        .iter()
        .map(|a| (a.job.id.clone(), Arc::clone(&a.running)))
        .collect()
}

/// If `cron/jobs.json` changed since last seen, reload it from disk and push the
/// new job set into shared state. `set_jobs` notifies the loop, which re-arms
/// (preserving in-flight guards) on the next iteration. A malformed file loads
/// as empty (one warning) and recovers on the next valid write.
fn maybe_reload(
    config: &SchedulerConfig,
    state: &SchedulerState,
    fingerprint_seen: &mut Option<FileFingerprint>,
) {
    let current = fingerprint(config);
    if current == *fingerprint_seen {
        return;
    }
    *fingerprint_seen = current;

    let jobs = load_jobs(&config.root, |m| tracing::warn!("{m}"));
    tracing::info!("[scheduler] Reloaded cron/jobs.json: {} job(s)", jobs.len());
    // Push into shared state so the HTTP API and the loop observe the same jobs.
    // This wakes the loop via `changed`, which re-arms preserving in-flight
    // guards (a job deleted while running keeps its own guard handle in its
    // spawned task and is simply dropped from the armed set).
    state.set_jobs(jobs);
}

/// Milliseconds until `next_at_ms`, clamped to zero.
fn next_delay(next_at_ms: i64) -> Duration {
    let now = Utc::now().timestamp_millis();
    Duration::from_millis((next_at_ms - now).max(0) as u64)
}

/// Read jobs from shared state and compute each enabled job's next fire,
/// publishing the computed `nextRunAt` back into runtime state. Disabled jobs
/// clear their `nextRunAt`. Jobs whose schedule fails to parse are skipped with
/// a warning. `guards` carries forward the overlap guard for any job id present
/// from a prior load; jobs new to this load get a fresh, idle guard.
fn arm_jobs(state: &SchedulerState, guards: &mut HashMap<String, Arc<AtomicBool>>) -> Vec<ArmedJob> {
    let now_ms = Utc::now().timestamp_millis();
    let jobs = state.jobs();
    let mut armed = Vec::new();
    for job in jobs {
        if !job.enabled {
            state.set_next_run(&job.id, None);
            continue;
        }
        match compute_next_run_at_ms(&job.schedule, now_ms) {
            Ok(next_run_at_ms) => {
                state.set_next_run(&job.id, Some(next_run_at_ms));
                let running = guards
                    .remove(&job.id)
                    .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
                armed.push(ArmedJob {
                    job,
                    next_run_at_ms,
                    running,
                    last_skipped_at_ms: None,
                });
            }
            Err(err) => {
                state.set_next_run(&job.id, None);
                tracing::warn!("[scheduler] Skipping job {}: bad schedule: {err:#}", job.id)
            }
        }
    }
    armed
}

/// Fire every job whose next-fire instant is now due. A job still running from a
/// previous fire is skipped (overlap-skip) and bookmarked; the rest fire
/// concurrently in their own tasks. Every due job's next fire is recomputed so
/// the timer always re-arms forward, even for skipped, hung, or erroring jobs.
async fn run_due_jobs(
    config: &SchedulerConfig,
    services: &ServiceRegistry,
    state: &Arc<SchedulerState>,
    armed: &mut [ArmedJob],
) {
    let now_ms = Utc::now().timestamp_millis();
    let due: Vec<usize> = armed
        .iter()
        .enumerate()
        .filter(|(_, a)| now_ms >= a.next_run_at_ms)
        .map(|(i, _)| i)
        .collect();

    if due.is_empty() {
        return;
    }

    let config = Arc::new(config.clone());
    for &idx in &due {
        // Overlap-skip: a previous fire of this job is still in flight. Skip
        // this fire, bookmark it, and let the re-arm below recompute the next
        // fire. The running task is left untouched.
        if armed[idx].running.load(Ordering::SeqCst) {
            armed[idx].last_skipped_at_ms = Some(now_ms);
            tracing::warn!(
                "[scheduler] Skipping fire of job {}: previous run still in progress",
                armed[idx].job.id
            );
            continue;
        }

        // Mark running and spawn the fire in its own task so a hung, panicking,
        // or erroring run cannot block the schedule loop. The `running` flag is
        // cleared by a drop guard so it is released even if the task panics
        // (otherwise that job would be overlap-skipped forever).
        armed[idx].running.store(true, Ordering::SeqCst);
        let job = armed[idx].job.clone();
        let config = Arc::clone(&config);
        let services = services.clone();
        let state = Arc::clone(state);
        let guard = RunningGuard(Arc::clone(&armed[idx].running));
        tokio::spawn(async move {
            let _guard = guard;
            execute_job(&config, &services, &state, &job).await;
        });
    }

    // Re-arm every due job forward from now so the next tick lands on the
    // following occurrence, publishing the new `nextRunAt` into runtime state.
    // Runs in flight from this or a prior tick keep ticking independently in
    // their own tasks.
    let after_ms = Utc::now().timestamp_millis();
    for &idx in &due {
        match compute_next_run_at_ms(&armed[idx].job.schedule, after_ms) {
            Ok(next) => {
                armed[idx].next_run_at_ms = next;
                state.set_next_run(&armed[idx].job.id, Some(next));
            }
            Err(err) => {
                tracing::warn!(
                    "[scheduler] Re-arm failed for job {}: {err:#}; pushing 1h out",
                    armed[idx].job.id
                );
                armed[idx].next_run_at_ms = after_ms + 3_600_000;
                state.set_next_run(&armed[idx].job.id, Some(after_ms + 3_600_000));
            }
        }
    }
}

/// Fire one job's runner and write its output file (for both ok and error runs).
async fn execute_job(
    config: &SchedulerConfig,
    services: &ServiceRegistry,
    state: &SchedulerState,
    job: &ScheduleJob,
) {
    let fired_at = Utc::now();
    state.mark_running(&job.id, fired_at.timestamp_millis());
    let schedule = format_schedule(&job.schedule);
    let name = if job.name.is_empty() {
        &job.id
    } else {
        &job.name
    };

    tracing::info!("[scheduler] Firing job {} ({})", job.id, schedule);

    // Run cwd is the agent's cron dir — contained under the agent root.
    let workspace = config.root.join("cron");
    if let Err(err) = std::fs::create_dir_all(&workspace) {
        tracing::error!(
            "[scheduler] Cannot create cron dir for job {}: {err:#}",
            job.id
        );
        state.mark_finished(&job.id, LastStatus::Error);
        return;
    }

    let timeout = config.timeout_for(job);
    let outcome = fire_runner(
        services,
        &config.runner_kind,
        &config.runner_command,
        &workspace,
        &config.root,
        &config.root,
        job.payload.message.clone(),
        &job.id,
        config.max_run_timeout_ms,
        timeout,
    )
    .await;

    let finished_at = Utc::now();
    let (status, response, error) = match outcome {
        Ok(run) if run.timed_out => (
            RunStatus::Error,
            None,
            Some(format!(
                "run exceeded the {}ms timeout and was killed",
                timeout.as_millis()
            )),
        ),
        Ok(run) => match run.exit {
            ExitKind::Normal => (RunStatus::Ok, Some(run.response), None),
            ExitKind::Abnormal(code) => (
                RunStatus::Error,
                None,
                Some(format!("runner exited abnormally (code {code:?})")),
            ),
            ExitKind::Interrupted { reason } => (
                RunStatus::Error,
                None,
                Some(format!("runner interrupted: {reason}")),
            ),
        },
        Err(err) => (RunStatus::Error, None, Some(format!("{err:#}"))),
    };

    let written = write_cron_run_output(&CronRunOutput {
        root: &config.root,
        job_id: &job.id,
        name,
        prompt: &job.payload.message,
        schedule: &schedule,
        fired_at,
        finished_at,
        status,
        response,
        error,
    });

    let final_status = match status {
        RunStatus::Ok => LastStatus::Ok,
        RunStatus::Error => LastStatus::Error,
    };
    state.mark_finished(&job.id, final_status);

    match written {
        Ok(path) => tracing::info!("[scheduler] Wrote output {}", path.display()),
        Err(err) => tracing::error!("[scheduler] Failed to write output for {}: {err:#}", job.id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{jobs_path, Payload, Schedule};
    use cap_runner::{KillReason, Runner, RunnerHandle, SpawnParams};
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration as StdDuration;

    /// In-test runner whose child sleeps `run_for` before exiting normally, and
    /// honors the kill channel (resolving as interrupted). Counts spawns so a
    /// test can assert how many fires actually reached the runner.
    struct SleepyRunner {
        run_for: StdDuration,
        spawns: Arc<AtomicUsize>,
    }

    impl Runner for SleepyRunner {
        fn spawn<'a>(
            &self,
            _params: SpawnParams<'a>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<RunnerHandle>> + Send + 'a>,
        > {
            let run_for = self.run_for;
            self.spawns.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                let (kill_tx, kill_rx) = tokio::sync::oneshot::channel::<KillReason>();
                let done = tokio::spawn(async move {
                    tokio::select! {
                        _ = tokio::time::sleep(run_for) => ExitKind::Normal,
                        _ = kill_rx => ExitKind::Interrupted { reason: "killed" },
                    }
                });
                Ok(RunnerHandle::new(0, kill_tx, done))
            })
        }
    }

    fn services_with(run_for: StdDuration) -> (ServiceRegistry, Arc<AtomicUsize>) {
        let spawns = Arc::new(AtomicUsize::new(0));
        let mut services = ServiceRegistry::default();
        services
            .service::<dyn Runner>(
                "fake",
                Arc::new(SleepyRunner {
                    run_for,
                    spawns: Arc::clone(&spawns),
                }),
            )
            .unwrap();
        (services, spawns)
    }

    fn test_config(root: PathBuf, job_timeout_ms: u64) -> SchedulerConfig {
        SchedulerConfig {
            root,
            runner_kind: "fake".to_string(),
            runner_command: String::new(),
            max_run_timeout_ms: 3_600_000,
            poll_interval_ms: 2_000,
            job_timeout_ms,
        }
    }

    fn armed(id: &str, timeout_ms: Option<u64>) -> ArmedJob {
        ArmedJob {
            job: job_with(id, timeout_ms, true, "hi"),
            // Due now.
            next_run_at_ms: Utc::now().timestamp_millis() - 1,
            running: Arc::new(AtomicBool::new(false)),
            last_skipped_at_ms: None,
        }
    }

    fn job_with(id: &str, timeout_ms: Option<u64>, enabled: bool, msg: &str) -> ScheduleJob {
        ScheduleJob {
            id: id.to_string(),
            name: String::new(),
            enabled,
            schedule: Schedule {
                cron: "* * * * *".to_string(),
                tz: "UTC".to_string(),
                start_at: None,
            },
            payload: Payload {
                message: msg.to_string(),
            },
            timeout_ms,
        }
    }

    fn latest_output(root: &std::path::Path, job_id: &str) -> Option<String> {
        let dir = root.join("cron").join("output").join(job_id);
        let mut entries: Vec<_> = std::fs::read_dir(&dir).ok()?.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        let last = entries.last()?;
        std::fs::read_to_string(last.path()).ok()
    }

    fn write_jobs(root: &std::path::Path, body: &str) {
        std::fs::create_dir_all(root.join("cron")).unwrap();
        std::fs::write(jobs_path(root), body).unwrap();
    }

    const ONE_JOB: &str = r#"{ "jobs": [
        { "id": "a", "schedule": { "cron": "0 0 1 1 *", "tz": "UTC" }, "payload": { "message": "x" } }
    ] }"#;

    const TWO_JOBS: &str = r#"{ "jobs": [
        { "id": "a", "schedule": { "cron": "0 0 1 1 *", "tz": "UTC" }, "payload": { "message": "x" } },
        { "id": "b", "schedule": { "cron": "0 0 1 1 *", "tz": "UTC" }, "payload": { "message": "y" } }
    ] }"#;

    #[test]
    fn per_job_timeout_overrides_extension_default() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path().to_path_buf(), 5_000);
        let with_override = armed("j", Some(250)).job;
        let without = armed("j", None).job;
        assert_eq!(config.timeout_for(&with_override), Duration::from_millis(250));
        assert_eq!(config.timeout_for(&without), Duration::from_millis(5_000));
    }

    #[tokio::test]
    async fn overlapping_fire_is_skipped_and_bookmarked() {
        let dir = tempfile::tempdir().unwrap();
        // Run takes longer than the gap between the two ticks below.
        let (services, spawns) = services_with(StdDuration::from_millis(400));
        let config = test_config(dir.path().to_path_buf(), 60_000);
        let mut jobs = vec![armed("job", None)];
        let state = Arc::new(SchedulerState::new(vec![]));

        // First tick fires the job (now running). The guard is set synchronously
        // inside `run_due_jobs`; the actual runner spawn happens in the spawned
        // task, so give it a beat to reach `runner.spawn`.
        run_due_jobs(&config, &services, &state, &mut jobs).await;
        assert!(jobs[0].running.load(Ordering::SeqCst));
        tokio::time::sleep(StdDuration::from_millis(50)).await;
        assert_eq!(spawns.load(Ordering::SeqCst), 1);
        assert!(jobs[0].last_skipped_at_ms.is_none());

        // Force the job due again while the first run is still in flight.
        jobs[0].next_run_at_ms = Utc::now().timestamp_millis() - 1;
        run_due_jobs(&config, &services, &state, &mut jobs).await;

        // Overlap-skip: no new spawn, skip is bookmarked, next run recomputed.
        assert_eq!(spawns.load(Ordering::SeqCst), 1, "second fire skipped");
        assert!(jobs[0].last_skipped_at_ms.is_some(), "skip bookmarked");
        assert!(
            jobs[0].next_run_at_ms > Utc::now().timestamp_millis(),
            "next run recomputed forward"
        );

        // Let the in-flight run finish so the guard clears.
        tokio::time::sleep(StdDuration::from_millis(600)).await;
        assert!(!jobs[0].running.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn timed_out_run_is_killed_and_recorded_as_error() {
        let dir = tempfile::tempdir().unwrap();
        // Child would run for 10s, but the job timeout is 150ms.
        let (services, _spawns) = services_with(StdDuration::from_secs(10));
        let config = test_config(dir.path().to_path_buf(), 150);
        let job = armed("timeout-job", None).job;
        let state = Arc::new(SchedulerState::new(vec![]));

        execute_job(&config, &services, &state, &job).await;

        let out = latest_output(dir.path(), "timeout-job").expect("output written");
        assert!(out.contains("status: error"), "recorded as error: {out}");
        assert!(out.contains("timeout"), "timeout message present: {out}");
    }

    /// Runner whose spawn future panics, to prove the overlap guard is released
    /// even when a fire task unwinds.
    struct PanickyRunner;
    impl Runner for PanickyRunner {
        fn spawn<'a>(
            &self,
            _params: SpawnParams<'a>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<RunnerHandle>> + Send + 'a>,
        > {
            Box::pin(async move { panic!("boom in runner") })
        }
    }

    #[tokio::test]
    async fn panicking_fire_releases_overlap_guard() {
        let dir = tempfile::tempdir().unwrap();
        let mut services = ServiceRegistry::default();
        services
            .service::<dyn Runner>("fake", Arc::new(PanickyRunner))
            .unwrap();
        let config = test_config(dir.path().to_path_buf(), 60_000);
        let mut jobs = vec![armed("job", None)];
        let state = Arc::new(SchedulerState::new(vec![]));

        run_due_jobs(&config, &services, &state, &mut jobs).await;
        // The fire task panics; the drop guard must still clear `running`.
        tokio::time::sleep(StdDuration::from_millis(100)).await;
        assert!(
            !jobs[0].running.load(Ordering::SeqCst),
            "overlap guard released after a panicking fire"
        );
    }

    #[tokio::test]
    async fn rearm_advances_next_run_even_when_run_spawns() {
        let dir = tempfile::tempdir().unwrap();
        let (services, _spawns) = services_with(StdDuration::from_millis(50));
        let config = test_config(dir.path().to_path_buf(), 60_000);
        let mut jobs = vec![armed("job", None)];
        let state = Arc::new(SchedulerState::new(vec![]));
        let before = jobs[0].next_run_at_ms;
        run_due_jobs(&config, &services, &state, &mut jobs).await;
        assert!(jobs[0].next_run_at_ms > before, "timer re-armed forward");
    }

    // --- Hot reload (ALG-223) ---------------------------------------------

    #[test]
    fn arm_jobs_loads_enabled_jobs_from_state() {
        let state = SchedulerState::new(vec![job_with("a", None, true, "x"), job_with("b", None, true, "y")]);
        let armed = arm_jobs(&state, &mut HashMap::new());
        assert_eq!(armed.len(), 2);
    }

    #[test]
    fn fingerprint_changes_when_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path().to_path_buf(), 60_000);
        assert!(fingerprint(&config).is_none(), "absent file → None");
        write_jobs(dir.path(), ONE_JOB);
        let fp1 = fingerprint(&config).expect("file present");
        std::thread::sleep(Duration::from_millis(10));
        write_jobs(dir.path(), TWO_JOBS);
        let fp2 = fingerprint(&config).expect("file present");
        assert!(fp1 != fp2, "fingerprint changes on edit");
    }

    #[test]
    fn reload_picks_up_added_job() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path().to_path_buf(), 60_000);
        write_jobs(dir.path(), ONE_JOB);
        let state = SchedulerState::new(load_jobs(dir.path(), |_| {}));
        let armed = arm_jobs(&state, &mut HashMap::new());
        let mut fp = fingerprint(&config);
        assert_eq!(armed.len(), 1);

        // Sleep past mtime granularity, then add a second job.
        std::thread::sleep(Duration::from_millis(10));
        write_jobs(dir.path(), TWO_JOBS);
        maybe_reload(&config, &state, &mut fp);
        assert_eq!(state.jobs().len(), 2);
    }

    #[test]
    fn reload_drops_removed_job() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path().to_path_buf(), 60_000);
        write_jobs(dir.path(), TWO_JOBS);
        let state = SchedulerState::new(load_jobs(dir.path(), |_| {}));
        let armed = arm_jobs(&state, &mut HashMap::new());
        let mut fp = fingerprint(&config);
        assert_eq!(armed.len(), 2);

        std::thread::sleep(Duration::from_millis(10));
        write_jobs(dir.path(), ONE_JOB);
        maybe_reload(&config, &state, &mut fp);
        let jobs = state.jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, "a");
    }

    #[test]
    fn reload_preserves_running_guard_for_surviving_job() {
        // A reload re-arms via `arm_jobs(state, guards_of(armed))`; the guard map
        // carries the in-flight overlap guard forward for a surviving job even
        // when its definition is edited.
        let state = SchedulerState::new(vec![job_with("a", None, true, "x")]);
        let armed = arm_jobs(&state, &mut HashMap::new());

        // Mark job "a" as running and keep a handle to its guard.
        armed[0].running.store(true, Ordering::Release);
        let guard = Arc::clone(&armed[0].running);

        // Edit "a"'s message in state, then re-arm preserving guards.
        state.set_jobs(vec![job_with("a", None, true, "edited")]);
        let rearmed = arm_jobs(&state, &mut guards_of(&armed));

        assert_eq!(rearmed.len(), 1);
        assert!(rearmed[0].running.load(Ordering::Acquire));
        assert!(Arc::ptr_eq(&guard, &rearmed[0].running));
        assert_eq!(rearmed[0].job.payload.message, "edited");
    }

    #[test]
    fn reload_dropping_running_job_does_not_orphan_track() {
        let state = SchedulerState::new(vec![
            job_with("a", None, true, "x"),
            job_with("b", None, true, "y"),
        ]);
        let armed = arm_jobs(&state, &mut HashMap::new());

        // Job "b" is running; its guard handle is held by an "in-flight task".
        let b_idx = armed.iter().position(|a| a.job.id == "b").unwrap();
        armed[b_idx].running.store(true, Ordering::Release);
        let inflight_guard = Arc::clone(&armed[b_idx].running);

        // Remove "b" and re-arm preserving guards.
        state.set_jobs(vec![job_with("a", None, true, "x")]);
        let rearmed = arm_jobs(&state, &mut guards_of(&armed));

        // "b" is gone from the armed set, but the in-flight task's guard is
        // untouched (it owns its own Arc) — no double tracking.
        assert_eq!(rearmed.len(), 1);
        assert_eq!(rearmed[0].job.id, "a");
        assert!(inflight_guard.load(Ordering::Acquire));
        // The in-flight task can still release its own guard on completion.
        inflight_guard.store(false, Ordering::Release);
        assert!(!inflight_guard.load(Ordering::Acquire));
    }

    #[test]
    fn malformed_reload_is_treated_as_empty_and_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path().to_path_buf(), 60_000);
        write_jobs(dir.path(), ONE_JOB);
        let state = SchedulerState::new(load_jobs(dir.path(), |_| {}));
        let armed = arm_jobs(&state, &mut HashMap::new());
        let mut fp = fingerprint(&config);
        assert_eq!(armed.len(), 1);

        // Malformed write → empty, no panic.
        std::thread::sleep(Duration::from_millis(10));
        write_jobs(dir.path(), "not json");
        maybe_reload(&config, &state, &mut fp);
        assert!(state.jobs().is_empty());

        // Next valid write recovers.
        std::thread::sleep(Duration::from_millis(10));
        write_jobs(dir.path(), TWO_JOBS);
        maybe_reload(&config, &state, &mut fp);
        assert_eq!(state.jobs().len(), 2);
    }

    #[test]
    fn no_change_does_not_reload() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path().to_path_buf(), 60_000);
        write_jobs(dir.path(), ONE_JOB);
        let state = SchedulerState::new(load_jobs(dir.path(), |_| {}));
        let mut fp = fingerprint(&config);
        let before = fp.clone();
        // Clear state to prove `maybe_reload` does NOT repopulate it when the
        // fingerprint is unchanged.
        state.set_jobs(vec![]);
        maybe_reload(&config, &state, &mut fp);
        assert!(before == fp);
        assert!(state.jobs().is_empty(), "no reload when fingerprint unchanged");
    }

    #[test]
    fn per_job_disabled_is_live_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path().to_path_buf(), 60_000);
        write_jobs(dir.path(), ONE_JOB);
        let state = SchedulerState::new(load_jobs(dir.path(), |_| {}));
        let armed = arm_jobs(&state, &mut HashMap::new());
        let mut fp = fingerprint(&config);
        assert_eq!(armed.len(), 1);

        // Disable the job in-place; reload pushes it (disabled) into state and a
        // subsequent arm drops it from the armed set.
        std::thread::sleep(Duration::from_millis(10));
        write_jobs(
            dir.path(),
            r#"{ "jobs": [
                { "id": "a", "enabled": false, "schedule": { "cron": "0 0 1 1 *", "tz": "UTC" }, "payload": { "message": "x" } }
            ] }"#,
        );
        maybe_reload(&config, &state, &mut fp);
        let rearmed = arm_jobs(&state, &mut HashMap::new());
        assert!(rearmed.is_empty(), "disabled job is not armed");
    }
}
