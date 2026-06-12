//! Codex runner extension — drives `codex app-server` via JSON-RPC 2.0.

use std::ffi::OsString;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use cap_runner::{ExitKind, KillReason, RunnerHandle, SpawnParams};
use host_api::{Extension, RegisterCtx};
use runner_core::{
    classify_protocol_line, common_env, effective_command, log_ev, scrub_loaded_env,
    setup_process_group, spawn_line_pump, strip_ansi, term_then_kill,
};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::oneshot;

const EVENT_KIND: &'static str = "runner.codex";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

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
    if let Some(ref effort) = p.effort {
        args.push(OsString::from("-c"));
        args.push(OsString::from(format!("model_reasoning_effort={effort:?}")));
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

fn make_turn_start(id: u64, thread_id: &str, workspace: &str, prompt: &str, model: Option<&str>, effort: Option<&str>) -> Value {
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
    let stdout = child.stdout.take().context("no stdout handle after spawn")?;
    let stderr = child.stderr.take().context("no stderr handle after spawn")?;

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
    let timeout = Duration::from_millis(p.max_run_timeout_ms);

    // Clone what the async task needs.
    let issue_id = p.issue_id.clone();
    let run_id = p.run_id.clone();
    let workspace_str = p.workspace.to_string_lossy().into_owned();
    let model = p.model.clone();
    let effort = p.effort.clone();
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

    Ok(RunnerHandle::new(pid, kill_tx, done))
}

// ---------------------------------------------------------------------------
// Protocol driver
// ---------------------------------------------------------------------------

/// Drive the full JSON-RPC handshake and turn, then supervise the child.
#[allow(clippy::too_many_arguments)]
async fn drive_protocol(
    pid: u32,
    stdin: &mut tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
    timeout: Duration,
    kill_rx: oneshot::Receiver<KillReason>,
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
    // We'll use a channel to pass kill_rx into the select inside run_turn.
    // The timeout wraps the entire protocol exchange.
    tokio::select! {
        kind = run_protocol_inner(
            pid, stdin, stdout, issue_id, run_id, workspace,
            model, effort, prompt, events, store, last_event_at,
        ) => kind,
        _ = tokio::time::sleep(timeout) => {
            log_ev(issue_id, "timeout", "turn_timeout_ms exceeded; killing");
            term_then_kill(pid, Duration::from_secs(5));
            ExitKind::Interrupted { reason: "turn_timeout" }
        }
        reason = kill_rx => {
            match reason {
                Ok(KillReason::Timeout) => log_ev(issue_id, "kill", "reason=timeout"),
                Ok(KillReason::OperatorStop) => log_ev(issue_id, "kill", "reason=operator_stop"),
                Ok(KillReason::Reconcile) => log_ev(issue_id, "kill", "reason=reconcile"),
                Err(_) => log_ev(issue_id, "kill", "handle dropped"),
            }
            term_then_kill(pid, Duration::from_secs(5));
            ExitKind::Abnormal(None)
        }
    }
}

/// Inner protocol: handshake + turn pump. Returns ExitKind.
#[allow(clippy::too_many_arguments)]
async fn run_protocol_inner(
    pid: u32,
    stdin: &mut tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
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
            match wait_for_response(&mut lines, $id, issue_id, run_id, &events, &store, &last_event_at).await {
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
    let thread_result = request!(make_thread_start(thread_id_req, workspace, model), thread_id_req);
    let thread_id = match extract_thread_id(&thread_result) {
        Some(id) => id,
        None => {
            log_ev(issue_id, "error", &format!("thread/start: no thread.id in result: {thread_result}"));
            term_then_kill(pid, Duration::from_secs(5));
            return ExitKind::Abnormal(None);
        }
    };

    // 4. turn/start
    let turn_id_req = next_id;
    let _ = request!(
        make_turn_start(turn_id_req, &thread_id, workspace, prompt, model, effort),
        turn_id_req
    );

    // 5. Pump until turn/completed or error notification.
    let success = pump_until_terminal(
        &mut lines,
        stdin,
        issue_id,
        run_id,
        &events,
        &store,
        &last_event_at,
    ).await;

    if success {
        // Give child ~5s to exit cleanly.
        term_then_kill(pid, Duration::from_secs(5));
        ExitKind::Normal
    } else {
        term_then_kill(pid, Duration::from_secs(5));
        ExitKind::Abnormal(None)
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
            log_ev(issue_id, "error", &format!("request {expected_id} timed out after 30s"));
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
                log_ev(issue_id, "error", &format!("request {expected_id} timed out after 30s"));
                return None;
            }
        };

        // Emit to events/store unconditionally.
        emit_line(issue_id, run_id, &line, "stdout", events, store, last_event_at);

        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Check if this is the response we're waiting for.
        if let Some(id) = msg.get("id").and_then(|v| v.as_u64()) {
            if id == expected_id {
                if let Some(err) = msg.get("error") {
                    log_ev(issue_id, "error", &format!("request {expected_id} failed: {err}"));
                    return None;
                }
                return Some(msg.get("result").cloned().unwrap_or(Value::Null));
            }
        }
        // Not our response — continue reading.
    }
}

/// Pump lines after turn/start until turn/completed or error notification.
/// Returns true on success (turn completed), false on failure.
async fn pump_until_terminal(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    stdin: &mut tokio::process::ChildStdin,
    issue_id: &str,
    run_id: &str,
    events: &Arc<dyn cap_runner::RunnerEventSink>,
    store: &Arc<dyn cap_runner::RunnerEventStore>,
    last_event_at: &Arc<Mutex<chrono::DateTime<chrono::Utc>>>,
) -> bool {
    let mut had_error = false;

    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => {
                log_ev(issue_id, "error", "stdout EOF before turn/completed");
                return false;
            }
            Err(e) => {
                log_ev(issue_id, "error", &format!("stdout read error: {e}"));
                return false;
            }
        };

        emit_line(issue_id, run_id, &line, "stdout", events, store, last_event_at);

        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let method = msg.get("method").and_then(|v| v.as_str());
        let id = msg.get("id");
        let has_id = id.map(|v| v.is_string() || v.is_number()).unwrap_or(false);

        // Server request (has both id and method).
        if has_id && method.is_some() {
            let server_id = id.unwrap().clone();
            let method_str = method.unwrap();
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
            continue;
        }

        // Notification: no id.
        if !has_id {
            match method {
                Some("turn/completed") => {
                    let status = msg
                        .get("params")
                        .and_then(|p| p.get("turn"))
                        .and_then(|t| t.get("status"))
                        .and_then(|s| s.as_str());
                    if status == Some("completed") {
                        return !had_error;
                    } else {
                        log_ev(issue_id, "error", &format!("turn/completed with non-success status: {:?}", status));
                        return false;
                    }
                }
                Some("error") => {
                    let msg_str = msg.get("params").map(|v| v.to_string()).unwrap_or_default();
                    log_ev(issue_id, "error", &format!("server error notification: {msg_str}"));
                    had_error = true;
                    // Continue pumping; treat as failure when turn never completes.
                }
                _ => {}
            }
        }
    }
}

/// Emit one stdout line to events + store.
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
    let (row_type, display) = classify_protocol_line(stream, &clean);
    let formatted = format!("child[{issue_id}]: {clean}");
    events.push(formatted);
    if let Ok(mut t) = last_event_at.lock() {
        *t = ts;
    }
    log_ev(issue_id, stream, &clean);
    let payload = serde_json::json!({
        "type": "protocol_event",
        "stream": stream,
        "log_row": row_type,
        "text": display,
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
            args.windows(2).any(|w| w[0] == "-c" && w[1].contains("approval_policy")),
            "approval_policy flag missing: {args:?}"
        );
        assert!(
            args.windows(2).any(|w| w[0] == "-c" && w[1].contains("sandbox_permissions")),
            "sandbox_permissions flag missing: {args:?}"
        );
        assert!(
            args.windows(2).any(|w| w[0] == "-c" && w[1].contains("model=")),
            "model -c flag missing: {args:?}"
        );
    }

    #[test]
    fn codex_effort_is_passed_as_config_flag() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let mut p = params(Some("o3".to_string()), workspace, workspace_root);
        p.effort = Some("high".to_string());
        let args = arg_strings(codex_args(&p));
        assert!(
            args.windows(2).any(|w| w[0] == "-c" && w[1].contains("model_reasoning_effort")),
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
            args.windows(2).any(|w| w[0] == "-c" && w[1].contains("approval_policy")),
            "approval_policy missing without model: {args:?}"
        );
        assert!(
            args.windows(2).any(|w| w[0] == "-c" && w[1].contains("sandbox_permissions")),
            "sandbox_permissions missing without model: {args:?}"
        );
        assert!(
            !args.windows(2).any(|w| w[0] == "-c" && w[1].starts_with("model=")),
            "model flag should be absent when unset: {args:?}"
        );
        assert!(
            !args.windows(2).any(|w| w[0] == "-c" && w[1].contains("model_reasoning_effort")),
            "effort flag should be absent when unset: {args:?}"
        );
        assert!(
            !args.windows(2).any(|w| w[0] == "-c" && w[1].contains("model_provider")),
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
            args.windows(2).any(|w| w[0] == "-c" && w[1].contains("model_provider")),
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
        let req = make_turn_start(3, "t1", "/ws/ISSUE-1", "do something", Some("o3"), Some("high"));
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

    // --- integration test with fake app-server ---

    #[tokio::test]
    async fn fake_server_happy_path_returns_normal() {
        use std::os::unix::fs::PermissionsExt;
        use tokio::fs;

        // Write a fake app-server shell script to a tempdir.
        let dir = tempfile::tempdir().expect("tempdir");
        let script_path = dir.path().join("fake-codex");

        // The script:
        //   - reads lines from stdin
        //   - answers initialize → success
        //   - answers thread/start → {thread:{id:"t1"}}
        //   - answers turn/start → {turn:{id:"u1"}}
        //   - emits turn/completed notification
        //   - reads until EOF, exits 0
        let script = r#"#!/bin/sh
set -e
while IFS= read -r line; do
    # Extract id and method (very naively)
    id=$(echo "$line" | sed 's/.*"id":\([0-9]*\).*/\1/' | grep -E '^[0-9]+$' || true)
    method=$(echo "$line" | grep -o '"method":"[^"]*"' | head -1 | sed 's/"method":"//;s/"//')
    if [ -z "$id" ]; then
        # notification (initialized) — ignore
        continue
    fi
    case "$method" in
        initialize)
            printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
            ;;
        thread/start)
            printf '{"jsonrpc":"2.0","id":%s,"result":{"thread":{"id":"t1"}}}\n' "$id"
            ;;
        turn/start)
            printf '{"jsonrpc":"2.0","id":%s,"result":{"turn":{"id":"u1"}}}\n' "$id"
            # After responding, emit the completion notification
            printf '{"jsonrpc":"2.0","method":"turn/completed","params":{"turn":{"id":"u1","status":"completed"}}}\n'
            ;;
    esac
done
exit 0
"#;

        fs::write(&script_path, script).await.expect("write script");
        fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
            .await
            .expect("chmod");

        let workspaces_dir = dir.path().join("workspaces");
        fs::create_dir_all(&workspaces_dir).await.expect("create workspaces dir");
        let workspace = workspaces_dir.join("ISSUE-1");
        fs::create_dir_all(&workspace).await.expect("create workspace dir");

        let params = SpawnParams::builder(
            script_path.to_str().unwrap(),
            "codex",
            &workspace,
            &workspaces_dir,
            dir.path(),
            "Test prompt".to_string(),
            "ISSUE-1".to_string(),
            "run-1".to_string(),
            10_000, // 10s timeout
            Arc::new(NullSink),
            Arc::new(NullStore),
            Arc::new(Mutex::new(Utc::now())),
        )
        .build();

        let handle = spawn_codex(params).await.expect("spawn");
        let exit = handle.wait().await;
        assert_eq!(exit, ExitKind::Normal, "expected Normal exit, got {exit:?}");
    }
}
