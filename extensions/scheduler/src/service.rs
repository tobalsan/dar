//! Scheduler runtime: load jobs, compute next fires, arm a single timer for the
//! earliest, and on each tick fire all due jobs concurrently before re-arming.
//!
//! Hot reload (ALG-223): a short poll interval re-reads `cron/jobs.json` and,
//! when the file changes, reconciles the in-memory armed set — adding new jobs,
//! dropping removed ones, and re-arming changed schedules — without restarting
//! the host. The reconcile preserves per-job execution guards: a job that is
//! mid-run when its definition is edited keeps its overlap guard, and a job
//! deleted while running is not orphan-tracked twice (its in-flight run owns its
//! own guard handle and completes normally).
//!
//! Still out of scope here and tracked separately: timeout guards (ALG-221),
//! HTTP CRUD (ALG-222), dashboard tab (ALG-225). A minimal overlap-skip guard
//! lives here because the reload contract requires it to survive a reload.

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
use crate::store::{jobs_path, load_jobs, ScheduleJob};

/// Floor so a misconfigured tiny interval cannot spin the loop.
const MIN_POLL_INTERVAL_MS: u64 = 250;

/// Static configuration the runtime needs to fire runs, resolved once at start
/// from `agent.yaml`.
#[derive(Clone)]
pub struct SchedulerConfig {
    pub root: PathBuf,
    pub runner_kind: String,
    pub runner_command: String,
    pub max_run_timeout_ms: u64,
    pub poll_interval_ms: u64,
}

/// A loaded job paired with its computed next-fire instant (UTC epoch millis)
/// and a shared overlap guard. The guard is `true` while a run for this job is
/// in flight; it is cloned into the spawned run task so it survives a reload
/// that drops or replaces the job entry.
struct ArmedJob {
    job: ScheduleJob,
    next_run_at_ms: i64,
    running: Arc<AtomicBool>,
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

/// Run the scheduler loop until shutdown. Loads jobs at boot, then polls
/// `cron/jobs.json` on `poll_interval_ms`; when the file changes the armed set
/// is reconciled and the timer re-armed. Each loop iteration waits on the
/// earliest of: shutdown, the next poll tick, and the next fire.
pub async fn run(config: SchedulerConfig, services: ServiceRegistry, mut shutdown: ShutdownToken) {
    let mut armed = arm_jobs(&config, &mut HashMap::new());
    let mut fingerprint_seen = fingerprint(&config);
    if armed.is_empty() {
        tracing::info!("[scheduler] No enabled jobs; idle (polling for edits)");
    } else {
        tracing::info!("[scheduler] Started with {} job(s)", armed.len());
    }

    let poll = Duration::from_millis(config.poll_interval_ms.max(MIN_POLL_INTERVAL_MS));

    loop {
        // Earliest fire across armed jobs, if any.
        let next_fire = armed.iter().map(|a| a.next_run_at_ms).min();
        let fire_delay = next_fire
            .map(next_delay)
            .unwrap_or_else(|| Duration::from_secs(3_600));

        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(poll) => {
                maybe_reload(&config, &mut armed, &mut fingerprint_seen);
            }
            _ = tokio::time::sleep(fire_delay), if next_fire.is_some() => {
                run_due_jobs(&config, &services, &mut armed).await;
            }
        }
    }
}

/// If `cron/jobs.json` changed since last seen, reload and reconcile the armed
/// set, preserving in-flight guards for surviving jobs.
fn maybe_reload(
    config: &SchedulerConfig,
    armed: &mut Vec<ArmedJob>,
    fingerprint_seen: &mut Option<FileFingerprint>,
) {
    let current = fingerprint(config);
    if current == *fingerprint_seen {
        return;
    }
    *fingerprint_seen = current;

    // Preserve the running guards of currently-armed jobs, keyed by id, so a
    // reload that keeps a job (even with an edited schedule) keeps its guard.
    let mut guards: HashMap<String, Arc<AtomicBool>> = armed
        .iter()
        .map(|a| (a.job.id.clone(), Arc::clone(&a.running)))
        .collect();

    let reloaded = arm_jobs(config, &mut guards);
    tracing::info!(
        "[scheduler] Reloaded cron/jobs.json: {} enabled job(s)",
        reloaded.len()
    );
    *armed = reloaded;
}

/// Load jobs from disk and compute each enabled job's next fire. Jobs whose
/// schedule fails to parse are skipped with a warning. `guards` carries forward
/// the overlap guard for any job id present from a prior load; jobs new to this
/// load get a fresh, idle guard.
fn arm_jobs(config: &SchedulerConfig, guards: &mut HashMap<String, Arc<AtomicBool>>) -> Vec<ArmedJob> {
    let now_ms = Utc::now().timestamp_millis();
    let jobs = load_jobs(&config.root, |m| tracing::warn!("{m}"));
    let mut armed = Vec::new();
    for job in jobs {
        if !job.enabled {
            continue;
        }
        match compute_next_run_at_ms(&job.schedule, now_ms) {
            Ok(next_run_at_ms) => {
                let running = guards
                    .remove(&job.id)
                    .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
                armed.push(ArmedJob {
                    job,
                    next_run_at_ms,
                    running,
                });
            }
            Err(err) => {
                tracing::warn!("[scheduler] Skipping job {}: bad schedule: {err:#}", job.id)
            }
        }
    }
    armed
}

/// Milliseconds until `next_at_ms`, clamped to zero.
fn next_delay(next_at_ms: i64) -> Duration {
    let now = Utc::now().timestamp_millis();
    Duration::from_millis((next_at_ms - now).max(0) as u64)
}

/// Fire every job whose next-fire instant is now due, concurrently, then
/// recompute each fired job's next fire so the timer re-arms forward. A job
/// already running (overlap guard set) is skipped, not fired again, and its
/// next fire is still advanced so the timer does not busy-loop on it.
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
    let mut handles = Vec::new();
    for &idx in &due {
        let running = &armed[idx].running;
        // Overlap-skip: if a run is already in flight for this job, do not start
        // another. `compare_exchange` claims the guard atomically.
        if running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            tracing::info!(
                "[scheduler] Skipping job {}: previous run still in flight",
                armed[idx].job.id
            );
            continue;
        }
        let job = armed[idx].job.clone();
        let running = Arc::clone(&armed[idx].running);
        let config = Arc::clone(&config);
        let services = services.clone();
        handles.push(tokio::spawn(async move {
            execute_job(&config, &services, &job).await;
            // Release the guard once the run completes. The guard handle is
            // owned by this task, so it survives a reload that drops the job
            // from the armed set — the run is never orphan-tracked twice.
            running.store(false, Ordering::Release);
        }));
    }
    // NOTE: awaiting inline means a long-running job blocks the whole loop
    // (including polling) until it returns; the non-blocking/timeout behaviour
    // is a later slice (ALG-221). The overlap guard above still protects the
    // cross-tick and reload-survival cases this slice requires.
    for handle in handles {
        let _ = handle.await;
    }

    // Re-arm fired (or skipped-but-due) jobs forward from now so the next tick
    // lands on the following occurrence.
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
    )
    .await;

    let finished_at = Utc::now();
    let (status, response, error) = match outcome {
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

    fn config(root: PathBuf) -> SchedulerConfig {
        SchedulerConfig {
            root,
            runner_kind: "pi".to_string(),
            runner_command: String::new(),
            max_run_timeout_ms: 1000,
            poll_interval_ms: 2_000,
        }
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
    fn arm_jobs_loads_enabled_jobs() {
        let dir = tempfile::tempdir().unwrap();
        write_jobs(dir.path(), TWO_JOBS);
        let armed = arm_jobs(&config(dir.path().to_path_buf()), &mut HashMap::new());
        assert_eq!(armed.len(), 2);
    }

    #[test]
    fn reload_picks_up_added_job() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path().to_path_buf());
        write_jobs(dir.path(), ONE_JOB);
        let mut armed = arm_jobs(&config, &mut HashMap::new());
        let mut fp = fingerprint(&config);
        assert_eq!(armed.len(), 1);

        // Sleep past mtime granularity, then add a second job.
        std::thread::sleep(Duration::from_millis(10));
        write_jobs(dir.path(), TWO_JOBS);
        maybe_reload(&config, &mut armed, &mut fp);
        assert_eq!(armed.len(), 2);
    }

    #[test]
    fn reload_drops_removed_job() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path().to_path_buf());
        write_jobs(dir.path(), TWO_JOBS);
        let mut armed = arm_jobs(&config, &mut HashMap::new());
        let mut fp = fingerprint(&config);
        assert_eq!(armed.len(), 2);

        std::thread::sleep(Duration::from_millis(10));
        write_jobs(dir.path(), ONE_JOB);
        maybe_reload(&config, &mut armed, &mut fp);
        assert_eq!(armed.len(), 1);
        assert_eq!(armed[0].job.id, "a");
    }

    #[test]
    fn reload_preserves_running_guard_for_surviving_job() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path().to_path_buf());
        write_jobs(dir.path(), ONE_JOB);
        let mut armed = arm_jobs(&config, &mut HashMap::new());
        let mut fp = fingerprint(&config);

        // Mark job "a" as running and keep a handle to its guard.
        armed[0].running.store(true, Ordering::Release);
        let guard = Arc::clone(&armed[0].running);

        // Edit the file (change "a"'s message) and reload.
        std::thread::sleep(Duration::from_millis(10));
        write_jobs(
            dir.path(),
            r#"{ "jobs": [
                { "id": "a", "schedule": { "cron": "0 0 1 1 *", "tz": "UTC" }, "payload": { "message": "edited" } }
            ] }"#,
        );
        maybe_reload(&config, &mut armed, &mut fp);

        // Same logical guard survives, still flagged running.
        assert_eq!(armed.len(), 1);
        assert!(armed[0].running.load(Ordering::Acquire));
        assert!(Arc::ptr_eq(&guard, &armed[0].running));
        assert_eq!(armed[0].job.payload.message, "edited");
    }

    #[test]
    fn reload_dropping_running_job_does_not_orphan_track() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path().to_path_buf());
        write_jobs(dir.path(), TWO_JOBS);
        let mut armed = arm_jobs(&config, &mut HashMap::new());
        let mut fp = fingerprint(&config);

        // Job "b" is running; its guard handle is held by an "in-flight task".
        let b_idx = armed.iter().position(|a| a.job.id == "b").unwrap();
        armed[b_idx].running.store(true, Ordering::Release);
        let inflight_guard = Arc::clone(&armed[b_idx].running);

        // Remove "b" from the file and reload.
        std::thread::sleep(Duration::from_millis(10));
        write_jobs(dir.path(), ONE_JOB);
        maybe_reload(&config, &mut armed, &mut fp);

        // "b" is gone from the armed set, but the in-flight task's guard is
        // untouched (it owns its own Arc) — no double tracking.
        assert_eq!(armed.len(), 1);
        assert_eq!(armed[0].job.id, "a");
        assert!(inflight_guard.load(Ordering::Acquire));
        // The in-flight task can still release its own guard on completion.
        inflight_guard.store(false, Ordering::Release);
        assert!(!inflight_guard.load(Ordering::Acquire));
    }

    #[test]
    fn malformed_reload_is_treated_as_empty_and_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path().to_path_buf());
        write_jobs(dir.path(), ONE_JOB);
        let mut armed = arm_jobs(&config, &mut HashMap::new());
        let mut fp = fingerprint(&config);
        assert_eq!(armed.len(), 1);

        // Malformed write → empty, no panic.
        std::thread::sleep(Duration::from_millis(10));
        write_jobs(dir.path(), "not json");
        maybe_reload(&config, &mut armed, &mut fp);
        assert!(armed.is_empty());

        // Next valid write recovers.
        std::thread::sleep(Duration::from_millis(10));
        write_jobs(dir.path(), TWO_JOBS);
        maybe_reload(&config, &mut armed, &mut fp);
        assert_eq!(armed.len(), 2);
    }

    #[test]
    fn no_change_does_not_reload() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path().to_path_buf());
        write_jobs(dir.path(), ONE_JOB);
        let mut armed = arm_jobs(&config, &mut HashMap::new());
        let mut fp = fingerprint(&config);
        let before = fp.clone();
        maybe_reload(&config, &mut armed, &mut fp);
        // Fingerprint unchanged → still the same.
        assert!(before == fp);
        assert_eq!(armed.len(), 1);
    }

    #[test]
    fn per_job_disabled_is_live_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path().to_path_buf());
        write_jobs(dir.path(), ONE_JOB);
        let mut armed = arm_jobs(&config, &mut HashMap::new());
        let mut fp = fingerprint(&config);
        assert_eq!(armed.len(), 1);

        // Disable the job in-place; reload drops it from the armed set.
        std::thread::sleep(Duration::from_millis(10));
        write_jobs(
            dir.path(),
            r#"{ "jobs": [
                { "id": "a", "enabled": false, "schedule": { "cron": "0 0 1 1 *", "tz": "UTC" }, "payload": { "message": "x" } }
            ] }"#,
        );
        maybe_reload(&config, &mut armed, &mut fp);
        assert!(armed.is_empty());
    }
}
