//! Scheduler runtime: read jobs from shared state, compute next fires, arm a
//! single timer for the earliest, and on each tick fire all due jobs
//! concurrently before re-arming. A create/update/delete over the HTTP API
//! wakes the loop ([`SchedulerState::changed`]) so a sooner schedule takes
//! effect immediately instead of after the current sleep.
//!
//! Out of scope here and tracked separately: overlap-skip / timeout guards
//! (ALG-221), hot reload of file edits (ALG-223), dashboard tab (ALG-225).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use cap_runner::ExitKind;
use chrono::Utc;
use host_api::{ServiceRegistry, ShutdownToken};

use crate::output::{write_cron_run_output, CronRunOutput, RunStatus};
use crate::runner::fire_runner;
use crate::schedule::{compute_next_run_at_ms, format_schedule};
use crate::state::{LastStatus, SchedulerState};
use crate::store::ScheduleJob;

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

/// Run the scheduler loop until shutdown. Jobs are read from shared state, so a
/// create/update/delete over the HTTP API is picked up on the next re-arm. The
/// loop selects on a shutdown signal, a sleep until the earliest fire, and a
/// `changed` notification that re-arms immediately when jobs are mutated.
pub async fn run(
    config: SchedulerConfig,
    services: ServiceRegistry,
    state: Arc<SchedulerState>,
    mut shutdown: ShutdownToken,
) {
    let mut armed = arm_jobs(&state);
    if armed.is_empty() {
        tracing::info!("[scheduler] No enabled jobs; idle");
    } else {
        tracing::info!("[scheduler] Started with {} job(s)", armed.len());
    }

    loop {
        let next = armed.iter().map(|a| a.next_run_at_ms).min();
        match next {
            Some(next_at_ms) => {
                let delay = next_delay(next_at_ms);
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = state.changed() => {
                        // Jobs mutated over HTTP: recompute fires so a sooner
                        // schedule takes effect immediately.
                        armed = arm_jobs(&state);
                    }
                    _ = tokio::time::sleep(delay) => {
                        run_due_jobs(&config, &services, &state, &mut armed).await;
                    }
                }
            }
            None => {
                // No armed jobs: wait for shutdown or a mutation that adds one.
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = state.changed() => {
                        armed = arm_jobs(&state);
                    }
                }
            }
        }
    }
}

/// Milliseconds until `next_at_ms`, clamped to zero.
fn next_delay(next_at_ms: i64) -> Duration {
    let now = Utc::now().timestamp_millis();
    Duration::from_millis((next_at_ms - now).max(0) as u64)
}

/// Read jobs from shared state and compute each enabled job's next fire,
/// publishing the computed `nextRunAt` back into runtime state. Disabled jobs
/// clear their `nextRunAt`. Jobs whose schedule fails to parse are skipped with
/// a warning (defensive: the HTTP API validates schedules before persisting).
fn arm_jobs(state: &SchedulerState) -> Vec<ArmedJob> {
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
                armed.push(ArmedJob {
                    job,
                    next_run_at_ms,
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

/// Fire every job whose next-fire instant is now due, concurrently, then
/// recompute each fired job's next fire so the timer re-arms forward.
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
    let mut handles = Vec::new();
    for &idx in &due {
        let job = armed[idx].job.clone();
        let config = Arc::clone(&config);
        let services = services.clone();
        let state = Arc::clone(state);
        handles.push(tokio::spawn(async move {
            execute_job(&config, &services, &state, &job).await;
        }));
    }
    for handle in handles {
        let _ = handle.await;
    }

    // Re-arm fired jobs forward from now so the next tick lands on the
    // following occurrence, publishing the new `nextRunAt` into runtime state.
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

    let timeout_ms = job.timeout_ms.unwrap_or(config.max_run_timeout_ms);
    let outcome = fire_runner(
        services,
        &config.runner_kind,
        &config.runner_command,
        &workspace,
        &config.root,
        &config.root,
        job.payload.message.clone(),
        &job.id,
        timeout_ms,
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
