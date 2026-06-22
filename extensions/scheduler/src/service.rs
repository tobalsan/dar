//! Scheduler runtime: load jobs, compute next fires, arm a single timer for the
//! earliest, and on each tick fire all due jobs concurrently before re-arming.
//!
//! This is the walking skeleton (ALG-219). Out of scope here and tracked
//! separately: overlap-skip / timeout guards (ALG-221), HTTP CRUD (ALG-222),
//! hot reload (ALG-223), dashboard tab (ALG-225).

use std::path::PathBuf;
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
}

/// A loaded job paired with its computed next-fire instant (UTC epoch millis).
struct ArmedJob {
    job: ScheduleJob,
    next_run_at_ms: i64,
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
            }),
            Err(err) => {
                tracing::warn!("[scheduler] Skipping job {}: bad schedule: {err:#}", job.id)
            }
        }
    }
    armed
}

/// Fire every job whose next-fire instant is now due, concurrently, then
/// recompute each fired job's next fire so the timer re-arms forward.
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
        let job = armed[idx].job.clone();
        let config = Arc::clone(&config);
        let services = services.clone();
        handles.push(tokio::spawn(async move {
            execute_job(&config, &services, &job).await;
        }));
    }
    for handle in handles {
        let _ = handle.await;
    }

    // Re-arm fired jobs forward from now so the next tick lands on the
    // following occurrence.
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
