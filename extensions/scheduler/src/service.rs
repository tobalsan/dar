//! Scheduler runtime: load jobs, compute next fires, arm a single timer for the
//! earliest, and on each tick fire all due jobs concurrently before re-arming.
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
//! Tracked separately: HTTP CRUD (ALG-222), hot reload (ALG-223), dashboard tab
//! (ALG-225).

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
use crate::store::{load_jobs, ScheduleJob};

/// Static configuration the runtime needs to fire runs, resolved once at start
/// from `agent.yaml`.
#[derive(Clone)]
pub struct SchedulerConfig {
    pub root: PathBuf,
    pub runner_kind: String,
    pub runner_command: String,
    pub max_run_timeout_ms: u64,
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
    /// run task, which clears it on completion.
    running: Arc<AtomicBool>,
    /// UTC epoch millis of the last skipped fire (overlap-skip bookmark), or
    /// `None` if none has been skipped. Run-now logic (a later slice) reads
    /// this to distinguish a skip from a normal completion.
    last_skipped_at_ms: Option<i64>,
}

/// Run the scheduler loop until shutdown. Loads jobs once at boot (hot reload is
/// a later slice), then repeatedly arms the earliest fire and ticks.
pub async fn run(config: SchedulerConfig, services: ServiceRegistry, mut shutdown: ShutdownToken) {
    let mut armed = arm_jobs(&config);
    if armed.is_empty() {
        tracing::info!("[scheduler] No enabled jobs; idle");
    } else {
        tracing::info!("[scheduler] Started with {} job(s)", armed.len());
    }

    loop {
        let Some(next_at_ms) = armed.iter().map(|a| a.next_run_at_ms).min() else {
            // No armed jobs: wait for shutdown only.
            shutdown.cancelled().await;
            return;
        };
        let delay = next_delay(next_at_ms);

        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(delay) => {
                run_due_jobs(&config, &services, &mut armed).await;
            }
        }
    }
}

/// Milliseconds until `next_at_ms`, clamped to zero.
fn next_delay(next_at_ms: i64) -> Duration {
    let now = Utc::now().timestamp_millis();
    Duration::from_millis((next_at_ms - now).max(0) as u64)
}

/// Load jobs from disk and compute each enabled job's next fire. Jobs whose
/// schedule fails to parse are skipped with a warning.
fn arm_jobs(config: &SchedulerConfig) -> Vec<ArmedJob> {
    let now_ms = Utc::now().timestamp_millis();
    let jobs = load_jobs(&config.root, |m| tracing::warn!("{m}"));
    let mut armed = Vec::new();
    for job in jobs {
        if !job.enabled {
            continue;
        }
        match compute_next_run_at_ms(&job.schedule, now_ms) {
            Ok(next_run_at_ms) => armed.push(ArmedJob {
                job,
                next_run_at_ms,
                running: Arc::new(AtomicBool::new(false)),
                last_skipped_at_ms: None,
            }),
            Err(err) => {
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
        let guard = RunningGuard(Arc::clone(&armed[idx].running));
        tokio::spawn(async move {
            let _guard = guard;
            execute_job(&config, &services, &job).await;
        });
    }

    // Re-arm every due job forward from now so the next tick lands on the
    // following occurrence. Runs in flight from this or a prior tick keep
    // ticking independently in their own tasks.
    let after_ms = Utc::now().timestamp_millis();
    for &idx in &due {
        match compute_next_run_at_ms(&armed[idx].job.schedule, after_ms) {
            Ok(next) => armed[idx].next_run_at_ms = next,
            Err(err) => {
                tracing::warn!(
                    "[scheduler] Re-arm failed for job {}: {err:#}; pushing 1h out",
                    armed[idx].job.id
                );
                armed[idx].next_run_at_ms = after_ms + 3_600_000;
            }
        }
    }
}

/// Fire one job's runner and write its output file (for both ok and error runs).
async fn execute_job(config: &SchedulerConfig, services: &ServiceRegistry, job: &ScheduleJob) {
    let fired_at = Utc::now();
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

    match written {
        Ok(path) => tracing::info!("[scheduler] Wrote output {}", path.display()),
        Err(err) => tracing::error!("[scheduler] Failed to write output for {}: {err:#}", job.id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Payload, Schedule};
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
            job_timeout_ms,
        }
    }

    fn armed(id: &str, timeout_ms: Option<u64>) -> ArmedJob {
        ArmedJob {
            job: ScheduleJob {
                id: id.to_string(),
                name: String::new(),
                enabled: true,
                schedule: Schedule {
                    cron: "* * * * *".to_string(),
                    tz: "UTC".to_string(),
                    start_at: None,
                },
                payload: Payload {
                    message: "hi".to_string(),
                },
                timeout_ms,
            },
            // Due now.
            next_run_at_ms: Utc::now().timestamp_millis() - 1,
            running: Arc::new(AtomicBool::new(false)),
            last_skipped_at_ms: None,
        }
    }

    fn latest_output(root: &std::path::Path, job_id: &str) -> Option<String> {
        let dir = root.join("cron").join("output").join(job_id);
        let mut entries: Vec<_> = std::fs::read_dir(&dir).ok()?.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        let last = entries.last()?;
        std::fs::read_to_string(last.path()).ok()
    }

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

        // First tick fires the job (now running). The guard is set synchronously
        // inside `run_due_jobs`; the actual runner spawn happens in the spawned
        // task, so give it a beat to reach `runner.spawn`.
        run_due_jobs(&config, &services, &mut jobs).await;
        assert!(jobs[0].running.load(Ordering::SeqCst));
        tokio::time::sleep(StdDuration::from_millis(50)).await;
        assert_eq!(spawns.load(Ordering::SeqCst), 1);
        assert!(jobs[0].last_skipped_at_ms.is_none());

        // Force the job due again while the first run is still in flight.
        jobs[0].next_run_at_ms = Utc::now().timestamp_millis() - 1;
        run_due_jobs(&config, &services, &mut jobs).await;

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

        execute_job(&config, &services, &job).await;

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

        run_due_jobs(&config, &services, &mut jobs).await;
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
        let before = jobs[0].next_run_at_ms;
        run_due_jobs(&config, &services, &mut jobs).await;
        assert!(jobs[0].next_run_at_ms > before, "timer re-armed forward");
    }
}
