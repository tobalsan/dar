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
use cap_runner::{
    ExitKind, KillReason, Runner, RunnerEventSink, RunnerEventStore, SpawnParams, TurnDecision,
};
use chrono::{DateTime, Utc};
use host_api::{ServiceRegistry, ShutdownToken};

/// Outcome of one runner fire.
pub enum RunOutcome {
    /// The run completed (or was killed): carries the exit classification and
    /// the captured assistant text.
    Completed(ExitKind, String),
    /// The scheduler's own timeout elapsed; the child was killed. The run is
    /// recorded as an error regardless of how the child exited.
    TimedOut,
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
    text_deltas: Mutex<String>,
    latest_error: Mutex<Option<String>>,
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
        match value.get("type").and_then(|t| t.as_str()) {
            Some("protocol_event") => {
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
            Some("text_delta") => {
                let Some(text) = value.get("text").and_then(|t| t.as_str()) else {
                    return;
                };
                if !text.is_empty() {
                    self.text_deltas
                        .lock()
                        .expect("capture mutex poisoned")
                        .push_str(text);
                }
            }
            Some("error") => {
                let Some(message) = value.get("message").and_then(|t| t.as_str()) else {
                    return;
                };
                let message = message.trim();
                if !message.is_empty() {
                    *self.latest_error.lock().expect("capture mutex poisoned") =
                        Some(message.to_string());
                }
            }
            _ => {}
        }
    }
}

/// All parameters for a single scheduler runner fire.
pub struct FireRunnerRequest<'a> {
    pub runner_kind: &'a str,
    pub runner_command: &'a str,
    pub runner_model: Option<String>,
    pub runner_provider: Option<String>,
    pub workspace: &'a std::path::Path,
    pub workspace_root: &'a std::path::Path,
    pub agent_root: &'a std::path::Path,
    pub prompt: String,
    pub job_id: &'a str,
    pub max_run_timeout_ms: u64,
    pub job_timeout: Duration,
}

/// Resolve the named runner service (`runner.use`, default `pi`) and fire it
/// once with `prompt` as the agent prompt. Blocks until the run completes,
/// returning its exit classification and captured assistant text.
pub async fn fire_runner(
    req: FireRunnerRequest<'_>,
    services: &ServiceRegistry,
    mut shutdown: ShutdownToken,
) -> Result<RunOutcome> {
    let FireRunnerRequest {
        runner_kind,
        runner_command,
        runner_model,
        runner_provider,
        workspace,
        workspace_root,
        agent_root,
        prompt,
        job_id,
        max_run_timeout_ms,
        job_timeout,
    } = req;
    let runner = services
        .get_named::<dyn Runner>(runner_kind)
        .with_context(|| format!("resolving runner service {runner_kind:?}"))?;

    let capture = Arc::new(CaptureStore::default());
    let events: Arc<dyn RunnerEventSink> = Arc::new(NullSink);
    let store: Arc<dyn RunnerEventStore> = capture.clone();
    let last_event_at = Arc::new(std::sync::Mutex::new(Utc::now()));

    if shutdown.is_cancelled() {
        anyhow::bail!("scheduler shutdown before runner spawn");
    }

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
    .model(runner_model)
    .provider(runner_provider)
    .host_tool_bridge(runner_core::host_tool_bridge(services, agent_root))
    .build();

    let mut handle = tokio::select! {
        _ = shutdown.cancelled() => anyhow::bail!("scheduler shutdown before runner spawn"),
        handle = runner.spawn(params) => handle.context("spawning runner")?,
    };

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
        // Scheduler runs are single-shot: on the first turn boundary, tell a
        // turn-capable child to finish so it quits cleanly instead of parking
        // until job_timeout kills it (false timeout). No-op for turn-opt-out
        // handles (cli/fake), whose supports_turns() is false.
        if handle.supports_turns() && handle.try_recv_turn_ended() {
            handle.send_turn_decision(TurnDecision::Finish);
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
        tokio::select! {
            _ = shutdown.cancelled() => {
                tracing::warn!("[scheduler] Shutdown requested; killing runner for job {job_id}");
                let exit = handle.request_kill_and_wait(KillReason::OperatorStop).await;
                break (exit, false);
            }
            _ = tokio::time::sleep_until(next_poll) => {}
        }
    };

    let response = capture
        .latest_assistant
        .lock()
        .expect("capture mutex poisoned")
        .clone()
        .unwrap_or_default();
    let deltas = capture
        .text_deltas
        .lock()
        .expect("capture mutex poisoned")
        .trim()
        .to_string();
    let response = if response.is_empty() {
        deltas
    } else {
        response
    };
    let error = capture
        .latest_error
        .lock()
        .expect("capture mutex poisoned")
        .clone()
        .unwrap_or_default();
    let text = if response.is_empty() { error } else { response };

    if timed_out {
        Ok(RunOutcome::TimedOut)
    } else {
        Ok(RunOutcome::Completed(exit, text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};

    use cap_runner::{HostToolBridge, RunnerHandle, TurnEnded};
    use serde_json::{json, Value};
    use tool_registry::{
        ToolExecutor, ToolOutcome, ToolRegistry, ToolRegistryHandle, ToolSpec,
        TOOL_REGISTRY_SERVICE,
    };

    fn shutdown_token(cancelled: bool) -> (tokio::sync::watch::Sender<bool>, ShutdownToken) {
        let (tx, rx) = tokio::sync::watch::channel(cancelled);
        (tx, ShutdownToken::new(rx))
    }

    struct CapturingRunner {
        spawns: Arc<AtomicUsize>,
        bridge: Arc<Mutex<Option<Option<HostToolBridge>>>>,
        runner_opts: Arc<Mutex<Option<(Option<String>, Option<String>)>>>,
    }

    impl Runner for CapturingRunner {
        fn spawn<'a>(
            &self,
            params: SpawnParams<'a>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<RunnerHandle>> + Send + 'a>>
        {
            self.spawns.fetch_add(1, Ordering::SeqCst);
            *self.bridge.lock().expect("bridge mutex poisoned") =
                Some(params.host_tool_bridge.clone());
            *self.runner_opts.lock().expect("runner opts mutex poisoned") =
                Some((params.model.clone(), params.provider.clone()));
            Box::pin(async move {
                let (kill_tx, _kill_rx) = tokio::sync::oneshot::channel();
                let done = tokio::spawn(async { ExitKind::Normal });
                Ok(RunnerHandle::new(0, kill_tx, done))
            })
        }
    }

    struct NoopTool;

    #[async_trait::async_trait]
    impl ToolExecutor for NoopTool {
        async fn execute(&self, _args: Value) -> Result<ToolOutcome> {
            Ok(ToolOutcome::ok("ok"))
        }
    }

    fn services_with_capture(
        registry: Option<ToolRegistry>,
    ) -> (
        ServiceRegistry,
        Arc<AtomicUsize>,
        Arc<Mutex<Option<Option<HostToolBridge>>>>,
        Arc<Mutex<Option<(Option<String>, Option<String>)>>>,
    ) {
        let spawns = Arc::new(AtomicUsize::new(0));
        let bridge = Arc::new(Mutex::new(None));
        let runner_opts = Arc::new(Mutex::new(None));
        let mut services = ServiceRegistry::default();
        services
            .service::<dyn Runner>(
                "fake",
                Arc::new(CapturingRunner {
                    spawns: Arc::clone(&spawns),
                    bridge: Arc::clone(&bridge),
                    runner_opts: Arc::clone(&runner_opts),
                }),
            )
            .unwrap();
        if let Some(registry) = registry {
            services
                .service::<dyn ToolRegistryHandle>(TOOL_REGISTRY_SERVICE, Arc::new(registry))
                .unwrap();
        }
        (services, spawns, bridge, runner_opts)
    }

    fn registry_with_tool() -> ToolRegistry {
        let registry = ToolRegistry::new();
        registry
            .register_tool(
                ToolSpec::new("noop", "noop", json!({ "type": "object" })),
                Arc::new(NoopTool),
            )
            .unwrap();
        registry
    }

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

    #[tokio::test]
    async fn host_tool_bridge_is_passed_to_scheduler_spawn_params_when_tools_exist() {
        let dir = tempfile::tempdir().unwrap();
        let (services, spawns, bridge, runner_opts) =
            services_with_capture(Some(registry_with_tool()));
        let (_tx, shutdown) = shutdown_token(false);

        fire_runner(
            FireRunnerRequest {
                runner_kind: "fake",
                runner_command: "",
                runner_model: Some("model-a".to_string()),
                runner_provider: Some("provider-a".to_string()),
                workspace: dir.path(),
                workspace_root: dir.path(),
                agent_root: dir.path(),
                prompt: "prompt".to_string(),
                job_id: "job",
                max_run_timeout_ms: 60_000,
                job_timeout: Duration::from_secs(60),
            },
            &services,
            shutdown,
        )
        .await
        .unwrap();

        assert_eq!(spawns.load(Ordering::SeqCst), 1);
        let bridge = bridge
            .lock()
            .expect("bridge mutex poisoned")
            .clone()
            .flatten();
        let bridge = bridge.expect("host tool bridge passed to runner");
        assert_eq!(bridge.args[0], "__mcp-bridge");
        assert_eq!(bridge.args[1], "--dir");
        assert_eq!(bridge.args[2], dir.path().display().to_string());
        assert_eq!(
            *runner_opts.lock().expect("runner opts mutex poisoned"),
            Some((Some("model-a".to_string()), Some("provider-a".to_string())))
        );
    }

    #[tokio::test]
    async fn host_tool_bridge_is_none_for_scheduler_spawn_params_when_registry_empty() {
        let dir = tempfile::tempdir().unwrap();
        let (services, spawns, bridge, _runner_opts) =
            services_with_capture(Some(ToolRegistry::new()));
        let (_tx, shutdown) = shutdown_token(false);

        fire_runner(
            FireRunnerRequest {
                runner_kind: "fake",
                runner_command: "",
                runner_model: None,
                runner_provider: None,
                workspace: dir.path(),
                workspace_root: dir.path(),
                agent_root: dir.path(),
                prompt: "prompt".to_string(),
                job_id: "job",
                max_run_timeout_ms: 60_000,
                job_timeout: Duration::from_secs(60),
            },
            &services,
            shutdown,
        )
        .await
        .unwrap();

        assert_eq!(spawns.load(Ordering::SeqCst), 1);
        assert_eq!(
            *bridge.lock().expect("bridge mutex poisoned"),
            Some(None),
            "empty registry keeps scheduler runner tool-blind"
        );
    }

    #[tokio::test]
    async fn shutdown_before_spawn_returns_without_calling_runner() {
        let dir = tempfile::tempdir().unwrap();
        let (services, spawns, _bridge, _runner_opts) =
            services_with_capture(Some(registry_with_tool()));
        let (_tx, shutdown) = shutdown_token(true);

        let err = match fire_runner(
            FireRunnerRequest {
                runner_kind: "fake",
                runner_command: "",
                runner_model: None,
                runner_provider: None,
                workspace: dir.path(),
                workspace_root: dir.path(),
                agent_root: dir.path(),
                prompt: "prompt".to_string(),
                job_id: "job",
                max_run_timeout_ms: 60_000,
                job_timeout: Duration::from_secs(60),
            },
            &services,
            shutdown,
        )
        .await
        {
            Ok(_) => panic!("fire_runner unexpectedly spawned after shutdown"),
            Err(err) => err,
        };

        assert!(err.to_string().contains("shutdown before runner spawn"));
        assert_eq!(spawns.load(Ordering::SeqCst), 0);
    }

    struct KillCapturingRunner {
        kill_reason: Arc<Mutex<Option<KillReason>>>,
        spawned: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    }

    impl Runner for KillCapturingRunner {
        fn spawn<'a>(
            &self,
            _params: SpawnParams<'a>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<RunnerHandle>> + Send + 'a>>
        {
            if let Some(tx) = self.spawned.lock().expect("spawned mutex poisoned").take() {
                let _ = tx.send(());
            }
            let kill_reason = Arc::clone(&self.kill_reason);
            Box::pin(async move {
                let (kill_tx, kill_rx) = tokio::sync::oneshot::channel();
                let done = tokio::spawn(async move {
                    match kill_rx.await {
                        Ok(reason) => {
                            *kill_reason.lock().expect("kill mutex poisoned") = Some(reason);
                            ExitKind::Interrupted { reason: "killed" }
                        }
                        Err(_) => ExitKind::Normal,
                    }
                });
                Ok(RunnerHandle::new(0, kill_tx, done))
            })
        }
    }

    #[tokio::test]
    async fn shutdown_kills_running_runner_with_operator_stop() {
        let dir = tempfile::tempdir().unwrap();
        let kill_reason = Arc::new(Mutex::new(None));
        let (spawned_tx, spawned_rx) = tokio::sync::oneshot::channel();
        let mut services = ServiceRegistry::default();
        services
            .service::<dyn Runner>(
                "fake",
                Arc::new(KillCapturingRunner {
                    kill_reason: Arc::clone(&kill_reason),
                    spawned: Mutex::new(Some(spawned_tx)),
                }),
            )
            .unwrap();
        let (shutdown_tx, shutdown) = shutdown_token(false);
        let root = dir.path().to_path_buf();
        let services_for_task = services.clone();

        let run = tokio::spawn(async move {
            fire_runner(
                FireRunnerRequest {
                    runner_kind: "fake",
                    runner_command: "",
                    runner_model: None,
                    runner_provider: None,
                    workspace: &root,
                    workspace_root: &root,
                    agent_root: &root,
                    prompt: "prompt".to_string(),
                    job_id: "job",
                    max_run_timeout_ms: 60_000,
                    job_timeout: Duration::from_secs(60),
                },
                &services_for_task,
                shutdown,
            )
            .await
        });

        spawned_rx.await.unwrap();
        shutdown_tx.send(true).unwrap();
        let outcome = run.await.unwrap().unwrap();

        assert!(matches!(
            outcome,
            RunOutcome::Completed(ExitKind::Interrupted { .. }, _)
        ));
        assert!(matches!(
            *kill_reason.lock().expect("kill mutex poisoned"),
            Some(KillReason::OperatorStop)
        ));
    }

    /// Turn-capable mock: parks at one turn boundary (emits `TurnEnded`) and
    /// only resolves `done` with `ExitKind::Normal` once it receives
    /// `TurnDecision::Finish` — mirroring pi/codex/opencode's long-lived child.
    /// If the decision never arrives it would hang until the scheduler kills it.
    struct TurnCapableRunner;

    impl Runner for TurnCapableRunner {
        fn spawn<'a>(
            &self,
            _params: SpawnParams<'a>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<RunnerHandle>> + Send + 'a>>
        {
            Box::pin(async move {
                let (kill_tx, mut kill_rx) = tokio::sync::oneshot::channel::<KillReason>();
                let (ended_tx, ended_rx) = tokio::sync::mpsc::unbounded_channel();
                let (decision_tx, mut decision_rx) = tokio::sync::mpsc::unbounded_channel();
                // Signal the turn boundary right away.
                ended_tx.send(TurnEnded).unwrap();
                let done = tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            reason = &mut kill_rx => {
                                return match reason {
                                    Ok(_) => ExitKind::Interrupted { reason: "killed" },
                                    Err(_) => ExitKind::Normal,
                                };
                            }
                            decision = decision_rx.recv() => {
                                match decision {
                                    Some(TurnDecision::Finish) | None => return ExitKind::Normal,
                                    Some(TurnDecision::Continue { .. }) => continue,
                                }
                            }
                        }
                    }
                });
                Ok(RunnerHandle::with_turns(
                    0,
                    kill_tx,
                    done,
                    ended_rx,
                    decision_tx,
                ))
            })
        }
    }

    #[tokio::test]
    async fn turn_capable_runner_finishes_on_its_own_before_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let mut services = ServiceRegistry::default();
        services
            .service::<dyn Runner>("fake", Arc::new(TurnCapableRunner))
            .unwrap();
        let (_tx, shutdown) = shutdown_token(false);

        let outcome = fire_runner(
            FireRunnerRequest {
                runner_kind: "fake",
                runner_command: "",
                workspace: dir.path(),
                workspace_root: dir.path(),
                agent_root: dir.path(),
                prompt: "prompt".to_string(),
                job_id: "job",
                max_run_timeout_ms: 60_000,
                // Generous deadline: the run must finish via `Finish`, not by
                // hitting this timeout.
                job_timeout: Duration::from_secs(60),
            },
            &services,
            shutdown,
        )
        .await
        .unwrap();

        assert!(
            matches!(outcome, RunOutcome::Completed(ExitKind::Normal, _)),
            "turn-capable scheduler run should finish Normal via Finish, not TimedOut"
        );
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
    fn capture_accumulates_text_deltas() {
        let store = CaptureStore::default();
        let ts = Utc::now();
        store.insert_event(None, "x", "k", r#"{"type":"text_delta","text":"hel"}"#, ts);
        store.insert_event(None, "x", "k", r#"{"type":"text_delta","text":"lo"}"#, ts);
        assert_eq!(store.text_deltas.lock().unwrap().as_str(), "hello");
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
