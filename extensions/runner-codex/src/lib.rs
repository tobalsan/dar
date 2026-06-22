//! Codex runner extension — drives `codex app-server` via JSON-RPC 2.0.

use std::ffi::OsString;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use std::collections::HashSet;

use anyhow::{Context, Result};
use cap_runner::{ExitKind, KillReason, RunnerHandle, SpawnParams, TurnDecision, TurnEnded};
use host_api::{Extension, RegisterCtx};
use runner_core::{
    classify_protocol_line, common_env, effective_command, log_ev, scrub_loaded_env,
    setup_process_group, spawn_line_pump, strip_ansi, term_then_kill,
};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

const EVENT_KIND: &str = "runner.codex";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// SIGTERM-to-SIGKILL grace passed to `term_then_kill` on shutdown/kill.
const KILL_GRACE: Duration = Duration::from_secs(5);

pub struct RunnerCodexExtension;

impl Extension for RunnerCodexExtension {
    fn id(&self) -> &'static str {
        "runner-codex"
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            ctx.services
                .service::<dyn cap_runner::Runner>("codex", Arc::new(CodexRunner))?;
            Ok(())
        })
    }
}

pub struct CodexRunner;

impl cap_runner::Runner for CodexRunner {
    fn spawn<'a>(
        &self,
        params: SpawnParams<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<RunnerHandle>> + Send + 'a>,
    > {
        Box::pin(async move { spawn_codex(params).await })
    }
}

/// Build args for `codex app-server`.
fn codex_args(p: &SpawnParams<'_>) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("app-server"),
        OsString::from("-c"),
        OsString::from("approval_policy=\"never\""),
        OsString::from("-c"),
        OsString::from("sandbox_permissions=[\"disk-full-read-access\", \"disk-write-access\"]"),
    ];
    if let Some(ref model) = p.model {
        args.push(OsString::from("-c"));
        args.push(OsString::from(format!("model={model:?}")));
    }
    if let Some(ref provider) = p.provider {
        args.push(OsString::from("-c"));
        args.push(OsString::from(format!("model_provider={provider:?}")));
    }
    if let Some(effort) = p
        .thinking
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        args.push(OsString::from("-c"));
        args.push(OsString::from(format!("model_reasoning_effort={effort:?}")));
    }
    // Wire the host MCP bridge as a codex MCP server on the same app-server
    // invocation, so the agent can call host-registered extension tools and the
    // result returns inside the same thread/turn.
    if let Some(bridge) = &p.host_tool_bridge {
        args.extend(runner_core::codex_mcp_bridge_args(bridge));
    }
    args
}

// ---------------------------------------------------------------------------
// JSON-RPC request builders (for unit testing)
// ---------------------------------------------------------------------------

fn make_initialize(id: u64, workspace: &str) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "clientInfo": { "name": "agentropy", "version": "0.1.0" },
            "capabilities": { "experimentalApi": true },
            "cwd": workspace,
        }
    })
}

fn make_initialized() -> Value {
    serde_json::json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} })
}

fn make_thread_start(id: u64, workspace: &str, model: Option<&str>) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "thread/start",
        "params": {
            "model": model,
            "cwd": workspace,
            "approvalPolicy": "never",
            "sandbox": "danger-full-access",
            "serviceName": "agentropy",
        }
    })
}

fn make_turn_start(
    id: u64,
    thread_id: &str,
    workspace: &str,
    prompt: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "turn/start",
        "params": {
            "threadId": thread_id,
            "input": [{ "type": "text", "text": prompt }],
            "cwd": workspace,
            "model": model,
            "approvalPolicy": "never",
            "sandboxPolicy": { "type": "dangerFullAccess" },
            "effort": effort,
        }
    })
}

// ---------------------------------------------------------------------------
// Core spawn logic
// ---------------------------------------------------------------------------

async fn spawn_codex(p: SpawnParams<'_>) -> Result<RunnerHandle> {
    host_api::assert_contained(p.workspace_root, p.workspace)
        .context("workspace containment check failed; refusing to spawn child")?;

    let command = effective_command(p.command, "codex");
    let args = codex_args(&p);

    let mut cmd = Command::new(&command);
    for arg in &args {
        cmd.arg(arg);
    }
    scrub_loaded_env(&mut cmd);
    cmd.envs(common_env(&p));
    cmd.current_dir(p.workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    setup_process_group(&mut cmd);

    let mut child = cmd.spawn().with_context(|| {
        format!(
            "spawning `{}` in {}",
            command.to_string_lossy(),
            p.workspace.display()
        )
    })?;

    let pid = child
        .id()
        .context("child has no pid immediately after spawn")?;

    log_ev(
        &p.issue_id,
        "spawn",
        &format!(
            "runner={} pid={pid} cwd={}",
            p.runner_kind,
            p.workspace.display()
        ),
    );

    // Persist spawn event.
    {
        let payload = serde_json::json!({
            "type": "spawn",
            "runner": p.runner_kind,
            "pid": pid,
            "command": command.to_string_lossy(),
            "args": args.iter().map(|a| a.to_string_lossy().to_string()).collect::<Vec<_>>(),
            "session_dir": serde_json::Value::Null,
        })
        .to_string();
        p.store.insert_event(
            Some(&p.run_id),
            &p.issue_id,
            EVENT_KIND,
            &payload,
            chrono::Utc::now(),
        );
    }

    // Take stdio handles.
    let mut stdin = child.stdin.take().context("no stdin handle after spawn")?;
    let stdout = child
        .stdout
        .take()
        .context("no stdout handle after spawn")?;
    let stderr = child
        .stderr
        .take()
        .context("no stderr handle after spawn")?;

    // Wire up stderr pump immediately (unchanged behavior).
    spawn_line_pump(
        stderr,
        p.issue_id.clone(),
        p.run_id.clone(),
        "stderr",
        EVENT_KIND,
        Arc::clone(&p.events),
        Arc::clone(&p.store),
        Arc::clone(&p.last_event_at),
        log_ev,
    );

    let (kill_tx, kill_rx) = oneshot::channel::<KillReason>();
    let (ended_tx, ended_rx) = tokio::sync::mpsc::unbounded_channel::<TurnEnded>();
    let (decision_tx, decision_rx) = tokio::sync::mpsc::unbounded_channel::<TurnDecision>();
    let timeout = Duration::from_millis(p.max_run_timeout_ms);

    // Clone what the async task needs.
    let issue_id = p.issue_id.clone();
    let run_id = p.run_id.clone();
    let workspace_str = p.workspace.to_string_lossy().into_owned();
    let model = p.model.clone();
    let effort = p.thinking.clone();
    let prompt = p.prompt.clone();
    let events = Arc::clone(&p.events);
    let store = Arc::clone(&p.store);
    let last_event_at = Arc::clone(&p.last_event_at);

    let done = tokio::spawn(async move {
        let result = drive_protocol(
            pid,
            &mut stdin,
            stdout,
            timeout,
            kill_rx,
            ended_tx,
            decision_rx,
            &issue_id,
            &run_id,
            &workspace_str,
            model.as_deref(),
            effort.as_deref(),
            &prompt,
            events,
            store,
            last_event_at,
        )
        .await;

        // Close stdin regardless.
        let _ = stdin.shutdown().await;
        result
    });

    Ok(RunnerHandle::with_turns(
        pid,
        kill_tx,
        done,
        ended_rx,
        decision_tx,
    ))
}

// ---------------------------------------------------------------------------
// Protocol driver
// ---------------------------------------------------------------------------

/// Drive the full JSON-RPC handshake and turn loop, then supervise the child.
/// The hard `timeout` ceiling and the `kill_rx` channel preempt the inner turn
/// loop in EVERY state — including while idle awaiting a [`TurnDecision`].
#[allow(clippy::too_many_arguments)]
async fn drive_protocol(
    pid: u32,
    stdin: &mut tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
    timeout: Duration,
    kill_rx: oneshot::Receiver<KillReason>,
    ended_tx: UnboundedSender<TurnEnded>,
    mut decision_rx: UnboundedReceiver<TurnDecision>,
    issue_id: &str,
    run_id: &str,
    workspace: &str,
    model: Option<&str>,
    effort: Option<&str>,
    prompt: &str,
    events: Arc<dyn cap_runner::RunnerEventSink>,
    store: Arc<dyn cap_runner::RunnerEventStore>,
    last_event_at: Arc<Mutex<chrono::DateTime<chrono::Utc>>>,
) -> ExitKind {
    // The timeout and kill channel wrap the entire protocol exchange and turn
    // loop. Awaiting a decision is just one suspension point inside the inner
    // future, so both still preempt it.
    tokio::select! {
        kind = run_protocol_inner(
            pid, stdin, stdout, &ended_tx, &mut decision_rx, issue_id, run_id, workspace,
            model, effort, prompt, events, store, last_event_at,
        ) => kind,
        _ = tokio::time::sleep(timeout) => {
            log_ev(issue_id, "timeout", "turn_timeout_ms exceeded; killing");
            term_then_kill(pid, KILL_GRACE);
            ExitKind::Interrupted { reason: "turn_timeout" }
        }
        reason = kill_rx => {
            match reason {
                Ok(KillReason::Timeout) => log_ev(issue_id, "kill", "reason=timeout"),
                Ok(KillReason::OperatorStop) => log_ev(issue_id, "kill", "reason=operator_stop"),
                Ok(KillReason::Reconcile) => log_ev(issue_id, "kill", "reason=reconcile"),
                Err(_) => log_ev(issue_id, "kill", "handle dropped"),
            }
            term_then_kill(pid, KILL_GRACE);
            ExitKind::Interrupted { reason: "killed" }
        }
    }
}

/// Inner protocol: handshake + turn loop. Returns ExitKind.
#[allow(clippy::too_many_arguments)]
async fn run_protocol_inner(
    pid: u32,
    stdin: &mut tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
    ended_tx: &UnboundedSender<TurnEnded>,
    decision_rx: &mut UnboundedReceiver<TurnDecision>,
    issue_id: &str,
    run_id: &str,
    workspace: &str,
    model: Option<&str>,
    effort: Option<&str>,
    prompt: &str,
    events: Arc<dyn cap_runner::RunnerEventSink>,
    store: Arc<dyn cap_runner::RunnerEventStore>,
    last_event_at: Arc<Mutex<chrono::DateTime<chrono::Utc>>>,
) -> ExitKind {
    let mut next_id: u64 = 1;
    let mut lines = BufReader::new(stdout).lines();

    // Helper: send a line to stdin.
    macro_rules! send {
        ($val:expr) => {{
            let line = $val.to_string() + "\n";
            if let Err(e) = stdin.write_all(line.as_bytes()).await {
                log_ev(issue_id, "error", &format!("stdin write failed: {e}"));
                term_then_kill(pid, Duration::from_secs(5));
                return ExitKind::Abnormal(None);
            }
        }};
    }

    // Helper: send a request and wait for its response with a 30s timeout.
    // Returns Some(result_value) or None on error/timeout.
    macro_rules! request {
        ($val:expr, $id:expr) => {{
            send!($val);
            match wait_for_response(
                &mut lines,
                $id,
                issue_id,
                run_id,
                &events,
                &store,
                &last_event_at,
            )
            .await
            {
                Some(result) => result,
                None => {
                    term_then_kill(pid, Duration::from_secs(5));
                    return ExitKind::Abnormal(None);
                }
            }
        }};
    }

    // 1. initialize
    let init_id = next_id;
    next_id += 1;
    let _ = request!(make_initialize(init_id, workspace), init_id);

    // 2. initialized (notification, no response expected)
    send!(make_initialized());

    // 3. thread/start
    let thread_id_req = next_id;
    next_id += 1;
    let thread_result = request!(
        make_thread_start(thread_id_req, workspace, model),
        thread_id_req
    );
    let thread_id = match extract_thread_id(&thread_result) {
        Some(id) => id,
        None => {
            log_ev(
                issue_id,
                "error",
                &format!("thread/start: no thread.id in result: {thread_result}"),
            );
            term_then_kill(pid, Duration::from_secs(5));
            return ExitKind::Abnormal(None);
        }
    };

    // 4. turn/start (initial turn). Fire-and-pump: the turn loop below tracks
    // the in-flight set and reacts to the turn/started + turn/completed
    // notifications, so we do not block on the request response here.
    let turn_id_req = next_id;
    next_id += 1;
    send!(make_turn_start(
        turn_id_req,
        &thread_id,
        workspace,
        prompt,
        model,
        effort
    ));

    // 5. Turn loop. We keep ONE in-flight set keyed by (threadId, turnId) across
    // ALL threads (the main thread plus any collab subagent threads). The run is
    // never "done" at the protocol level — a turn boundary is "in-flight set just
    // became empty", at which point we report a `TurnEnded` and ask the
    // orchestrator (the state machine) what to do next.
    //
    // States:
    //   - `Busy`: in-flight set non-empty; pump notifications.
    //   - `Idle`: in-flight set empty; `TurnEnded` was sent; pump notifications
    //     AND await a `TurnDecision` concurrently. A new `turn/started` here
    //     (forwarded subagent completion injects a new parent turn) makes the
    //     pending boundary stale → back to `Busy`; any `Continue` that then
    //     arrives mid-turn is swallowed (never send `turn/start` mid-turn).
    let mut in_flight: HashSet<(String, String)> = HashSet::new();
    // Some(prompt) once a `Continue` arrives while idle and we are ready to feed
    // it. We never feed a continuation while turns are in flight.
    let mut idle = false; // a `TurnEnded` is outstanding (we are awaiting a decision)

    loop {
        if idle {
            // Idle: race a stdout line against a decision. We must keep pumping
            // so a forwarded subagent completion (new parent turn/started) can
            // make this boundary stale before we act on a decision.
            tokio::select! {
                line = lines.next_line() => {
                    match handle_pump_line(
                        line, stdin, &mut in_flight, &thread_id,
                        issue_id, run_id, &events, &store, &last_event_at,
                    ).await {
                        PumpStep::Continue => {
                            if !in_flight.is_empty() {
                                // A new turn started: the boundary is stale.
                                idle = false;
                            }
                        }
                        PumpStep::BoundaryReached => {
                            // Already idle; a redundant empty transition. Stay idle.
                        }
                        PumpStep::Exited => {
                            term_then_kill(pid, KILL_GRACE);
                            return ExitKind::Normal;
                        }
                        PumpStep::MainTurnFailed | PumpStep::ProtocolError => {
                            term_then_kill(pid, KILL_GRACE);
                            return ExitKind::Abnormal(None);
                        }
                    }
                }
                decision = decision_rx.recv() => {
                    match decision {
                        Some(TurnDecision::Continue { prompt }) => {
                            if in_flight.is_empty() {
                                // Genuinely idle: feed the continuation turn.
                                let cont_id = next_id;
                                next_id += 1;
                                send!(make_turn_start(cont_id, &thread_id, workspace, &prompt, model, effort));
                                idle = false;
                            } else {
                                // Boundary went stale (a new turn started before
                                // the decision arrived): swallow it. A fresh
                                // `TurnEnded` is emitted when the set empties.
                                log_ev(issue_id, "turn", "stale Continue swallowed (turn in flight)");
                                idle = false;
                            }
                        }
                        Some(TurnDecision::Finish) | None => {
                            log_ev(issue_id, "finish", "graceful shutdown");
                            term_then_kill(pid, KILL_GRACE);
                            return ExitKind::Normal;
                        }
                    }
                }
            }
        } else {
            // Busy: pump notifications until the in-flight set empties.
            let line = lines.next_line().await;
            match handle_pump_line(
                line,
                stdin,
                &mut in_flight,
                &thread_id,
                issue_id,
                run_id,
                &events,
                &store,
                &last_event_at,
            )
            .await
            {
                PumpStep::Continue => {}
                PumpStep::BoundaryReached => {
                    // In-flight set just emptied: report idle and park.
                    if ended_tx.send(TurnEnded).is_err() {
                        log_ev(issue_id, "finish", "handle dropped; graceful shutdown");
                        term_then_kill(pid, KILL_GRACE);
                        return ExitKind::Normal;
                    }
                    idle = true;
                }
                PumpStep::Exited => {
                    term_then_kill(pid, KILL_GRACE);
                    return ExitKind::Normal;
                }
                PumpStep::MainTurnFailed | PumpStep::ProtocolError => {
                    term_then_kill(pid, KILL_GRACE);
                    return ExitKind::Abnormal(None);
                }
            }
        }
    }
}

/// Wait for a JSON-RPC response matching `expected_id`. While waiting, pass
/// through any stdout lines to events/store (they are not yet protocol-pumped).
/// Returns `Some(result)` on success, `None` on timeout or error response.
async fn wait_for_response(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    expected_id: u64,
    issue_id: &str,
    run_id: &str,
    events: &Arc<dyn cap_runner::RunnerEventSink>,
    store: &Arc<dyn cap_runner::RunnerEventStore>,
    last_event_at: &Arc<Mutex<chrono::DateTime<chrono::Utc>>>,
) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + REQUEST_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            log_ev(
                issue_id,
                "error",
                &format!("request {expected_id} timed out after 30s"),
            );
            return None;
        }
        let line = match tokio::time::timeout(remaining, lines.next_line()).await {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => {
                log_ev(issue_id, "error", "stdout EOF while awaiting response");
                return None;
            }
            Ok(Err(e)) => {
                log_ev(issue_id, "error", &format!("stdout read error: {e}"));
                return None;
            }
            Err(_) => {
                log_ev(
                    issue_id,
                    "error",
                    &format!("request {expected_id} timed out after 30s"),
                );
                return None;
            }
        };

        // Emit to events/store unconditionally.
        emit_line(
            issue_id,
            run_id,
            &line,
            "stdout",
            events,
            store,
            last_event_at,
        );

        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Check if this is the response we're waiting for.
        if let Some(id) = msg.get("id").and_then(|v| v.as_u64()) {
            if id == expected_id {
                if let Some(err) = msg.get("error") {
                    log_ev(
                        issue_id,
                        "error",
                        &format!("request {expected_id} failed: {err}"),
                    );
                    return None;
                }
                return Some(msg.get("result").cloned().unwrap_or(Value::Null));
            }
        }
        // Not our response — continue reading.
    }
}

/// What processing one pumped stdout line tells the turn loop to do next.
enum PumpStep {
    /// Ordinary line (delta, server request answered, subagent event). The
    /// in-flight set may have changed but is still non-empty.
    Continue,
    /// A `turn/completed` removal just emptied the in-flight set — this is a turn
    /// boundary.
    BoundaryReached,
    /// stdout EOF / read error: the child is exiting on its own.
    Exited,
    /// The MAIN thread's turn completed with a failed/errored status (abnormal).
    MainTurnFailed,
    /// An unrecoverable protocol error notification.
    ProtocolError,
}

/// Read+process one stdout line: emit it, answer server requests, and maintain
/// the cross-thread in-flight `(threadId, turnId)` set.
///
/// - `turn/started` → insert; `turn/completed` (any status) → remove.
/// - A MAIN-thread `turn/completed` with a non-success status → `MainTurnFailed`.
/// - A subagent-thread failed turn is logged and removed but does NOT end the
///   run (the tracker-driven loop decides).
/// - Removal that empties the set → `BoundaryReached`.
#[allow(clippy::too_many_arguments)]
async fn handle_pump_line(
    line: std::io::Result<Option<String>>,
    stdin: &mut tokio::process::ChildStdin,
    in_flight: &mut HashSet<(String, String)>,
    main_thread_id: &str,
    issue_id: &str,
    run_id: &str,
    events: &Arc<dyn cap_runner::RunnerEventSink>,
    store: &Arc<dyn cap_runner::RunnerEventStore>,
    last_event_at: &Arc<Mutex<chrono::DateTime<chrono::Utc>>>,
) -> PumpStep {
    let line = match line {
        Ok(Some(line)) => line,
        Ok(None) => {
            log_ev(issue_id, "exit", "stdout EOF");
            return PumpStep::Exited;
        }
        Err(e) => {
            log_ev(issue_id, "error", &format!("stdout read error: {e}"));
            return PumpStep::Exited;
        }
    };

    emit_line(
        issue_id,
        run_id,
        &line,
        "stdout",
        events,
        store,
        last_event_at,
    );

    let msg: Value = match serde_json::from_str(&line) {
        Ok(v) => v,
        Err(_) => return PumpStep::Continue,
    };

    let method = msg.get("method").and_then(|v| v.as_str());
    let id = msg.get("id");
    let has_id = id.map(|v| v.is_string() || v.is_number()).unwrap_or(false);

    // Server request (has both id and method).
    if let (true, Some(server_id), Some(method_str)) = (has_id, id.cloned(), method) {
        if method_str == "item/commandExecution/requestApproval"
            || method_str == "item/fileChange/requestApproval"
        {
            // Respond with "cancel".
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": server_id,
                "result": "cancel"
            });
            let resp_line = response.to_string() + "\n";
            let _ = stdin.write_all(resp_line.as_bytes()).await;
        } else {
            // Unsupported server request → method not found error.
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": server_id,
                "error": { "code": -32601, "message": format!("method not found: {method_str}") }
            });
            let resp_line = response.to_string() + "\n";
            let _ = stdin.write_all(resp_line.as_bytes()).await;
        }
        return PumpStep::Continue;
    }

    // Notification: no id.
    if !has_id {
        match method {
            Some("turn/started") => {
                if let Some(key) = turn_key(&msg) {
                    in_flight.insert(key);
                }
            }
            Some("turn/completed") => {
                let thread_id = notification_thread_id(&msg);
                let status = msg
                    .get("params")
                    .and_then(|p| p.get("turn"))
                    .and_then(|t| t.get("status"))
                    .and_then(|s| s.as_str());
                if let Some(key) = turn_key(&msg) {
                    in_flight.remove(&key);
                }
                let is_main = thread_id.as_deref() == Some(main_thread_id);
                if status != Some("completed") {
                    if is_main {
                        log_ev(
                            issue_id,
                            "error",
                            &format!("main turn/completed with non-success status: {status:?}"),
                        );
                        return PumpStep::MainTurnFailed;
                    } else {
                        // Subagent failure: log and let the loop continue. The
                        // tracker-driven loop decides what to do.
                        log_ev(
                            issue_id,
                            "turn",
                            &format!(
                                "subagent turn/completed status={status:?} thread={thread_id:?}"
                            ),
                        );
                    }
                }
                if in_flight.is_empty() {
                    return PumpStep::BoundaryReached;
                }
            }
            Some("error") => {
                let msg_str = msg.get("params").map(|v| v.to_string()).unwrap_or_default();
                log_ev(
                    issue_id,
                    "error",
                    &format!("server error notification: {msg_str}"),
                );
                return PumpStep::ProtocolError;
            }
            _ => {}
        }
    }

    PumpStep::Continue
}

/// The threadId carried by a `turn/started` / `turn/completed` notification.
fn notification_thread_id(msg: &Value) -> Option<String> {
    msg.get("params")
        .and_then(|p| p.get("threadId"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// The `(threadId, turnId)` key for a turn notification, if both are present.
fn turn_key(msg: &Value) -> Option<(String, String)> {
    let thread_id = notification_thread_id(msg)?;
    let turn_id = msg
        .get("params")
        .and_then(|p| p.get("turn"))
        .and_then(|t| t.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())?;
    Some((thread_id, turn_id))
}

/// Return `true` when `line` is a JSON-RPC notification whose `method` ends
/// with `/delta` (or contains `/delta/`). These are streaming token chunks:
/// the final assembled text arrives in a separate `item/completed` line, so
/// deltas carry no information worth logging or storing.
fn is_delta_notification(line: &str) -> bool {
    if let Ok(v) = serde_json::from_str::<Value>(line) {
        if let Some(method) = v.get("method").and_then(|m| m.as_str()) {
            return method.ends_with("/delta") || method.contains("/delta/");
        }
    }
    false
}

/// Emit one stdout line to events + store.
///
/// - Delta notifications (`method` ends with `/delta`): update `last_event_at`
///   only. They are the liveness signal during long quiet turns but add no
///   information to logs or the dashboard Events tab.
/// - Non-delta lines with an empty classified `row_type`: push to the event
///   sink and log, but skip the store insert (no blank cards, no DB bloat).
/// - All other lines: full path — sink, liveness, log, and store.
fn emit_line(
    issue_id: &str,
    run_id: &str,
    line: &str,
    stream: &'static str,
    events: &Arc<dyn cap_runner::RunnerEventSink>,
    store: &Arc<dyn cap_runner::RunnerEventStore>,
    last_event_at: &Arc<Mutex<chrono::DateTime<chrono::Utc>>>,
) {
    let ts = chrono::Utc::now();
    let clean = strip_ansi(line);

    // Deltas are only a liveness heartbeat; skip log/sink/store entirely.
    if is_delta_notification(&clean) {
        if let Ok(mut t) = last_event_at.lock() {
            *t = ts;
        }
        return;
    }

    let pl = classify_protocol_line(stream, &clean);
    let formatted = format!("child[{issue_id}]: {clean}");
    events.push(formatted);
    if let Ok(mut t) = last_event_at.lock() {
        *t = ts;
    }
    log_ev(issue_id, stream, &clean);

    // Skip the store for lines that classify to an empty row_type — these are
    // protocol noise (handshake responses, intermediate notifications) that
    // would produce blank cards in the dashboard Events tab.
    if pl.row_type.is_empty() {
        return;
    }

    let payload = serde_json::json!({
        "type": "protocol_event",
        "stream": stream,
        "log_row": pl.row_type,
        "text": pl.text,
        "detail": pl.detail,
    })
    .to_string();
    store.insert_event(Some(run_id), issue_id, EVENT_KIND, &payload, ts);
}

fn extract_thread_id(result: &Value) -> Option<String> {
    result
        .get("thread")
        .and_then(|t| t.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    struct NullSink;
    impl cap_runner::RunnerEventSink for NullSink {
        fn push(&self, _line: String) {}
    }

    struct NullStore;
    impl cap_runner::RunnerEventStore for NullStore {
        fn insert_event(
            &self,
            _run_id: Option<&str>,
            _issue_identifier: &str,
            _kind: &'static str,
            _payload: &str,
            _ts: DateTime<Utc>,
        ) {
        }
    }

    /// Event sink that records every pushed line.
    #[derive(Default)]
    struct RecordingSink {
        lines: Mutex<Vec<String>>,
    }
    impl cap_runner::RunnerEventSink for RecordingSink {
        fn push(&self, line: String) {
            self.lines.lock().unwrap().push(line);
        }
    }
    impl RecordingSink {
        fn count(&self) -> usize {
            self.lines.lock().unwrap().len()
        }
    }

    /// Event store that counts insert_event calls.
    #[derive(Default)]
    struct CountingStore {
        count: Mutex<usize>,
    }
    impl cap_runner::RunnerEventStore for CountingStore {
        fn insert_event(
            &self,
            _run_id: Option<&str>,
            _issue_identifier: &str,
            _kind: &'static str,
            _payload: &str,
            _ts: DateTime<Utc>,
        ) {
            *self.count.lock().unwrap() += 1;
        }
    }
    impl CountingStore {
        fn count(&self) -> usize {
            *self.count.lock().unwrap()
        }
    }

    fn params<'a>(
        model: Option<String>,
        workspace: &'a Path,
        workspace_root: &'a Path,
    ) -> SpawnParams<'a> {
        SpawnParams::builder(
            "runner",
            "codex",
            workspace,
            workspace_root,
            workspace_root.parent().unwrap_or(workspace_root),
            String::new(),
            "ISSUE-1".to_string(),
            "ISSUE-1-test".to_string(),
            1000,
            Arc::new(NullSink),
            Arc::new(NullStore),
            Arc::new(Mutex::new(Utc::now())),
        )
        .model(model)
        .build()
    }

    fn arg_strings(args: Vec<OsString>) -> Vec<String> {
        args.into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    // --- args construction tests (preserved) ---

    #[test]
    fn codex_args_include_app_server_and_defaults() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let p = params(Some("codex-1".to_string()), workspace, workspace_root);
        let args = arg_strings(codex_args(&p));

        assert_eq!(args[0], "app-server");
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-c" && w[1].contains("approval_policy")),
            "approval_policy flag missing: {args:?}"
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-c" && w[1].contains("sandbox_permissions")),
            "sandbox_permissions flag missing: {args:?}"
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-c" && w[1].contains("model=")),
            "model -c flag missing: {args:?}"
        );
    }

    #[test]
    fn codex_args_wire_mcp_bridge_when_present() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let mut p = params(Some("codex-1".to_string()), workspace, workspace_root);
        p.host_tool_bridge = Some(cap_runner::HostToolBridge {
            command: "/opt/agentropy".to_string(),
            args: vec![
                "__mcp-bridge".to_string(),
                "--dir".to_string(),
                "/tmp/agent".to_string(),
            ],
        });
        let args = arg_strings(codex_args(&p));

        // Rides the same app-server invocation, NOT codex exec.
        assert_eq!(args[0], "app-server");
        let command_flag = args
            .windows(2)
            .find(|w| w[0] == "-c" && w[1].starts_with("mcp_servers.agentropy.command="))
            .map(|w| w[1].clone())
            .expect("mcp_servers.agentropy.command flag missing");
        assert_eq!(
            command_flag,
            "mcp_servers.agentropy.command=\"/opt/agentropy\""
        );
        let args_flag = args
            .windows(2)
            .find(|w| w[0] == "-c" && w[1].starts_with("mcp_servers.agentropy.args="))
            .map(|w| w[1].clone())
            .expect("mcp_servers.agentropy.args flag missing");
        assert_eq!(
            args_flag,
            "mcp_servers.agentropy.args=[\"__mcp-bridge\", \"--dir\", \"/tmp/agent\"]"
        );
    }

    #[test]
    fn codex_args_omit_mcp_bridge_when_absent() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let p = params(Some("codex-1".to_string()), workspace, workspace_root);
        let args = arg_strings(codex_args(&p));
        assert!(
            !args.iter().any(|a| a.contains("mcp_servers")),
            "no mcp_servers flag should be present without a bridge: {args:?}"
        );
    }

    #[test]
    fn codex_effort_is_passed_as_config_flag() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let mut p = params(Some("o3".to_string()), workspace, workspace_root);
        p.thinking = Some("high".to_string());
        let args = arg_strings(codex_args(&p));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-c" && w[1].contains("model_reasoning_effort")),
            "model_reasoning_effort flag missing: {args:?}"
        );
    }

    #[test]
    fn codex_approval_and_sandbox_always_set_even_without_model() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let p = params(None, workspace, workspace_root);
        let args = arg_strings(codex_args(&p));

        assert_eq!(args[0], "app-server");
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-c" && w[1].contains("approval_policy")),
            "approval_policy missing without model: {args:?}"
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-c" && w[1].contains("sandbox_permissions")),
            "sandbox_permissions missing without model: {args:?}"
        );
        assert!(
            !args
                .windows(2)
                .any(|w| w[0] == "-c" && w[1].starts_with("model=")),
            "model flag should be absent when unset: {args:?}"
        );
        assert!(
            !args
                .windows(2)
                .any(|w| w[0] == "-c" && w[1].contains("model_reasoning_effort")),
            "effort flag should be absent when unset: {args:?}"
        );
        assert!(
            !args
                .windows(2)
                .any(|w| w[0] == "-c" && w[1].contains("model_provider")),
            "provider flag should be absent when unset: {args:?}"
        );
    }

    #[test]
    fn codex_provider_is_passed_as_config_flag() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let mut p = params(Some("o3".to_string()), workspace, workspace_root);
        p.provider = Some("openai".to_string());
        let args = arg_strings(codex_args(&p));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-c" && w[1].contains("model_provider")),
            "model_provider flag missing: {args:?}"
        );
    }

    // --- request payload builder tests ---

    #[test]
    fn initialize_request_shape() {
        let req = make_initialize(1, "/ws/ISSUE-1");
        assert_eq!(req["jsonrpc"], "2.0");
        assert_eq!(req["id"], 1);
        assert_eq!(req["method"], "initialize");
        assert_eq!(req["params"]["clientInfo"]["name"], "agentropy");
        assert_eq!(req["params"]["capabilities"]["experimentalApi"], true);
        assert_eq!(req["params"]["cwd"], "/ws/ISSUE-1");
    }

    #[test]
    fn thread_start_request_shape() {
        let req = make_thread_start(2, "/ws/ISSUE-1", Some("o3"));
        assert_eq!(req["method"], "thread/start");
        assert_eq!(req["params"]["approvalPolicy"], "never");
        assert_eq!(req["params"]["sandbox"], "danger-full-access");
        assert_eq!(req["params"]["serviceName"], "agentropy");
        assert_eq!(req["params"]["model"], "o3");
        assert_eq!(req["params"]["cwd"], "/ws/ISSUE-1");
    }

    #[test]
    fn thread_start_null_model_when_none() {
        let req = make_thread_start(2, "/ws/ISSUE-1", None);
        assert!(req["params"]["model"].is_null());
    }

    #[test]
    fn turn_start_request_shape() {
        let req = make_turn_start(
            3,
            "t1",
            "/ws/ISSUE-1",
            "do something",
            Some("o3"),
            Some("high"),
        );
        assert_eq!(req["method"], "turn/start");
        assert_eq!(req["params"]["threadId"], "t1");
        assert_eq!(req["params"]["input"][0]["type"], "text");
        assert_eq!(req["params"]["input"][0]["text"], "do something");
        assert_eq!(req["params"]["approvalPolicy"], "never");
        assert_eq!(req["params"]["sandboxPolicy"]["type"], "dangerFullAccess");
        assert_eq!(req["params"]["model"], "o3");
        assert_eq!(req["params"]["effort"], "high");
    }

    #[test]
    fn turn_start_null_effort_when_none() {
        let req = make_turn_start(3, "t1", "/ws/ISSUE-1", "do", None, None);
        assert!(req["params"]["effort"].is_null());
        assert!(req["params"]["model"].is_null());
    }

    // --- in-flight accounting helpers ---

    fn started(thread: &str, turn: &str) -> Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "turn/started",
            "params": { "threadId": thread, "turn": { "id": turn } }
        })
    }

    fn completed(thread: &str, turn: &str, status: &str) -> Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "turn/completed",
            "params": { "threadId": thread, "turn": { "id": turn, "status": status } }
        })
    }

    #[test]
    fn turn_key_reads_thread_and_turn_id() {
        assert_eq!(
            turn_key(&started("t1", "u1")),
            Some(("t1".to_string(), "u1".to_string()))
        );
        assert_eq!(
            turn_key(&completed("ta", "ua", "completed")),
            Some(("ta".to_string(), "ua".to_string()))
        );
        // Missing threadId → no key.
        assert!(turn_key(&serde_json::json!({"params":{"turn":{"id":"u1"}}})).is_none());
    }

    #[test]
    fn notification_thread_id_distinguishes_main_from_subagent() {
        assert_eq!(
            notification_thread_id(&completed("t1", "u1", "completed")).as_deref(),
            Some("t1")
        );
        assert_eq!(
            notification_thread_id(&completed("sub", "us", "completed")).as_deref(),
            Some("sub")
        );
    }

    // --- integration tests with fake app-server (ALG-234 turn loop) ---

    use std::os::unix::fs::PermissionsExt;

    fn write_script(dir: &Path, body: &str) -> std::path::PathBuf {
        let path = dir.join("fake-codex.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Spawn the real codex driver against `script` with a fresh workspace.
    ///
    /// Retries up to 3 times on ETXTBSY (os error 26): under parallel `cargo
    /// test` a sibling fork may briefly hold the script's fd open.
    async fn spawn_against(dir: &Path, script: &Path) -> RunnerHandle {
        let workspaces = dir.join("workspaces");
        let workspace = workspaces.join("ISSUE-1");
        std::fs::create_dir_all(&workspace).unwrap();
        for attempt in 0u8..3 {
            let params = SpawnParams::builder(
                script.to_str().unwrap(),
                "codex",
                &workspace,
                &workspaces,
                dir,
                "initial prompt".to_string(),
                "ISSUE-1".to_string(),
                "run-1".to_string(),
                10_000,
                Arc::new(NullSink),
                Arc::new(NullStore),
                Arc::new(Mutex::new(Utc::now())),
            )
            .build();
            match spawn_codex(params).await {
                Ok(h) => return h,
                Err(e) if attempt < 2 && is_etxtbsy(&e) => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(e) => panic!("spawn: {e:#}"),
            }
        }
        unreachable!()
    }

    /// Returns true when `e` (or any source in its chain) is ETXTBSY (os error 26).
    fn is_etxtbsy(e: &anyhow::Error) -> bool {
        e.chain().any(|cause| {
            cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.raw_os_error() == Some(26))
        })
    }

    async fn wait_for_turn_ended(handle: &mut RunnerHandle) {
        for _ in 0..200 {
            if handle.try_recv_turn_ended() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("no TurnEnded within deadline");
    }

    /// Assert NO TurnEnded arrives within a short window.
    async fn assert_no_turn_ended_for(handle: &mut RunnerHandle, ticks: u32) {
        for _ in 0..ticks {
            assert!(
                !handle.try_recv_turn_ended(),
                "unexpected TurnEnded before in-flight set emptied"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn is_alive(pid: u32) -> bool {
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
    }

    /// Shared shell prelude: answer initialize/thread/start, log every received
    /// line to `$AGENT_WORKSPACE/received.log`, and dispatch on method. The
    /// caller appends the `turn/start` case body.
    const PRELUDE: &str = r#"set -e
log="$AGENT_WORKSPACE/received.log"
while IFS= read -r line; do
    printf '%s\n' "$line" >> "$log"
    id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/' | grep -E '^[0-9]+$' || true)
    method=$(printf '%s' "$line" | grep -o '"method":"[^"]*"' | head -1 | sed 's/"method":"//;s/"//')
    case "$method" in
        initialize)
            printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
            ;;
        thread/start)
            printf '{"jsonrpc":"2.0","id":%s,"result":{"thread":{"id":"main"}}}\n' "$id"
            ;;
        turn/start)
"#;

    /// Plain happy path: one main turn starts + completes, the driver reports a
    /// TurnEnded, and the test (acting as orchestrator) sends Finish → Normal.
    #[tokio::test(flavor = "multi_thread")]
    async fn happy_path_turn_then_finish_returns_normal() {
        let body = format!(
            "{PRELUDE}{}",
            r#"            printf '{"jsonrpc":"2.0","id":%s,"result":{"turn":{"id":"u1"}}}\n' "$id"
            printf '{"jsonrpc":"2.0","method":"turn/started","params":{"threadId":"main","turn":{"id":"u1"}}}\n'
            printf '{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"main","turn":{"id":"u1","status":"completed"}}}\n'
            ;;
    esac
done
exit 0
"#
        );
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), &body);

        let mut handle = spawn_against(dir.path(), &script).await;
        wait_for_turn_ended(&mut handle).await;
        handle.send_turn_decision(TurnDecision::Finish);
        assert_eq!(handle.wait().await, ExitKind::Normal);
    }

    /// ALG-234 regression: a collab subagent's turn/completed arrives FIRST, the
    /// main turn completes later, and a forwarded subagent completion injects a
    /// follow-up main turn. No TurnEnded fires until the in-flight set is empty,
    /// the child survives the subagent completion, and Finish yields Normal.
    #[tokio::test(flavor = "multi_thread")]
    async fn subagent_completes_first_does_not_kill_run() {
        let body = format!(
            "{PRELUDE}{}",
            r#"            printf '{"jsonrpc":"2.0","id":%s,"result":{"turn":{"id":"u1"}}}\n' "$id"
            # main turn starts, then a subagent thread starts.
            printf '{"jsonrpc":"2.0","method":"turn/started","params":{"threadId":"main","turn":{"id":"u1"}}}\n'
            printf '{"jsonrpc":"2.0","method":"turn/started","params":{"threadId":"sub","turn":{"id":"s1"}}}\n'
            # subagent completes FIRST (must NOT end the run; main still in flight).
            printf '{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"sub","turn":{"id":"s1","status":"completed"}}}\n'
            sleep 0.3
            # main completes -> set briefly empties -> boundary.
            printf '{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"main","turn":{"id":"u1","status":"completed"}}}\n'
            # forwarded subagent completion injects a follow-up main turn.
            printf '{"jsonrpc":"2.0","method":"turn/started","params":{"threadId":"main","turn":{"id":"u2"}}}\n'
            sleep 0.2
            printf '{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"main","turn":{"id":"u2","status":"completed"}}}\n'
            ;;
    esac
done
exit 0
"#
        );
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), &body);

        let mut handle = spawn_against(dir.path(), &script).await;
        let pid = handle.pid();

        // While the main turn (and the subagent) are in flight, no boundary.
        assert_no_turn_ended_for(&mut handle, 6).await;
        assert!(is_alive(pid), "child killed mid-run by subagent completion");

        // Eventually the in-flight set empties for good and we get a boundary.
        wait_for_turn_ended(&mut handle).await;
        assert!(is_alive(pid), "child died before the final boundary");

        handle.send_turn_decision(TurnDecision::Finish);
        assert_eq!(handle.wait().await, ExitKind::Normal);
    }

    /// Continue at a boundary feeds a SECOND turn/start on the same threadId into
    /// the same live child. The fake records received requests so we can assert
    /// two turn/start requests landed.
    #[tokio::test(flavor = "multi_thread")]
    async fn continue_feeds_second_turn_start_same_thread() {
        let body = format!(
            "{PRELUDE}{}",
            r#"            printf '{"jsonrpc":"2.0","id":%s,"result":{"turn":{"id":"u%s"}}}\n' "$id" "$id"
            printf '{"jsonrpc":"2.0","method":"turn/started","params":{"threadId":"main","turn":{"id":"u%s"}}}\n' "$id"
            printf '{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"main","turn":{"id":"u%s","status":"completed"}}}\n' "$id"
            ;;
    esac
done
exit 0
"#
        );
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), &body);
        let received = dir.path().join("workspaces/ISSUE-1/received.log");

        let mut handle = spawn_against(dir.path(), &script).await;
        wait_for_turn_ended(&mut handle).await;
        handle.send_turn_decision(TurnDecision::Continue {
            prompt: "second prompt".to_string(),
        });
        wait_for_turn_ended(&mut handle).await;
        handle.send_turn_decision(TurnDecision::Finish);
        assert_eq!(handle.wait().await, ExitKind::Normal);

        let log = std::fs::read_to_string(&received).unwrap();
        let starts: Vec<&str> = log
            .lines()
            .filter(|l| l.contains("\"method\":\"turn/start\""))
            .collect();
        assert_eq!(
            starts.len(),
            2,
            "expected two turn/start requests, got: {log}"
        );
        assert!(starts[0].contains("initial prompt"), "first: {}", starts[0]);
        assert!(starts[1].contains("second prompt"), "second: {}", starts[1]);
        // Both turn/start requests target the same main threadId.
        assert!(
            starts[1].contains("\"threadId\":\"main\""),
            "second: {}",
            starts[1]
        );
    }

    /// Kill while idle awaiting a decision is honored: Interrupted + child reaped.
    #[tokio::test(flavor = "multi_thread")]
    async fn kill_while_awaiting_decision_is_interrupted() {
        let body = format!(
            "{PRELUDE}{}",
            r#"            printf '{"jsonrpc":"2.0","id":%s,"result":{"turn":{"id":"u1"}}}\n' "$id"
            printf '{"jsonrpc":"2.0","method":"turn/started","params":{"threadId":"main","turn":{"id":"u1"}}}\n'
            printf '{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"main","turn":{"id":"u1","status":"completed"}}}\n'
            ;;
    esac
done
exit 0
"#
        );
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), &body);

        let mut handle = spawn_against(dir.path(), &script).await;
        let pid = handle.pid();
        wait_for_turn_ended(&mut handle).await;

        let exit = handle.request_kill_and_wait(KillReason::OperatorStop).await;
        assert!(matches!(exit, ExitKind::Interrupted { .. }), "got {exit:?}");

        let pgid = nix::unistd::Pid::from_raw(-(pid as i32));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if nix::sys::signal::kill(pgid, None).is_err() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "child {pid} still alive"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    // --- emit_line filtering tests ---

    fn make_emit_line_parts() -> (
        Arc<RecordingSink>,
        Arc<CountingStore>,
        Arc<Mutex<chrono::DateTime<Utc>>>,
    ) {
        let sink = Arc::new(RecordingSink::default());
        let store = Arc::new(CountingStore::default());
        let last_event_at = Arc::new(Mutex::new(Utc::now()));
        (sink, store, last_event_at)
    }

    /// (a) Delta line: `last_event_at` is updated; nothing is logged to sink or
    /// stored (sink.count == 0, store.count == 0).
    #[test]
    fn delta_line_updates_liveness_skips_sink_and_store() {
        let delta =
            r#"{"jsonrpc":"2.0","method":"item/agentMessage/delta","params":{"delta":"hello"}}"#;
        assert!(is_delta_notification(delta), "test line must be a delta");

        let (sink, store, last_event_at) = make_emit_line_parts();
        let before = *last_event_at.lock().unwrap();

        // Tiny sleep so `ts` is strictly after `before`.
        std::thread::sleep(std::time::Duration::from_millis(2));
        emit_line(
            "ISSUE-1",
            "run-1",
            delta,
            "stdout",
            &(sink.clone() as Arc<dyn cap_runner::RunnerEventSink>),
            &(store.clone() as Arc<dyn cap_runner::RunnerEventStore>),
            &last_event_at,
        );

        let after = *last_event_at.lock().unwrap();
        assert!(after > before, "last_event_at must advance for delta lines");
        assert_eq!(sink.count(), 0, "delta line must not be pushed to sink");
        assert_eq!(
            store.count(),
            0,
            "delta line must not be inserted into store"
        );
    }

    /// (b) Non-delta line with empty row_type: pushed to sink and logged, but
    /// NOT inserted into store (store.count == 0).
    #[test]
    fn empty_row_type_line_is_pushed_but_not_stored() {
        // A JSON-RPC response (id+result, no method) classifies as row_type "".
        let rpc_response = r#"{"jsonrpc":"2.0","id":1,"result":{}}"#;
        assert!(
            !is_delta_notification(rpc_response),
            "test line must not be delta"
        );

        let (sink, store, last_event_at) = make_emit_line_parts();
        emit_line(
            "ISSUE-1",
            "run-1",
            rpc_response,
            "stdout",
            &(sink.clone() as Arc<dyn cap_runner::RunnerEventSink>),
            &(store.clone() as Arc<dyn cap_runner::RunnerEventStore>),
            &last_event_at,
        );

        assert_eq!(sink.count(), 1, "non-delta line must be pushed to sink");
        assert_eq!(
            store.count(),
            0,
            "empty row_type must not be inserted into store"
        );
    }

    /// (c) Normal classified line (assistant): fully stored — sink and store both
    /// receive one entry.
    #[test]
    fn classified_line_is_fully_stored() {
        // item/completed with agentMessage → row_type "assistant"
        let assistant = r#"{"jsonrpc":"2.0","method":"item/completed","params":{"item":{"type":"agentMessage","text":"Hello world"}}}"#;
        assert!(
            !is_delta_notification(assistant),
            "test line must not be delta"
        );

        let (sink, store, last_event_at) = make_emit_line_parts();
        emit_line(
            "ISSUE-1",
            "run-1",
            assistant,
            "stdout",
            &(sink.clone() as Arc<dyn cap_runner::RunnerEventSink>),
            &(store.clone() as Arc<dyn cap_runner::RunnerEventStore>),
            &last_event_at,
        );

        assert_eq!(sink.count(), 1, "classified line must be pushed to sink");
        assert_eq!(
            store.count(),
            1,
            "classified line must be inserted into store"
        );
    }
}
