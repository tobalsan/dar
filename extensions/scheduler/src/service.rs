//! Scheduler runtime: read jobs from shared state, compute next fires, arm a
//! single timer for the earliest, and on each tick fire all due jobs
//! concurrently before re-arming. A create/update/delete over the HTTP API
//! wakes the loop ([`SchedulerState::changed`]) so a sooner schedule takes
//! effect immediately instead of after the current sleep.
//!
//! Execution guards (ALG-221):
//! - **Overlap-skip:** a scheduled fire at the job's `maxConcurrent`
//!   limit (default 1) is skipped (warning logged), the skip is bookmarked
//!   (`last_skipped_at_ms`) for occurrence bookkeeping, and
//!   the job's next fire is recomputed so the timer re-arms forward.
//! - **Timeout:** every run is bounded by a timeout (per-job `timeoutMs`, else
//!   `extensions.scheduler.jobTimeoutMs`, else a 10-minute default). On timeout
//!   the runner child is killed and the run is recorded as an error.
//! - **Re-arm on every path:** the timer loop recomputes the next fire after
//!   every tick; a fired job runs in its own task so a hung/panicking/erroring
//!   job never wedges the loop, and a `RunningGuard` releases only its own slot on
//!   drop, including when a fire panics.
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
//! The per-job runtime this loop maintains (next/last run, last status + error,
//! running-for) is what the read-only Cron dashboard tab ([`crate::tab`])
//! renders via the shared [`SchedulerState`]. Scheduled runs fire the runner
//! service directly and never reach the orchestrator's run list or `RunSnapshot`.

use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use crate::history::RunRecorder;
use crate::runner::RunOutcome;
use cap_deliver::{DeliverySink, Destination};
use cap_runner::ExitKind;
use chrono::Utc;
use host_api::{assert_contained, ServiceRegistry, ShutdownToken};

use crate::output::{write_cron_run_output, CronRunOutput, RunStatus};
use crate::runner::{fire_runner, FireRunnerRequest};
use crate::schedule::{compute_next_run_at_ms, format_schedule};
use crate::state::{LastStatus, RunCompletion, RunToken, SchedulerState};
use crate::store::{jobs_path, load_jobs_checked, ScheduleJob};

/// Floor so a misconfigured tiny poll interval cannot spin the loop.
const MIN_POLL_INTERVAL_MS: u64 = 250;
/// Fits the smallest supported channel message limits while leaving room for
/// transport-specific metadata.
const MAX_DELIVERY_BYTES: usize = 4_000;
const TRUNCATION_MARKER: &str = "\n[truncated]";

fn delivery_text(output: &ExecutionOutput) -> Option<String> {
    let text = match output.status {
        RunStatus::Ok => output.response.as_deref()?.trim(),
        RunStatus::Error => output
            .error
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .unwrap_or("Scheduler job failed"),
    };
    if text.is_empty() || text == "silent tick" {
        return None;
    }
    // Keep gate stdout in the canonical run record, but deliver only the
    // runner's response when the gate woke an agent.
    let text = text
        .strip_prefix("Gate output:\n")
        .and_then(|text| {
            text.split_once("\n\nAgent response:\n")
                .map(|(_, response)| response)
        })
        .unwrap_or(text);
    let mut text = text.to_string();
    if text.len() > MAX_DELIVERY_BYTES {
        let mut boundary = MAX_DELIVERY_BYTES - TRUNCATION_MARKER.len();
        while !text.is_char_boundary(boundary) {
            boundary -= 1;
        }
        text.truncate(boundary);
        text.push_str(TRUNCATION_MARKER);
    }
    Some(text)
}

async fn deliver_result(
    services: &ServiceRegistry,
    job: &ScheduleJob,
    output: &ExecutionOutput,
) -> Vec<String> {
    let Some(text) = delivery_text(output) else {
        return Vec::new();
    };
    let mut outcomes = Vec::new();
    for target in &job.deliver {
        let destination = Destination {
            channel: target.channel.clone(),
            user: target.user.clone(),
        };
        match services.get::<dyn DeliverySink>(&target.target) {
            Ok(sink) => {
                if let Err(err) = sink.deliver(&destination, &text).await {
                    tracing::warn!("[scheduler] Delivery to {} failed: {err:#}", target.target);
                    outcomes.push(format!("{}: warning: {err:#}", target.target));
                } else {
                    outcomes.push(format!("{}: delivered", target.target));
                }
            }
            Err(err) => {
                tracing::warn!(
                    "[scheduler] Delivery sink {} is unavailable: {err:#}",
                    target.target
                );
                outcomes.push(format!(
                    "{}: warning: sink unavailable ({err:#})",
                    target.target
                ));
            }
        }
    }
    outcomes
}

/// Static configuration the runtime needs to fire runs, resolved once at start
/// from `agent.yaml`.
#[derive(Clone)]
pub struct SchedulerConfig {
    pub root: PathBuf,
    pub workflow_root: PathBuf,
    pub host_http_addr: Arc<std::sync::Mutex<Option<std::net::SocketAddr>>>,
    pub runner_kind: String,
    pub runner_command: String,
    pub runner_model: Option<String>,
    pub runner_provider: Option<String>,
    pub runner_thinking: Option<String>,
    pub system_context: Option<String>,
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

/// Releases one run claim on panic or cancellation. Normal completion already
/// releases the token, so the subsequent drop is idempotent.
struct RunningGuard {
    state: Arc<SchedulerState>,
    job_id: String,
    token: RunToken,
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.state.clear_running(&self.job_id, self.token);
    }
}

/// A loaded job paired with its computed next-fire instant (UTC epoch millis).
///
/// Capacity claims live in shared state, survive edits, and are pruned on delete.
struct ArmedJob {
    job: ScheduleJob,
    next_run_at_ms: i64,
}

/// Fingerprint of `cron/jobs.json` used to detect edits cheaply. Content hash
/// avoids missing same-length edits that land within one filesystem mtime tick.
#[derive(Clone, PartialEq, Eq)]
struct FileFingerprint {
    len: u64,
    hash: u64,
}

fn fingerprint(config: &SchedulerConfig) -> Option<FileFingerprint> {
    let bytes = std::fs::read(jobs_path(&config.root)).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    Some(FileFingerprint {
        len: bytes.len() as u64,
        hash: hasher.finish(),
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
    state.set_shutdown(shutdown.clone());
    let mut armed = arm_jobs(&state);
    let mut fires = tokio::task::JoinSet::new();
    let mut fingerprint_seen = fingerprint(&config);
    if armed.is_empty() {
        tracing::info!("[scheduler] No enabled jobs; idle (polling for edits)");
    } else {
        tracing::info!("[scheduler] Started with {} job(s)", armed.len());
    }

    let poll = Duration::from_millis(config.poll_interval_ms.max(MIN_POLL_INTERVAL_MS));

    loop {
        while let Some(result) = fires.try_join_next() {
            if let Err(err) = result {
                tracing::warn!("[scheduler] Fire task ended unexpectedly: {err}");
            }
        }
        let next = armed.iter().map(|a| a.next_run_at_ms).min();
        let fire_delay = next.map(next_delay);
        tokio::select! {
            _ = shutdown.cancelled() => {
                while let Some(result) = fires.join_next().await {
                    if let Err(err) = result {
                        tracing::warn!("[scheduler] Fire task ended unexpectedly during shutdown: {err}");
                    }
                }
                return;
            }
            _ = state.changed() => {
                // Jobs mutated over HTTP (or pushed by a reload below):
                // recompute fires so a sooner schedule takes effect immediately.
                // In-flight overlap claims live in shared state and survive the
                // re-arm for unchanged/edited jobs (pruned only for removed ones).
                armed = arm_jobs(&state);
            }
            _ = tokio::time::sleep(poll) => {
                maybe_reload(&config, &state, &mut fingerprint_seen);
            }
            _ = sleep_opt(fire_delay), if fire_delay.is_some() => {
                run_due_jobs(&config, &services, &state, &mut armed, &mut fires, &shutdown).await;
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

    let jobs = match load_jobs_checked(&config.root) {
        Ok(jobs) => jobs,
        Err(err) => {
            tracing::warn!("[scheduler] Rejected cron/jobs.json reload: {err}");
            return;
        }
    };
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
/// a warning. In-flight overlap claims are not carried here — they live in
/// shared state and are preserved across re-arms for surviving jobs.
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

/// Fire every job whose next-fire instant is now due. A job at its concurrency
/// limit is skipped (overlap-skip) and bookmarked; the rest fire
/// concurrently in their own tasks. Every due job's next fire is recomputed so
/// the timer always re-arms forward, even for skipped, hung, or erroring jobs.
async fn run_due_jobs(
    config: &SchedulerConfig,
    services: &ServiceRegistry,
    state: &Arc<SchedulerState>,
    armed: &mut [ArmedJob],
    fires: &mut tokio::task::JoinSet<()>,
    shutdown: &ShutdownToken,
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
        let job_id = armed[idx].job.id.clone();
        // Scheduled fires and Run now share one atomic capacity claim.
        let Some(token) =
            state.try_claim_running(&job_id, now_ms, armed[idx].job.max_concurrent.unwrap_or(1))
        else {
            state.mark_skipped(&job_id, now_ms);
            tracing::warn!(
                "[scheduler] Skipping fire of job {job_id}: concurrent run limit reached"
            );
            continue;
        };

        // Spawn the fire in its own task so a hung, panicking, or erroring run
        // cannot block the schedule loop. A `RunningGuard` releases the shared
        // claim on drop so it is freed even if the task panics (otherwise that
        // job would be overlap-skipped forever).
        let job = armed[idx].job.clone();
        let config = Arc::clone(&config);
        let services = services.clone();
        let state = Arc::clone(state);
        let guard = RunningGuard {
            state: Arc::clone(&state),
            job_id,
            token,
        };
        let shutdown = shutdown.clone();
        fires.spawn(async move {
            let _guard = guard;
            let _ = execute_job(&config, &services, &state, &job, Some(token), shutdown).await;
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

/// Outcome of a single job fire, surfaced to the run-now HTTP handler. Scheduled
/// fires ignore the return value.
pub struct ExecuteResult {
    pub status: RunStatus,
    pub fired_at: chrono::DateTime<Utc>,
    pub finished_at: chrono::DateTime<Utc>,
    /// Path of the written output file, if the write succeeded.
    pub output_path: Option<PathBuf>,
    /// Error text for an error run (timeout, abnormal exit, runner error).
    pub error: Option<String>,
}

fn never_shutdown() -> (tokio::sync::watch::Sender<bool>, ShutdownToken) {
    let (tx, rx) = tokio::sync::watch::channel(false);
    (tx, ShutdownToken::new(rx))
}

/// Create `<root>/cron` and return its path. Returns `Err` on failure.
fn ensure_workspace(root: &std::path::Path) -> anyhow::Result<PathBuf> {
    let workspace = root.join("cron");
    std::fs::create_dir_all(&workspace)?;
    Ok(workspace)
}

/// Map a run result to (status, response, error) — pure, no I/O or state.
fn classify_outcome(
    outcome: anyhow::Result<RunOutcome>,
    timeout: Duration,
) -> (RunStatus, Option<String>, Option<String>) {
    match outcome {
        Ok(RunOutcome::TimedOut) => (
            RunStatus::Error,
            None,
            Some(format!(
                "run exceeded the {}ms timeout and was killed",
                timeout.as_millis()
            )),
        ),
        Ok(RunOutcome::Completed(ExitKind::Normal, text)) => (RunStatus::Ok, Some(text), None),
        Ok(RunOutcome::Completed(ExitKind::Abnormal(code), text)) => {
            let error = if text.trim().is_empty() {
                format!("runner exited abnormally (code {code:?})")
            } else {
                text
            };
            (RunStatus::Error, None, Some(error))
        }
        Ok(RunOutcome::Completed(ExitKind::Interrupted { reason }, _)) => (
            RunStatus::Error,
            None,
            Some(format!("runner interrupted: {reason}")),
        ),
        Err(err) => (RunStatus::Error, None, Some(format!("{err:#}"))),
    }
}

struct ExecutionOutput {
    status: RunStatus,
    response: Option<String>,
    error: Option<String>,
}

const MAX_SCRIPT_OUTPUT_BYTES: usize = 64 * 1024;
const SCRIPT_OUTPUT_TRUNCATED: &str = "\n[output truncated]";

#[derive(Debug)]
struct ScriptOutput {
    stdout: String,
    final_stdout_line: Option<String>,
}

struct CapturedOutput {
    text: String,
    final_line: Option<String>,
}

/// Drain a script pipe while retaining only a bounded prefix. Draining after
/// the cap is reached is important: otherwise a chatty child can block on a
/// full pipe before it exits or can be reaped on timeout.
async fn capture_script_output<R>(
    mut reader: R,
    recorder: Option<RunRecorder>,
    stream: &'static str,
) -> std::io::Result<CapturedOutput>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let mut retained = Vec::with_capacity(MAX_SCRIPT_OUTPUT_BYTES);
    let mut final_line = Vec::new();
    let mut completed_line = None;
    let mut buffer = [0_u8; 8 * 1024];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        if let Some(recorder) = &recorder {
            if let Err(err) = recorder.capture(stream, &buffer[..read]) {
                recorder.remember_error(&err);
            }
        }
        let remaining = MAX_SCRIPT_OUTPUT_BYTES.saturating_sub(retained.len());
        let keep = remaining.min(read);
        retained.extend_from_slice(&buffer[..keep]);
        truncated |= keep < read;
        for byte in &buffer[..read] {
            if *byte == b'\n' {
                completed_line = Some(final_line.clone());
                final_line.clear();
            } else if final_line.len() < MAX_SCRIPT_OUTPUT_BYTES {
                final_line.push(*byte);
            }
        }
    }
    let mut text = String::from_utf8_lossy(&retained).into_owned();
    if truncated {
        text.push_str(SCRIPT_OUTPUT_TRUNCATED);
    }
    Ok(CapturedOutput {
        text,
        final_line: (!final_line.is_empty())
            .then(|| String::from_utf8_lossy(&final_line).into_owned())
            .or_else(|| completed_line.map(|line| String::from_utf8_lossy(&line).into_owned())),
    })
}

async fn run_script(
    config: &SchedulerConfig,
    job: &ScheduleJob,
    timeout: Duration,
    recorder: Option<RunRecorder>,
    mut shutdown: ShutdownToken,
) -> Result<ScriptOutput, String> {
    let script = job
        .payload
        .script
        .as_deref()
        .ok_or_else(|| "missing script".to_string())?;
    let path = assert_contained(&config.root, config.root.join(script))
        .map_err(|e| format!("invalid script path: {e:#}"))?;
    let metadata = std::fs::metadata(&path)
        .map_err(|e| format!("cannot read script {}: {e}", path.display()))?;
    let mut command = if matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("sh" | "bash")
    ) {
        let mut command = tokio::process::Command::new("bash");
        command.arg(&path);
        command
    } else {
        #[cfg(unix)]
        if std::os::unix::fs::PermissionsExt::mode(&metadata.permissions()) & 0o111 == 0 {
            return Err(format!("script {} is not executable", path.display()));
        }
        tokio::process::Command::new(&path)
    };
    command
        .current_dir(&config.root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Put the script in its own process group. A shell script may have a
    // foreground child which inherits its output pipes; killing only the shell
    // would leave that child alive and keep capture readers blocked.
    #[cfg(unix)]
    {
        unsafe {
            command.pre_exec(|| {
                nix::unistd::setpgid(nix::unistd::Pid::from_raw(0), nix::unistd::Pid::from_raw(0))
                    .map_err(std::io::Error::other)
            });
        }
    }
    let mut child = command
        .spawn()
        .map_err(|err| format!("failed to start script: {err}"))?;
    if let (Some(recorder), Some(pid)) = (&recorder, child.id()) {
        if let Err(err) = recorder.pid(pid) {
            recorder.remember_error(&err);
        }
    }
    let stdout = child.stdout.take().expect("stdout explicitly piped");
    let stderr = child.stderr.take().expect("stderr explicitly piped");
    // Include pipe draining in the deadline: a shell can exit successfully
    // while a background descendant still holds stdout/stderr open.
    let pid = child.id();
    let completed = tokio::select! {
        result = tokio::time::timeout(timeout, async {
            tokio::join!(
                child.wait(),
                capture_script_output(stdout, recorder.clone(), "stdout"),
                capture_script_output(stderr, recorder.clone(), "stderr"),
            )
        }) => result.ok(),
        _ = shutdown.cancelled() => None,
    };
    let Some((status, stdout, stderr)) = completed else {
        // Keep the original group id even after child.wait reaps its leader.
        #[cfg(unix)]
        if let Some(pid) = pid {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        if let Some(recorder) = &recorder {
            if let Err(err) = recorder.exit_code(None) {
                recorder.remember_error(&err);
            }
        }
        if shutdown.is_cancelled() {
            return Err("script interrupted by scheduler shutdown".to_string());
        }
        return Err(format!(
            "script exceeded the {}ms timeout and was killed",
            timeout.as_millis(),
        ));
    };
    let status = status.map_err(|err| format!("failed while waiting for script: {err}"))?;
    if let Some(recorder) = &recorder {
        if let Err(err) = recorder.exit_code(status.code()) {
            recorder.remember_error(&err);
        }
    }
    let stdout = stdout.map_err(|err| format!("failed to read script stdout: {err}"))?;
    let stderr = stderr.map_err(|err| format!("failed to read script stderr: {err}"))?;

    if !status.success() {
        return Err(format!(
            "script failed (exit {})\nstderr:\n{}\nstdout:\n{}",
            status
                .code()
                .map_or("signal".to_string(), |c| c.to_string()),
            stderr.text,
            stdout.text
        ));
    }
    Ok(ScriptOutput {
        stdout: stdout.text,
        final_stdout_line: stdout.final_line,
    })
}

fn wake_agent(final_line: Option<&str>) -> (bool, Option<String>) {
    let Some(line) = final_line else {
        return (true, None);
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return (true, None);
    };
    if value.get("wakeAgent").and_then(serde_json::Value::as_bool) == Some(false) {
        return (false, None);
    }
    (true, value.get("context").map(|v| v.to_string()))
}

/// Write the output file and update shared runtime state. Returns the
/// [`ExecuteResult`] for the caller.
async fn persist_execute_result(
    services: &ServiceRegistry,
    config: &SchedulerConfig,
    state: (&SchedulerState, Option<RunToken>),
    job: &ScheduleJob,
    fired_at: chrono::DateTime<Utc>,
    recorder: Option<&RunRecorder>,
    output: ExecutionOutput,
) -> ExecuteResult {
    let (state, token) = state;
    let name = if job.name.is_empty() {
        &job.id
    } else {
        &job.name
    };
    let finished_at = Utc::now();
    let ExecutionOutput {
        mut status,
        response,
        mut error,
    } = output;
    if let Some(storage_error) = recorder.and_then(RunRecorder::storage_error) {
        status = RunStatus::Error;
        error = Some(format!("failed to persist run history: {storage_error}"));
    }
    let delivery = deliver_result(
        services,
        job,
        &ExecutionOutput {
            status,
            response: response.clone(),
            error: error.clone(),
        },
    )
    .await;
    let schedule = format_schedule(&job.schedule);

    let uneventful = job.payload.quiet_output
        && status == RunStatus::Ok
        && response
            .as_deref()
            .is_some_and(|text| text.trim().is_empty() || text.trim() == "silent tick");
    let run_kind = if job.payload.script.is_none() {
        None
    } else if response
        .as_deref()
        .is_some_and(|text| text.trim() == "silent tick")
    {
        Some("silent_tick".to_string())
    } else if response
        .as_deref()
        .is_some_and(|text| text.starts_with("Gate output:\n"))
    {
        Some("woke_agent".to_string())
    } else {
        Some("script_only".to_string())
    };
    let written = if uneventful {
        Ok(None)
    } else {
        write_cron_run_output(&CronRunOutput {
            root: &config.root,
            job_id: &job.id,
            name,
            prompt: job.payload.message.as_deref().unwrap_or(""),
            schedule: &schedule,
            fired_at,
            finished_at,
            status,
            response,
            error: error.clone(),
            delivery: delivery.clone(),
        })
        .map(Some)
    };

    let (mut status, mut error, output_path) = match written {
        Ok(path) => {
            if let Some(path) = &path {
                tracing::info!("[scheduler] Wrote output {}", path.display());
            }
            (status, error, path)
        }
        Err(write_error) => {
            tracing::error!(
                "[scheduler] Failed to write output for {}: {write_error:#}",
                job.id
            );
            let persistence_error = format!("failed to persist run output: {write_error:#}");
            let error = Some(match error {
                Some(run_error) => format!("{run_error}; {persistence_error}"),
                None => persistence_error,
            });
            (RunStatus::Error, error, None)
        }
    };
    if let Some(recorder) = recorder {
        let outcome = if status == RunStatus::Ok {
            "succeeded"
        } else if error
            .as_deref()
            .is_some_and(|error| error.contains("timeout"))
        {
            "timed_out"
        } else {
            "failed"
        };
        let output_file = output_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        if let Err(err) = recorder.finalize(
            outcome,
            output_file.as_deref(),
            run_kind.as_deref() == Some("woke_agent"),
            error.as_deref(),
            uneventful,
        ) {
            status = RunStatus::Error;
            error = Some(format!("failed to persist run history: {err:#}"));
        }
    }
    let final_status = match status {
        RunStatus::Ok => LastStatus::Ok,
        RunStatus::Error => LastStatus::Error,
    };
    let exit_code = error.as_deref().and_then(|error| {
        error
            .strip_prefix("script failed (exit ")
            .and_then(|rest| rest.split(')').next())
            .and_then(|code| code.parse().ok())
    });
    state.mark_finished_details(
        &job.id,
        token,
        RunCompletion {
            status: final_status,
            error: error.clone(),
            exit_code,
            run_kind,
            delivery,
        },
    );

    ExecuteResult {
        status,
        fired_at,
        finished_at,
        output_path,
        error,
    }
}

/// Fire one job's runner and write its output file (for both ok and error runs).
async fn execute_job(
    config: &SchedulerConfig,
    services: &ServiceRegistry,
    state: &SchedulerState,
    job: &ScheduleJob,
    token: Option<RunToken>,
    shutdown: ShutdownToken,
) -> ExecuteResult {
    // The caller (timer loop or run-now) has already claimed the run via
    // `try_claim_running`, which reserved this token. Completion releases only that claim.
    let fired_at = Utc::now();

    let phase = if job.payload.script.is_some() {
        if job.payload.no_agent {
            "script"
        } else {
            "gate"
        }
    } else {
        "agent"
    };
    let recorder = match state
        .history
        .as_ref()
        .map(|history| {
            history.start(
                &job.id,
                Some(if job.payload.no_agent {
                    "script"
                } else {
                    &config.runner_kind
                }),
                if job.payload.no_agent {
                    None
                } else {
                    config.runner_model.as_deref()
                },
                phase,
            )
        })
        .transpose()
    {
        Ok(recorder) => recorder,
        Err(err) => {
            return persist_execute_result(
                services,
                config,
                (state, token),
                job,
                fired_at,
                None,
                ExecutionOutput {
                    status: RunStatus::Error,
                    response: None,
                    error: Some(format!("failed to start run history: {err:#}")),
                },
            )
            .await;
        }
    };

    tracing::info!(
        "[scheduler] Firing job {} ({})",
        job.id,
        format_schedule(&job.schedule)
    );

    // Run cwd is the agent's cron dir — contained under the agent root.
    let workspace = match ensure_workspace(&config.root) {
        Ok(ws) => ws,
        Err(err) => {
            tracing::error!(
                "[scheduler] Cannot create cron dir for job {}: {err:#}",
                job.id
            );
            let msg = format!("cannot create cron dir: {err:#}");
            return persist_execute_result(
                services,
                config,
                (state, token),
                job,
                fired_at,
                recorder.as_ref(),
                ExecutionOutput {
                    status: RunStatus::Error,
                    response: None,
                    error: Some(msg),
                },
            )
            .await;
        }
    };

    let timeout = config.timeout_for(job);
    if job.payload.script.is_some() {
        let script =
            match run_script(config, job, timeout, recorder.clone(), shutdown.clone()).await {
                Ok(script) => script,
                Err(error) => {
                    return persist_execute_result(
                        services,
                        config,
                        (state, token),
                        job,
                        fired_at,
                        recorder.as_ref(),
                        ExecutionOutput {
                            status: RunStatus::Error,
                            response: None,
                            error: Some(error),
                        },
                    )
                    .await
                }
            };
        if job.payload.no_agent {
            return persist_execute_result(
                services,
                config,
                (state, token),
                job,
                fired_at,
                recorder.as_ref(),
                ExecutionOutput {
                    status: RunStatus::Ok,
                    response: Some(script.stdout),
                    error: None,
                },
            )
            .await;
        }
        let (wake, context) = wake_agent(script.final_stdout_line.as_deref());
        if !wake {
            return persist_execute_result(
                services,
                config,
                (state, token),
                job,
                fired_at,
                recorder.as_ref(),
                ExecutionOutput {
                    status: RunStatus::Ok,
                    response: Some("silent tick".to_string()),
                    error: None,
                },
            )
            .await;
        }
        if let Some(recorder) = &recorder {
            if let Err(err) = recorder.phase("agent") {
                recorder.remember_error(&err);
            }
        }
        let mut prompt = job.payload.message.clone().unwrap_or_default();
        if let Some(context) = context {
            prompt.push_str("\n\nGate context:\n");
            prompt.push_str(&context);
        }
        let host_http_addr = *config
            .host_http_addr
            .lock()
            .expect("scheduler host address mutex poisoned");
        let outcome = fire_runner(
            FireRunnerRequest {
                runner_kind: &config.runner_kind,
                runner_command: &config.runner_command,
                runner_model: config.runner_model.clone(),
                runner_provider: config.runner_provider.clone(),
                runner_thinking: config.runner_thinking.clone(),
                system_prompt: config.system_context.clone(),
                workspace: &workspace,
                workspace_root: &config.root,
                agent_root: &config.root,
                workflow_root: &config.workflow_root,
                host_http_addr,
                prompt,
                job_id: &job.id,
                max_run_timeout_ms: config.max_run_timeout_ms,
                job_timeout: timeout,
                recorder: recorder.clone(),
            },
            services,
            shutdown,
        )
        .await;
        let (status, response, error) = classify_outcome(outcome, timeout);
        return persist_execute_result(
            services,
            config,
            (state, token),
            job,
            fired_at,
            recorder.as_ref(),
            ExecutionOutput {
                status,
                response: Some(format!(
                    "Gate output:\n{}\n\nAgent response:\n{}",
                    script.stdout,
                    response.unwrap_or_default()
                )),
                error,
            },
        )
        .await;
    }
    let host_http_addr = *config
        .host_http_addr
        .lock()
        .expect("scheduler host address mutex poisoned");
    let outcome = fire_runner(
        FireRunnerRequest {
            runner_kind: &config.runner_kind,
            runner_command: &config.runner_command,
            runner_model: config.runner_model.clone(),
            runner_provider: config.runner_provider.clone(),
            runner_thinking: config.runner_thinking.clone(),
            system_prompt: config.system_context.clone(),
            workspace: &workspace,
            workspace_root: &config.root,
            agent_root: &config.root,
            workflow_root: &config.workflow_root,
            host_http_addr,
            prompt: job.payload.message.clone().unwrap_or_default(),
            job_id: &job.id,
            max_run_timeout_ms: config.max_run_timeout_ms,
            job_timeout: timeout,
            recorder: recorder.clone(),
        },
        services,
        shutdown,
    )
    .await;

    let (status, response, error) = classify_outcome(outcome, timeout);
    persist_execute_result(
        services,
        config,
        (state, token),
        job,
        fired_at,
        recorder.as_ref(),
        ExecutionOutput {
            status,
            response,
            error,
        },
    )
    .await
}

/// Outcome of a `run-now` request, mapped to an HTTP status by the handler
/// (aihub `RunResult.status` parity).
pub enum RunNowOutcome {
    /// The manual run completed. Carries the run result; the handler maps an
    /// `ok` result to 200 and an `error` result to 500.
    Ran(ExecuteResult),
    /// The fire was skipped at execution time (the job vanished or was disabled
    /// between the claim and the fire) — aihub `skipped` result → 202.
    Skipped,
    /// This job is at its concurrency limit; the manual fire was rejected
    /// → 409.
    Conflict,
    /// The job is disabled, so it was not fired — aihub `inactive` result → 500.
    Disabled,
    /// No job with this id exists → 404.
    Unknown,
}

/// Fire a job immediately over HTTP without disturbing its schedule.
///
/// Leaves the next fire untouched, preserving any advancement by the timer
/// while the manual run is active, whether the scheduled fire succeeds or skips.
struct ManualRunGuard(Arc<SchedulerState>);

impl Drop for ManualRunGuard {
    fn drop(&mut self) {
        self.0.manual_finished();
    }
}

// The fire belongs to the scheduler, even when a synchronous HTTP caller
// disconnects. Track the task so extension shutdown waits for child cleanup.
pub async fn run_job_now(
    config: &SchedulerConfig,
    services: &ServiceRegistry,
    state: &Arc<SchedulerState>,
    job_id: &str,
) -> RunNowOutcome {
    state.manual_started();
    let guard = ManualRunGuard(Arc::clone(state));
    let config = config.clone();
    let services = services.clone();
    let state = Arc::clone(state);
    let job_id = job_id.to_string();
    tokio::spawn(async move {
        let _guard = guard;
        if state
            .shutdown()
            .is_some_and(|shutdown| shutdown.is_cancelled())
        {
            return RunNowOutcome::Skipped;
        }
        run_job_now_inner(&config, &services, &state, &job_id).await
    })
    .await
    .unwrap_or(RunNowOutcome::Skipped)
}

async fn run_job_now_inner(
    config: &SchedulerConfig,
    services: &ServiceRegistry,
    state: &Arc<SchedulerState>,
    job_id: &str,
) -> RunNowOutcome {
    let Some(job) = state.jobs().into_iter().find(|j| j.id == job_id) else {
        return RunNowOutcome::Unknown;
    };
    if !job.enabled {
        return RunNowOutcome::Disabled;
    }

    let Some(token) = state.try_claim_running(
        &job.id,
        Utc::now().timestamp_millis(),
        job.max_concurrent.unwrap_or(1),
    ) else {
        return RunNowOutcome::Conflict;
    };
    let _guard = RunningGuard {
        state: Arc::clone(state),
        job_id: job.id.clone(),
        token,
    };

    // Re-check the job survived the claim: a concurrent delete or disable over
    // the CRUD API between the lookup and the claim means there is nothing to
    // fire — report a skip rather than running a stale definition.
    match state.jobs().into_iter().find(|j| j.id == job_id) {
        Some(j) if j.enabled => {}
        _ => {
            state.clear_running(&job.id, token);
            return RunNowOutcome::Skipped;
        }
    }

    let (_shutdown_tx, fallback_shutdown) = never_shutdown();
    let shutdown = state.shutdown().unwrap_or(fallback_shutdown);
    let result = execute_job(config, services, state, &job, Some(token), shutdown).await;

    // Manual execution never writes next_run_at_ms, preserving timer advances
    // without a check/restore race against a concurrent scheduled fire.

    RunNowOutcome::Ran(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{jobs_path, load_jobs, Payload, Schedule};
    use async_trait::async_trait;
    use cap_runner::{KillReason, Runner, RunnerHandle, SpawnParams};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration as StdDuration;

    #[derive(Default)]
    struct RecordingSink {
        deliveries: std::sync::Mutex<Vec<(Destination, String)>>,
    }

    #[async_trait]
    impl DeliverySink for RecordingSink {
        async fn deliver(&self, dest: &Destination, text: &str) -> anyhow::Result<()> {
            self.deliveries
                .lock()
                .unwrap()
                .push((dest.clone(), text.to_string()));
            Ok(())
        }
    }

    // --- classify_outcome unit tests -----------------------------------------

    #[test]
    fn classify_outcome_timed_out() {
        let timeout = Duration::from_millis(500);
        let (status, response, error) = classify_outcome(Ok(RunOutcome::TimedOut), timeout);
        assert_eq!(status, RunStatus::Error);
        assert!(response.is_none());
        let err = error.unwrap();
        assert!(err.contains("500"), "timeout ms in message: {err}");
        assert!(err.contains("timeout"), "mentions timeout: {err}");
    }

    #[test]
    fn classify_outcome_normal_exit() {
        let timeout = Duration::from_millis(60_000);
        let (status, response, error) = classify_outcome(
            Ok(RunOutcome::Completed(ExitKind::Normal, "hello".to_string())),
            timeout,
        );
        assert_eq!(status, RunStatus::Ok);
        assert_eq!(response.as_deref(), Some("hello"));
        assert!(error.is_none());
    }

    #[test]
    fn wake_agent_honors_false_context_and_default() {
        assert_eq!(wake_agent(Some(r#"{"wakeAgent":false}"#)), (false, None));
        assert_eq!(
            wake_agent(Some(r#"{"wakeAgent":true,"context":{"count":2}}"#)),
            (true, Some(r#"{"count":2}"#.to_string()))
        );
        assert_eq!(wake_agent(Some("not json")), (true, None));
        assert_eq!(wake_agent(None), (true, None));
    }

    #[test]
    fn classify_outcome_abnormal_exit() {
        let timeout = Duration::from_millis(60_000);
        let (status, response, error) = classify_outcome(
            Ok(RunOutcome::Completed(
                ExitKind::Abnormal(Some(1)),
                String::new(),
            )),
            timeout,
        );
        assert_eq!(status, RunStatus::Error);
        assert!(response.is_none());
        assert!(error.unwrap().contains("abnormally"));
    }

    #[test]
    fn classify_outcome_interrupted() {
        let timeout = Duration::from_millis(60_000);
        let (status, response, error) = classify_outcome(
            Ok(RunOutcome::Completed(
                ExitKind::Interrupted { reason: "killed" },
                String::new(),
            )),
            timeout,
        );
        assert_eq!(status, RunStatus::Error);
        assert!(response.is_none());
        assert!(error.unwrap().contains("interrupted"));
    }

    #[test]
    fn classify_outcome_err() {
        let timeout = Duration::from_millis(60_000);
        let (status, response, error) = classify_outcome(Err(anyhow::anyhow!("boom")), timeout);
        assert_eq!(status, RunStatus::Error);
        assert!(response.is_none());
        assert!(error.unwrap().contains("boom"));
    }

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

    #[derive(Default)]
    struct ControlledRunner {
        completions: std::sync::Mutex<Vec<tokio::sync::oneshot::Sender<ExitKind>>>,
        sessions: std::sync::Mutex<Vec<String>>,
        panic_on_second: bool,
    }

    impl Runner for ControlledRunner {
        fn spawn<'a>(
            &self,
            params: SpawnParams<'a>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<RunnerHandle>> + Send + 'a>,
        > {
            let should_panic = {
                let mut sessions = self.sessions.lock().unwrap();
                sessions.push(params.run_id);
                self.panic_on_second && sessions.len() == 2
            };
            if should_panic {
                return Box::pin(async { panic!("second run panics") });
            }
            let (complete, completed) = tokio::sync::oneshot::channel();
            self.completions.lock().unwrap().push(complete);
            Box::pin(async move {
                let (kill, killed) = tokio::sync::oneshot::channel();
                let done = tokio::spawn(async move {
                    tokio::select! {
                        outcome = completed => outcome.unwrap(),
                        _ = killed => ExitKind::Interrupted { reason: "killed" },
                    }
                });
                Ok(RunnerHandle::new(0, kill, done))
            })
        }
    }

    impl ControlledRunner {
        async fn wait_for_starts(&self, count: usize) {
            tokio::time::timeout(Duration::from_secs(2), async {
                while self.sessions.lock().unwrap().len() < count {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("runner started");
        }

        fn finish_first(&self) {
            self.completions
                .lock()
                .unwrap()
                .remove(0)
                .send(ExitKind::Normal)
                .unwrap();
        }
    }

    fn controlled_services(panic_on_second: bool) -> (ServiceRegistry, Arc<ControlledRunner>) {
        let runner = Arc::new(ControlledRunner {
            panic_on_second,
            ..Default::default()
        });
        let mut services = ServiceRegistry::default();
        services
            .service::<dyn Runner>("fake", runner.clone())
            .unwrap();
        (services, runner)
    }

    fn test_config(root: PathBuf, job_timeout_ms: u64) -> SchedulerConfig {
        SchedulerConfig {
            root,
            workflow_root: std::path::PathBuf::new(),
            host_http_addr: Arc::new(std::sync::Mutex::new(None)),
            runner_kind: "fake".to_string(),
            runner_command: String::new(),
            runner_model: None,
            runner_provider: None,
            runner_thinking: None,
            system_context: None,
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
                message: Some(msg.to_string()),
                script: None,
                no_agent: false,
                quiet_output: false,
            },
            timeout_ms,
            max_concurrent: None,
            deliver: Vec::new(),
        }
    }

    fn script_job(id: &str, script: &str, timeout_ms: u64) -> ScheduleJob {
        let mut job = job_with(id, Some(timeout_ms), true, "unused");
        job.payload.message = None;
        job.payload.script = Some(script.to_string());
        job.payload.no_agent = true;
        job
    }

    fn write_script(root: &std::path::Path, name: &str, body: &str) {
        let scripts = root.join("cron/scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        let path = scripts.join(name);
        std::fs::write(&path, body).unwrap();
    }

    fn latest_output(root: &std::path::Path, job_id: &str) -> Option<String> {
        let dir = root.join("cron").join("output").join(job_id);
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .ok()?
            .filter_map(|e| e.ok())
            .collect();
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
        assert_eq!(
            config.timeout_for(&with_override),
            Duration::from_millis(250)
        );
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

        // First tick claims the job in shared state (now running) and spawns the
        // fire; give the task a beat to reach `runner.spawn`.
        let mut fires = tokio::task::JoinSet::new();
        let (_shutdown_tx, shutdown) = never_shutdown();
        run_due_jobs(&config, &services, &state, &mut jobs, &mut fires, &shutdown).await;
        assert!(state.is_running("job"));
        tokio::time::sleep(StdDuration::from_millis(50)).await;
        assert_eq!(spawns.load(Ordering::SeqCst), 1);
        assert!(state.last_skipped_at_ms("job").is_none());

        // Force the job due again while the first run is still in flight.
        jobs[0].next_run_at_ms = Utc::now().timestamp_millis() - 1;
        run_due_jobs(&config, &services, &state, &mut jobs, &mut fires, &shutdown).await;

        // Overlap-skip: no new spawn, skip is bookmarked, next run recomputed.
        assert_eq!(spawns.load(Ordering::SeqCst), 1, "second fire skipped");
        assert!(
            state.last_skipped_at_ms("job").is_some(),
            "skip bookmarked in state"
        );
        assert!(
            jobs[0].next_run_at_ms > Utc::now().timestamp_millis(),
            "next run recomputed forward"
        );

        // Let the in-flight run finish so the claim clears.
        tokio::time::sleep(StdDuration::from_millis(600)).await;
        assert!(!state.is_running("job"));
    }

    #[tokio::test]
    async fn cap_two_fires_twice_skips_third_and_completion_frees_one_slot() {
        let dir = tempfile::tempdir().unwrap();
        let (services, runner) = controlled_services(false);
        let config = test_config(dir.path().to_path_buf(), 60_000);
        let mut jobs = vec![armed("job", None)];
        jobs[0].job.max_concurrent = Some(2);
        let history = crate::history::History::open(dir.path(), 100).unwrap();
        let state = Arc::new(SchedulerState::with_history(
            vec![jobs[0].job.clone()],
            history.clone(),
        ));
        let mut fires = tokio::task::JoinSet::new();
        let (_tx, shutdown) = never_shutdown();
        for starts in 1..=2 {
            jobs[0].next_run_at_ms = 0;
            run_due_jobs(&config, &services, &state, &mut jobs, &mut fires, &shutdown).await;
            runner.wait_for_starts(starts).await;
        }
        assert_eq!(state.runtime("job").running_count(), 2);
        assert_eq!(history.list("job").unwrap().len(), 2);
        jobs[0].next_run_at_ms = 0;
        run_due_jobs(&config, &services, &state, &mut jobs, &mut fires, &shutdown).await;
        assert_eq!(runner.sessions.lock().unwrap().len(), 2);
        assert!(state.last_skipped_at_ms("job").is_some());
        runner.finish_first();
        tokio::time::timeout(Duration::from_secs(2), fires.join_next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(state.runtime("job").running_count(), 1);
        jobs[0].next_run_at_ms = 0;
        run_due_jobs(&config, &services, &state, &mut jobs, &mut fires, &shutdown).await;
        runner.wait_for_starts(3).await;
        jobs[0].next_run_at_ms = 0;
        run_due_jobs(&config, &services, &state, &mut jobs, &mut fires, &shutdown).await;
        assert_eq!(runner.sessions.lock().unwrap().len(), 3);
        runner.finish_first();
        runner.finish_first();
        while let Some(result) = fires.join_next().await {
            result.unwrap();
        }
        assert_eq!(state.runtime("job").running_count(), 0);
        let records = history.list("job").unwrap();
        assert_eq!(records.len(), 3);
        assert!(records.iter().all(|record| record.outcome == "succeeded"));
        let outputs = std::fs::read_dir(dir.path().join("cron/output/job"))
            .unwrap()
            .count();
        assert_eq!(outputs, 3);
        let sessions = runner.sessions.lock().unwrap();
        assert_eq!(
            sessions
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            3
        );
    }

    #[tokio::test]
    async fn concurrent_history_off_runs_get_distinct_runner_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let (services, runner) = controlled_services(false);
        let config = test_config(dir.path().to_path_buf(), 60_000);
        let mut jobs = vec![armed("job", None)];
        jobs[0].job.max_concurrent = Some(32);
        let state = Arc::new(SchedulerState::new(vec![jobs[0].job.clone()]));
        let mut fires = tokio::task::JoinSet::new();
        let (_tx, shutdown) = never_shutdown();
        for _ in 0..32 {
            jobs[0].next_run_at_ms = 0;
            run_due_jobs(&config, &services, &state, &mut jobs, &mut fires, &shutdown).await;
        }
        runner.wait_for_starts(32).await;
        let unique = runner
            .sessions
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<std::collections::HashSet<_>>()
            .len();
        for _ in 0..32 {
            runner.finish_first();
        }
        while let Some(result) = fires.join_next().await {
            result.unwrap();
        }
        assert_eq!(unique, 32, "one session per run even when history is off");
    }

    #[tokio::test]
    async fn panic_releases_only_the_panicking_runs_slot() {
        let dir = tempfile::tempdir().unwrap();
        let (services, runner) = controlled_services(true);
        let config = test_config(dir.path().to_path_buf(), 60_000);
        let mut jobs = vec![armed("job", None)];
        jobs[0].job.max_concurrent = Some(2);
        let state = Arc::new(SchedulerState::new(vec![jobs[0].job.clone()]));
        let mut fires = tokio::task::JoinSet::new();
        let (_tx, shutdown) = never_shutdown();
        run_due_jobs(&config, &services, &state, &mut jobs, &mut fires, &shutdown).await;
        runner.wait_for_starts(1).await;
        jobs[0].next_run_at_ms = 0;
        run_due_jobs(&config, &services, &state, &mut jobs, &mut fires, &shutdown).await;
        let panic = tokio::time::timeout(Duration::from_secs(2), fires.join_next())
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(panic.is_panic());
        assert_eq!(state.runtime("job").running_count(), 1);
        jobs[0].next_run_at_ms = 0;
        run_due_jobs(&config, &services, &state, &mut jobs, &mut fires, &shutdown).await;
        runner.wait_for_starts(3).await;
        jobs[0].next_run_at_ms = 0;
        run_due_jobs(&config, &services, &state, &mut jobs, &mut fires, &shutdown).await;
        assert_eq!(runner.sessions.lock().unwrap().len(), 3);
        runner.finish_first();
        runner.finish_first();
        while let Some(result) = fires.join_next().await {
            result.unwrap();
        }
    }

    #[tokio::test]
    async fn manual_runs_respect_cap_and_keep_a_successful_scheduled_advance() {
        let dir = tempfile::tempdir().unwrap();
        let (services, runner) = controlled_services(false);
        let config = test_config(dir.path().to_path_buf(), 60_000);
        let mut jobs = vec![armed("job", None)];
        jobs[0].job.max_concurrent = Some(2);
        let state = Arc::new(SchedulerState::new(vec![jobs[0].job.clone()]));
        state.set_next_run("job", Some(123));
        let manual = {
            let config = config.clone();
            let services = services.clone();
            let state = state.clone();
            tokio::spawn(async move { run_job_now(&config, &services, &state, "job").await })
        };
        runner.wait_for_starts(1).await;
        let mut fires = tokio::task::JoinSet::new();
        let (_tx, shutdown) = never_shutdown();
        run_due_jobs(&config, &services, &state, &mut jobs, &mut fires, &shutdown).await;
        runner.wait_for_starts(2).await;
        let advanced = state.runtime("job").next_run_at_ms.unwrap();
        assert!(advanced > 123);
        assert!(matches!(
            run_job_now(&config, &services, &state, "job").await,
            RunNowOutcome::Conflict
        ));
        runner.finish_first();
        assert!(matches!(manual.await.unwrap(), RunNowOutcome::Ran(_)));
        assert_eq!(state.runtime("job").next_run_at_ms, Some(advanced));
        assert_eq!(state.runtime("job").running_count(), 1);
        let extra = {
            let config = config.clone();
            let services = services.clone();
            let state = state.clone();
            tokio::spawn(async move { run_job_now(&config, &services, &state, "job").await })
        };
        runner.wait_for_starts(3).await;
        assert_eq!(state.runtime("job").running_count(), 2);
        runner.finish_first();
        runner.finish_first();
        assert!(matches!(extra.await.unwrap(), RunNowOutcome::Ran(_)));
        while let Some(result) = fires.join_next().await {
            result.unwrap();
        }
    }

    #[tokio::test]
    async fn timed_out_run_is_killed_and_recorded_as_error() {
        let dir = tempfile::tempdir().unwrap();
        // Child would run for 10s, but the job timeout is 150ms.
        let (services, _spawns) = services_with(StdDuration::from_secs(10));
        let config = test_config(dir.path().to_path_buf(), 150);
        let job = armed("timeout-job", None).job;
        let state = Arc::new(SchedulerState::new(vec![]));

        let (_shutdown_tx, shutdown) = never_shutdown();
        execute_job(
            &config,
            &services,
            &state,
            &job,
            state.try_claim_running(&job.id, Utc::now().timestamp_millis(), 1),
            shutdown,
        )
        .await;

        let out = latest_output(dir.path(), "timeout-job").expect("output written");
        assert!(out.contains("status: error"), "recorded as error: {out}");
        assert!(out.contains("timeout"), "timeout message present: {out}");
    }

    #[tokio::test]
    async fn script_failure_records_exit_code_and_bounded_streamed_output() {
        let dir = tempfile::tempdir().unwrap();
        write_script(
            dir.path(),
            "fail.sh",
            "head -c 131072 /dev/zero | tr '\\0' x\necho stderr-detail >&2\nexit 17\n",
        );
        let config = test_config(dir.path().to_path_buf(), 60_000);
        let job = script_job("script-failure", "cron/scripts/fail.sh", 60_000);

        let (_shutdown_tx, shutdown) = never_shutdown();
        let error = run_script(&config, &job, Duration::from_secs(1), None, shutdown)
            .await
            .expect_err("non-zero script must fail");
        assert!(error.contains("exit 17"), "exit code retained: {error}");
        assert!(error.contains("stderr-detail"), "stderr retained: {error}");
        assert!(
            error.contains(SCRIPT_OUTPUT_TRUNCATED),
            "output capped: {error}"
        );
        assert!(
            error.len() <= MAX_SCRIPT_OUTPUT_BYTES + 512,
            "capture is bounded"
        );
    }

    #[tokio::test]
    async fn timed_out_script_is_killed_and_reaped() {
        let dir = tempfile::tempdir().unwrap();
        write_script(dir.path(), "sleep.sh", "sleep 10\n");
        let config = test_config(dir.path().to_path_buf(), 60_000);
        let job = script_job("script-timeout", "cron/scripts/sleep.sh", 50);

        let started = std::time::Instant::now();
        let (_shutdown_tx, shutdown) = never_shutdown();
        let error = run_script(&config, &job, Duration::from_millis(50), None, shutdown)
            .await
            .expect_err("timed-out script must fail");
        assert!(
            started.elapsed() < StdDuration::from_secs(2),
            "child was reaped"
        );
        assert!(error.contains("timeout"), "timeout retained: {error}");
    }

    #[tokio::test]
    async fn exited_script_with_descendant_pipes_obeys_timeout_and_shutdown() {
        for cancel in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            write_script(
                dir.path(),
                "background.sh",
                "echo BEFORE-BACKGROUND\nsleep 30 &\necho $! > descendant.pid\n",
            );
            let config = test_config(dir.path().to_path_buf(), 60_000);
            let job = script_job("background", "cron/scripts/background.sh", 100);
            let history = crate::history::History::open(dir.path(), 3).unwrap();
            let recorder = history
                .start("background", Some("script"), None, "script")
                .unwrap();
            let (tx, shutdown) = never_shutdown();
            let started = std::time::Instant::now();
            let cancel_task = tokio::spawn(async move {
                if cancel {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    tx.send(true).unwrap();
                } else {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            });
            let timeout = if cancel {
                Duration::from_secs(30)
            } else {
                Duration::from_millis(100)
            };
            let error = run_script(&config, &job, timeout, Some(recorder.clone()), shutdown)
                .await
                .unwrap_err();
            cancel_task.abort();
            assert!(started.elapsed() < Duration::from_secs(2));
            assert!(error.contains(if cancel { "shutdown" } else { "timeout" }));
            assert!(history
                .events(recorder.id(), 0)
                .unwrap()
                .iter()
                .any(|event| event.text.contains("BEFORE-BACKGROUND")));
            let pid: i32 = std::fs::read_to_string(dir.path().join("descendant.pid"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let dead = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let output = std::process::Command::new("ps")
                        .args(["-o", "stat=", "-p", &pid.to_string()])
                        .output()
                        .unwrap();
                    let state = String::from_utf8_lossy(&output.stdout);
                    if state.trim().is_empty() || state.trim().starts_with('Z') {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;
            assert!(dead.is_ok(), "background descendant was killed");
        }
    }

    #[tokio::test]
    async fn scheduled_run_records_runtime_status_without_orchestrator_snapshot() {
        // A fired job's outcome lands in the scheduler's own runtime state
        // (what the Cron tab renders) and its output file. The scheduler fires
        // the runner service directly (`execute_job` takes only services +
        // scheduler state — no event bus / orchestrator run handle), so a
        // scheduled run can never reach the orchestrator's RunSnapshot or its
        // run list. This test pins that isolation: the run completes and is
        // visible at the cron level only.
        let dir = tempfile::tempdir().unwrap();
        let (services, spawns) = services_with(StdDuration::from_millis(10));
        let config = test_config(dir.path().to_path_buf(), 60_000);
        let job = armed("isolated", None).job;
        let state = Arc::new(SchedulerState::new(vec![job.clone()]));

        // The timer loop claims the run before firing; `execute_job` only
        // records the outcome. Mirror that here so the claimed `last_run_at_ms`
        // is set, as it would be in production.
        let token = state.try_claim_running("isolated", Utc::now().timestamp_millis(), 1);
        let (_shutdown_tx, shutdown) = never_shutdown();
        execute_job(&config, &services, &state, &job, token, shutdown).await;

        assert_eq!(spawns.load(Ordering::SeqCst), 1, "runner fired directly");
        let rt = state.runtime("isolated");
        assert_eq!(rt.last_status, Some(LastStatus::Ok), "status at cron level");
        assert!(rt.last_run_at_ms.is_some(), "last run recorded for the tab");
        assert!(
            latest_output(dir.path(), "isolated").is_some(),
            "output written at cron level"
        );
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

        let mut fires = tokio::task::JoinSet::new();
        let (_shutdown_tx, shutdown) = never_shutdown();
        run_due_jobs(&config, &services, &state, &mut jobs, &mut fires, &shutdown).await;
        // The fire task panics; the drop guard must still clear the shared claim.
        tokio::time::sleep(StdDuration::from_millis(100)).await;
        assert!(
            !state.is_running("job"),
            "overlap claim released after a panicking fire"
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
        let mut fires = tokio::task::JoinSet::new();
        let (_shutdown_tx, shutdown) = never_shutdown();
        run_due_jobs(&config, &services, &state, &mut jobs, &mut fires, &shutdown).await;
        assert!(jobs[0].next_run_at_ms > before, "timer re-armed forward");
    }

    // --- Hot reload (ALG-223) ---------------------------------------------

    #[test]
    fn arm_jobs_loads_enabled_jobs_from_state() {
        let state = SchedulerState::new(vec![
            job_with("a", None, true, "x"),
            job_with("b", None, true, "y"),
        ]);
        let armed = arm_jobs(&state);
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
        let armed = arm_jobs(&state);
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
        let armed = arm_jobs(&state);
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
    fn reload_preserves_running_claim_for_surviving_job() {
        // The overlap claim lives in shared state, so a re-arm (after an edit)
        // keeps a surviving job's in-flight claim — arm_jobs does not reset it.
        let state = SchedulerState::new(vec![job_with("a", None, true, "x")]);
        let _ = arm_jobs(&state);

        // Claim job "a" as running.
        assert!(state
            .try_claim_running("a", Utc::now().timestamp_millis(), 1)
            .is_some());

        // Edit "a"'s message in state, then re-arm.
        state.set_jobs(vec![job_with("a", None, true, "edited")]);
        let rearmed = arm_jobs(&state);

        assert_eq!(rearmed.len(), 1);
        assert!(state.is_running("a"), "surviving job keeps its claim");
        assert_eq!(rearmed[0].job.payload.message.as_deref(), Some("edited"));
    }

    #[test]
    fn reload_dropping_running_job_prunes_its_claim() {
        let state = SchedulerState::new(vec![
            job_with("a", None, true, "x"),
            job_with("b", None, true, "y"),
        ]);
        let _ = arm_jobs(&state);

        // Job "b" is running.
        assert!(state
            .try_claim_running("b", Utc::now().timestamp_millis(), 1)
            .is_some());

        // Remove "b" and re-arm.
        state.set_jobs(vec![job_with("a", None, true, "x")]);
        let rearmed = arm_jobs(&state);

        // "b" is gone; set_jobs prunes its runtime claim so a recreated job with
        // the same id starts fresh and is not falsely overlap-skipped.
        assert_eq!(rearmed.len(), 1);
        assert_eq!(rearmed[0].job.id, "a");
        assert!(!state.is_running("b"), "removed job's claim pruned");
    }

    #[test]
    fn malformed_reload_keeps_last_valid_jobs_and_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path().to_path_buf(), 60_000);
        write_jobs(dir.path(), ONE_JOB);
        let state = SchedulerState::new(load_jobs(dir.path(), |_| {}));
        let armed = arm_jobs(&state);
        let mut fp = fingerprint(&config);
        assert_eq!(armed.len(), 1);

        // Malformed write is rejected; the last valid schedule keeps running.
        std::thread::sleep(Duration::from_millis(10));
        write_jobs(dir.path(), "not json");
        maybe_reload(&config, &state, &mut fp);
        assert_eq!(state.jobs().len(), 1);

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
        assert!(
            state.jobs().is_empty(),
            "no reload when fingerprint unchanged"
        );
    }

    #[test]
    fn per_job_disabled_is_live_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path().to_path_buf(), 60_000);
        write_jobs(dir.path(), ONE_JOB);
        let state = SchedulerState::new(load_jobs(dir.path(), |_| {}));
        let armed = arm_jobs(&state);
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
        let rearmed = arm_jobs(&state);
        assert!(rearmed.is_empty(), "disabled job is not armed");
    }

    #[test]
    fn delivery_text_skips_silent_results_and_preserves_utf8_when_capped() {
        let silent = ExecutionOutput {
            status: RunStatus::Ok,
            response: Some(" silent tick ".into()),
            error: None,
        };
        assert!(delivery_text(&silent).is_none());
        let output = ExecutionOutput {
            status: RunStatus::Ok,
            response: Some("é".repeat(MAX_DELIVERY_BYTES)),
            error: None,
        };
        let text = delivery_text(&output).unwrap();
        assert!(text.is_char_boundary(text.len()));
        assert!(text.ends_with("[truncated]"));

        let error = ExecutionOutput {
            status: RunStatus::Error,
            response: Some("ignored response".into()),
            error: Some("runner failed".into()),
        };
        assert_eq!(delivery_text(&error).as_deref(), Some("runner failed"));
    }

    #[tokio::test]
    async fn delivery_sends_only_runner_response_for_woken_gate_and_warns_for_missing_sink() {
        let sink = Arc::new(RecordingSink::default());
        let mut services = ServiceRegistry::default();
        services
            .service::<dyn DeliverySink>("slack", sink.clone())
            .unwrap();
        let mut job = script_job("delivery", "echo ignored", 60_000);
        job.deliver = vec![
            crate::store::DeliverTarget {
                target: "slack".into(),
                channel: Some("#alerts".into()),
                user: None,
            },
            crate::store::DeliverTarget {
                target: "missing".into(),
                channel: None,
                user: Some("42".into()),
            },
        ];
        let output = ExecutionOutput {
            status: RunStatus::Ok,
            response: Some("Gate output:\ngate context\n\nAgent response:\nrunner result".into()),
            error: None,
        };

        let outcomes = deliver_result(&services, &job, &output).await;

        assert_eq!(
            sink.deliveries.lock().unwrap().as_slice(),
            &[(
                Destination {
                    channel: Some("#alerts".into()),
                    user: None,
                },
                "runner result".into(),
            )]
        );
        assert_eq!(
            output.response.as_deref(),
            Some("Gate output:\ngate context\n\nAgent response:\nrunner result")
        );
        assert_eq!(outcomes[0], "slack: delivered");
        assert!(outcomes[1].starts_with("missing: warning: sink unavailable"));
    }
    #[tokio::test]
    async fn persistent_history_drops_only_successful_quiet_ticks() {
        let dir = tempfile::tempdir().unwrap();
        write_script(dir.path(), "silent.sh", "echo '{\"wakeAgent\":false}'\n");
        write_script(dir.path(), "fail.sh", "echo BOOM >&2\nexit 1\n");
        let config = test_config(dir.path().to_path_buf(), 60_000);
        let history = crate::history::History::open(dir.path(), 3).unwrap();
        let state = SchedulerState::with_history(vec![], history.clone());
        let (services, _) = services_with(StdDuration::from_millis(1));
        let mut job = script_job("quiet", "cron/scripts/silent.sh", 60_000);
        job.payload.no_agent = false;
        job.payload.message = Some("wake".into());
        job.payload.quiet_output = true;
        let (_tx, shutdown) = never_shutdown();
        let result = execute_job(
            &config,
            &services,
            &state,
            &job,
            state.try_claim_running(&job.id, Utc::now().timestamp_millis(), 1),
            shutdown.clone(),
        )
        .await;
        assert_eq!(result.status, RunStatus::Ok);
        assert!(result.output_path.is_none());
        assert!(history.list("quiet").unwrap().is_empty());
        assert_eq!(state.runtime("quiet").last_status, Some(LastStatus::Ok));

        job.payload.script = Some("cron/scripts/fail.sh".into());
        let result = execute_job(
            &config,
            &services,
            &state,
            &job,
            state.try_claim_running(&job.id, Utc::now().timestamp_millis(), 1),
            shutdown,
        )
        .await;
        assert_eq!(result.status, RunStatus::Error);
        let record = history.list("quiet").unwrap().remove(0);
        assert_eq!(record.outcome, "failed");
        assert_eq!(record.exit_code, Some(1));
        assert!(history
            .events(&record.id, 0)
            .unwrap()
            .iter()
            .any(|event| event.kind == "stderr" && event.text.contains("BOOM")));
    }

    #[tokio::test]
    async fn script_output_is_durable_before_timeout_and_output_write_failure_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        write_script(dir.path(), "partial.sh", "echo BEFORE-TIMEOUT\nsleep 10\n");
        let config = test_config(dir.path().to_path_buf(), 60_000);
        let history = crate::history::History::open(dir.path(), 3).unwrap();
        let job = script_job("partial", "cron/scripts/partial.sh", 300);
        let state = Arc::new(SchedulerState::with_history(
            vec![job.clone()],
            history.clone(),
        ));
        let (services, _) = services_with(StdDuration::from_millis(1));
        let task = {
            let config = config.clone();
            let state = state.clone();
            let services = services.clone();
            tokio::spawn(async move { run_job_now(&config, &services, &state, "partial").await })
        };
        let record = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(record) = history.list("partial").unwrap().first() {
                    if history
                        .events(&record.id, 0)
                        .unwrap()
                        .iter()
                        .any(|event| event.text.contains("BEFORE-TIMEOUT"))
                    {
                        break record.clone();
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(record.outcome, "running");
        assert!(!task.is_finished());
        task.await.unwrap();
        let record = history.get(&record.id).unwrap().unwrap();
        assert_eq!(record.outcome, "timed_out");
        assert!(history
            .events(&record.id, 0)
            .unwrap()
            .iter()
            .any(|event| event.text.contains("BEFORE-TIMEOUT")));

        write_script(dir.path(), "ok.sh", "echo DONE\n");
        // The timeout wrote an output directory; block this job's own output path.
        std::fs::write(
            dir.path().join("cron/output/write-fail"),
            "blocks directory",
        )
        .unwrap();
        let job = script_job("write-fail", "cron/scripts/ok.sh", 60_000);
        let (_tx, shutdown) = never_shutdown();
        let result = execute_job(
            &config,
            &services,
            &state,
            &job,
            state.try_claim_running(&job.id, Utc::now().timestamp_millis(), 1),
            shutdown,
        )
        .await;
        assert_eq!(result.status, RunStatus::Error);
        let record = history.list("write-fail").unwrap().remove(0);
        assert_eq!(record.outcome, "failed");
        assert_eq!(record.exit_code, Some(0));
        assert!(record.error.unwrap().contains("persist run output"));
        assert!(history
            .events(&record.id, 0)
            .unwrap()
            .iter()
            .any(|event| event.text.contains("DONE")));
    }
    #[tokio::test]
    async fn shutdown_joins_manual_run_after_http_caller_disconnects() {
        let dir = tempfile::tempdir().unwrap();
        write_script(
            dir.path(),
            "shutdown.sh",
            "echo BEFORE-SHUTDOWN\nsleep 10\n",
        );
        let config = test_config(dir.path().to_path_buf(), 60_000);
        let history = crate::history::History::open(dir.path(), 3).unwrap();
        let job = script_job("shutdown", "cron/scripts/shutdown.sh", 60_000);
        let state = Arc::new(SchedulerState::with_history(vec![job], history.clone()));
        let (services, _) = services_with(StdDuration::from_millis(1));
        let (tx, shutdown) = never_shutdown();
        state.set_shutdown(shutdown);
        let caller = {
            let config = config.clone();
            let state = state.clone();
            tokio::spawn(async move { run_job_now(&config, &services, &state, "shutdown").await })
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(record) = history.list("shutdown").unwrap().first() {
                    if history
                        .events(&record.id, 0)
                        .unwrap()
                        .iter()
                        .any(|event| event.text.contains("BEFORE-SHUTDOWN"))
                    {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        caller.abort();
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), state.wait_manual_runs())
            .await
            .unwrap();
        let record = history.list("shutdown").unwrap().remove(0);
        assert_eq!(record.outcome, "failed");
        assert!(record.error.unwrap().contains("shutdown"));
        assert!(history
            .events(&record.id, 0)
            .unwrap()
            .iter()
            .any(|event| event.text.contains("BEFORE-SHUTDOWN")));
        let pid = record.pid.unwrap();
        assert!(std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| !status.success()));
    }
}
