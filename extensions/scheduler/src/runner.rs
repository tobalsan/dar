//! Glue to spawn the agent's default runner for one cron fire and capture its
//! response text.
//!
//! The `cap-runner` contract streams output through a `RunnerEventSink` (display
//! lines) and a `RunnerEventStore` (structured `protocol_event` payloads). The
//! scheduler supplies its own capturing store that keeps the latest assistant
//! text — the parity equivalent of aihub's `latestAssistantText(result.payloads)`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use cap_runner::{ExitKind, KillReason, Runner, RunnerEventSink, RunnerEventStore, SpawnParams};
use chrono::{DateTime, Utc};
use host_api::ServiceRegistry;

/// Outcome of one runner fire: how it exited and the captured assistant text.
pub struct RunOutcome {
    pub exit: ExitKind,
    pub response: String,
    /// True when the scheduler's own timeout elapsed and it killed the child.
    /// The run is recorded as an error regardless of how the child exited.
    pub timed_out: bool,
}

/// Sink that discards display lines; the scheduler captures structured text via
/// `CaptureStore` instead.
struct NullSink;

impl RunnerEventSink for NullSink {
    fn push(&self, _line: String) {}
}

/// Store impl that captures the latest assistant `text` from streamed
/// `protocol_event` payloads. Errors are surfaced via the run's `ExitKind`, not
/// captured here.
#[derive(Default)]
struct CaptureStore {
    latest_assistant: Mutex<Option<String>>,
}

impl RunnerEventStore for CaptureStore {
    fn insert_event(
        &self,
        _run_id: Option<&str>,
        _issue_identifier: &str,
        _kind: &'static str,
        payload: &str,
        _ts: DateTime<Utc>,
    ) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
            return;
        };
        if value.get("type").and_then(|t| t.as_str()) != Some("protocol_event") {
            return;
        }
        if value.get("log_row").and_then(|r| r.as_str()) != Some("assistant") {
            return;
        }
        let Some(text) = value.get("text").and_then(|t| t.as_str()) else {
            return;
        };
        let text = text.trim();
        if !text.is_empty() {
            *self
                .latest_assistant
                .lock()
                .expect("capture mutex poisoned") = Some(text.to_string());
        }
    }
}

/// Resolve the named runner service (`runner.use`, default `pi`) and fire it
/// once with `prompt` as the agent prompt. Blocks until the run completes,
/// returning its exit classification and captured assistant text.
#[allow(clippy::too_many_arguments)]
pub async fn fire_runner(
    services: &ServiceRegistry,
    runner_kind: &str,
    runner_command: &str,
    workspace: &std::path::Path,
    workspace_root: &std::path::Path,
    agent_root: &std::path::Path,
    prompt: String,
    job_id: &str,
    max_run_timeout_ms: u64,
    job_timeout: Duration,
) -> Result<RunOutcome> {
    let runner = services
        .get_named::<dyn Runner>(runner_kind)
        .with_context(|| format!("resolving runner service {runner_kind:?}"))?;

    let capture = Arc::new(CaptureStore::default());
    let events: Arc<dyn RunnerEventSink> = Arc::new(NullSink);
    let store: Arc<dyn RunnerEventStore> = capture.clone();
    let last_event_at = Arc::new(std::sync::Mutex::new(Utc::now()));

    let params = SpawnParams::builder(
        runner_command,
        runner_kind,
        workspace,
        workspace_root,
        agent_root,
        prompt,
        format!("scheduler:{job_id}"),
        format!("scheduler-{job_id}-{}", Utc::now().timestamp_millis()),
        max_run_timeout_ms,
        events,
        store,
        last_event_at,
    )
    .build();

    let handle = runner.spawn(params).await.context("spawning runner")?;

    // Scheduler-owned per-run timeout. Poll the supervising task for completion
    // until the deadline; on elapse, kill the child and wait for it to exit so
    // we never leak a runaway process. This guard is independent of (and
    // typically tighter than) the runner's own `max_run_timeout_ms` hard cap.
    let deadline = tokio::time::Instant::now() + job_timeout;
    let (exit, timed_out) = loop {
        // A run that completes on its own (even right at the deadline) is
        // collected as a normal exit; only a child still running past the
        // deadline is killed and flagged timed-out.
        if handle.is_finished() {
            break (handle.wait().await, false);
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                "[scheduler] Job {job_id} exceeded {}ms timeout; killing runner",
                job_timeout.as_millis()
            );
            let exit = handle.request_kill_and_wait(KillReason::Timeout).await;
            break (exit, true);
        }
        // Poll cadence is small relative to any realistic job timeout; a kill is
        // never delayed by more than this interval past the deadline.
        let next_poll = (tokio::time::Instant::now() + Duration::from_millis(50)).min(deadline);
        tokio::time::sleep_until(next_poll).await;
    };

    let response = capture
        .latest_assistant
        .lock()
        .expect("capture mutex poisoned")
        .clone()
        .unwrap_or_default();

    Ok(RunOutcome {
        exit,
        response,
        timed_out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn protocol_event(log_row: &str, text: &str) -> String {
        serde_json::json!({
            "type": "protocol_event",
            "stream": "stdout",
            "log_row": log_row,
            "text": text,
            "detail": "",
        })
        .to_string()
    }

    #[test]
    fn capture_keeps_latest_assistant_text() {
        let store = CaptureStore::default();
        let ts = Utc::now();
        store.insert_event(None, "x", "k", &protocol_event("assistant", "first"), ts);
        store.insert_event(None, "x", "k", &protocol_event("tool_call", "ignored"), ts);
        store.insert_event(None, "x", "k", &protocol_event("assistant", "second"), ts);
        assert_eq!(
            store.latest_assistant.lock().unwrap().as_deref(),
            Some("second")
        );
    }

    #[test]
    fn capture_ignores_non_protocol_and_empty() {
        let store = CaptureStore::default();
        let ts = Utc::now();
        store.insert_event(None, "x", "k", "not json", ts);
        store.insert_event(None, "x", "k", &protocol_event("assistant", "   "), ts);
        store.insert_event(None, "x", "k", r#"{"type":"spawn"}"#, ts);
        assert!(store.latest_assistant.lock().unwrap().is_none());
    }
}
