//! Pi chat backend — one long-lived `pi --mode rpc` child per opened session,
//! for the interactive operator chat (`cap-chat`). Distinct from `runner-pi`
//! (per-issue one-shot runs): turns go in as `{"type":"prompt"}` lines, cancel
//! is `{"type":"abort"}` (graceful, session survives), quit is stdin close.
//!
//! Strict JSONL both ways: stdout is split on `\n` only (`\r` stripped);
//! commands may carry an `id` echoed by the matching `response` line; events
//! have no id. Unknown event types are ignored — the pump never crashes on
//! protocol drift. Note: the `jsonrpc/turn` shape in `runner-pi` is NOT stock
//! pi's protocol and is deliberately not used here.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use cap_chat::{ChatBackend, ChatEvent, ChatRole, ChatSession, ChatSessionParams};
use host_api::{Extension, RegisterCtx};
use runner_core::{
    effective_command, scrub_loaded_env, setup_process_group, strip_ansi, term_then_kill,
};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc::Sender;
use tokio::sync::Mutex;

/// How long `close` waits for a clean exit after stdin EOF before escalating.
const CLOSE_WAIT: Duration = Duration::from_secs(2);
const SPAWN_RETRY_WAIT: Duration = Duration::from_millis(25);
const SPAWN_RETRIES: usize = 10;
/// SIGTERM-to-SIGKILL grace passed to `term_then_kill` on close overrun.
const KILL_GRACE: Duration = Duration::from_secs(5);

pub struct ChatPiExtension;

impl Extension for ChatPiExtension {
    fn id(&self) -> &'static str {
        "chat-pi"
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            ctx.services
                .service::<dyn ChatBackend>("pi", Arc::new(PiChatBackend))?;
            Ok(())
        })
    }
}

pub struct PiChatBackend;

impl ChatBackend for PiChatBackend {
    fn open<'a>(
        &'a self,
        params: ChatSessionParams,
        tx: Sender<ChatEvent>,
    ) -> cap_chat::BoxFuture<'a, Result<Box<dyn ChatSession>>> {
        Box::pin(async move {
            let session = PiChatSession::spawn(&params, tx).await?;
            Ok(Box::new(session) as Box<dyn ChatSession>)
        })
    }
}

pub struct PiChatSession {
    pid: u32,
    next_turn: u64,
    tx: Sender<ChatEvent>,
    /// Shared with the stdout pump (extension_ui_request auto-answers).
    /// `close` takes it; dropping the writer is pi's clean-quit signal.
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    queue: Arc<Mutex<TurnQueue>>,
    /// Set before the deliberate stdin close so the wait task does not
    /// classify the resulting exit as a crash (`SessionClosed`).
    closing: Arc<AtomicBool>,
    wait_handle: tokio::task::JoinHandle<()>,
}

#[derive(Default)]
struct TurnQueue {
    busy: bool,
    pending: VecDeque<String>,
}

impl PiChatSession {
    async fn spawn(params: &ChatSessionParams, tx: Sender<ChatEvent>) -> Result<Self> {
        let command = effective_command(&params.command, "pi");
        let mut cmd = Command::new(&command);
        cmd.args(pi_args(params));
        scrub_loaded_env(&mut cmd);
        // Wire the host MCP bridge so the chat pi agent sees the same registry
        // tools an issue worker does: a per-session `--mcp-config` pointing at
        // `<host> __mcp-bridge`, plus `MCP_DIRECT_TOOLS`. Reuses the runner's
        // config writer so chat and worker spawns stay identical.
        if let Some(bridge) = &params.host_tool_bridge {
            let (bridge_args, env) =
                runner_core::pi_mcp_config_args(&params.session_dir, bridge)?;
            cmd.args(bridge_args);
            cmd.envs(env);
        }
        cmd.current_dir(&params.agent_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        setup_process_group(&mut cmd);

        let mut child = spawn_child(cmd).await.with_context(|| {
            format!(
                "spawning `{}` in {}",
                command.to_string_lossy(),
                params.agent_root.display()
            )
        })?;
        let pid = child
            .id()
            .context("chat child has no pid immediately after spawn")?;

        let stdin = Arc::new(Mutex::new(child.stdin.take()));
        let stdout = child.stdout.take().context("chat child stdout not piped")?;
        let stderr = child.stderr.take().context("chat child stderr not piped")?;
        let queue = Arc::new(Mutex::new(TurnQueue::default()));
        spawn_stdout_pump(stdout, tx.clone(), Arc::clone(&stdin), Arc::clone(&queue));
        spawn_stderr_pump(stderr, tx.clone());

        let closing = Arc::new(AtomicBool::new(false));
        let wait_handle = {
            let closing = Arc::clone(&closing);
            let tx = tx.clone();
            tokio::spawn(async move {
                let status = child.wait().await;
                if !closing.load(Ordering::SeqCst) {
                    let error = match status {
                        Ok(status) => format!("pi exited unexpectedly: {status}"),
                        Err(e) => format!("pi wait failed: {e}"),
                    };
                    let _ = tx
                        .send(ChatEvent::SessionClosed { error: Some(error) })
                        .await;
                }
            })
        };

        Ok(Self {
            pid,
            next_turn: 0,
            tx,
            stdin,
            queue,
            closing,
            wait_handle,
        })
    }

    async fn write_line(&self, line: String) -> Result<()> {
        write_line_to(&self.stdin, line).await
    }

    async fn accept_turn(&self, line: String) -> Result<()> {
        {
            let mut queue = self.queue.lock().await;
            if queue.busy {
                queue.pending.push_back(line);
                return Ok(());
            }
            queue.busy = true;
        }
        if let Err(error) = self.write_line(line).await {
            self.queue.lock().await.busy = false;
            return Err(error);
        }
        Ok(())
    }
}

async fn spawn_child(mut cmd: Command) -> io::Result<tokio::process::Child> {
    for attempt in 0..SPAWN_RETRIES {
        match cmd.spawn() {
            Err(err) if is_executable_busy(&err) && attempt + 1 < SPAWN_RETRIES => {
                tokio::time::sleep(SPAWN_RETRY_WAIT).await;
            }
            result => return result,
        }
    }
    cmd.spawn()
}

fn is_executable_busy(err: &io::Error) -> bool {
    err.raw_os_error() == Some(26)
}

impl ChatSession for PiChatSession {
    fn send_turn(&mut self, prompt: String) -> cap_chat::BoxFuture<'_, Result<()>> {
        self.next_turn += 1;
        let line = prompt_command(&format!("t{}", self.next_turn), &prompt);
        Box::pin(async move { self.accept_turn(line).await })
    }

    fn abort(&mut self) -> cap_chat::BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.write_line(abort_command()).await?;
            abort_queued_turns(&self.queue, &self.tx, false).await;
            Ok(())
        })
    }

    fn close(self: Box<Self>) -> cap_chat::BoxFuture<'static, Result<()>> {
        let this = *self;
        Box::pin(async move {
            this.closing.store(true, Ordering::SeqCst);
            this.stdin.lock().await.take(); // drop writer -> EOF -> clean pi quit
            if tokio::time::timeout(CLOSE_WAIT, this.wait_handle)
                .await
                .is_err()
            {
                term_then_kill(this.pid, KILL_GRACE);
            }
            Ok(())
        })
    }
}

fn pi_args(params: &ChatSessionParams) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("--mode"),
        OsString::from("rpc"),
        OsString::from("--session-dir"),
        params.session_dir.as_os_str().to_os_string(),
    ];
    if let Some(model) = &params.model {
        args.push(OsString::from("--model"));
        args.push(OsString::from(model));
    }
    if let Some(provider) = &params.provider {
        args.push(OsString::from("--provider"));
        args.push(OsString::from(provider));
    }
    args
}

fn prompt_command(id: &str, message: &str) -> String {
    serde_json::json!({ "id": id, "type": "prompt", "message": message }).to_string() + "\n"
}

fn abort_command() -> String {
    serde_json::json!({ "type": "abort" }).to_string() + "\n"
}

async fn write_line_to(stdin: &Arc<Mutex<Option<ChildStdin>>>, line: String) -> Result<()> {
    let mut guard = stdin.lock().await;
    let stdin = guard.as_mut().context("chat session stdin is closed")?;
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await?;
    Ok(())
}

/// What one stdout line asks of the pump.
enum Mapped {
    Emit(ChatEvent),
    /// Blocking `extension_ui_request` dialog: answer on stdin so the turn
    /// keeps moving, and surface a notice in the transcript.
    AutoRespond {
        reply: String,
        notice: String,
    },
    Ignore,
}

fn map_stdout_line(line: &str) -> Mapped {
    if line.trim().is_empty() {
        return Mapped::Ignore;
    }
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Mapped::Emit(ChatEvent::Error(line.to_string()));
    };
    match value.get("type").and_then(Value::as_str) {
        Some("message_update") => map_message_update(&value),
        Some("tool_execution_update") => Mapped::Emit(ChatEvent::ToolOutput {
            id: tool_call_id(&value),
            // partialResult is ACCUMULATED output: each update replaces the
            // prior text for this id (ChatEvent::ToolOutput contract).
            text: value
                .get("partialResult")
                .map(result_text)
                .unwrap_or_default(),
            is_error: false,
            done: false,
        }),
        Some("tool_execution_end") => Mapped::Emit(ChatEvent::ToolOutput {
            id: tool_call_id(&value),
            text: value.get("result").map(result_text).unwrap_or_default(),
            is_error: value
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            done: true,
        }),
        Some("agent_end") => Mapped::Emit(ChatEvent::TurnFinished {
            ok: true,
            error: None,
        }),
        Some("response") => match value.get("success").and_then(Value::as_bool) {
            Some(false) => Mapped::Emit(ChatEvent::TurnFinished {
                ok: false,
                error: Some(
                    value
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("backend rejected command")
                        .to_string(),
                ),
            }),
            _ => Mapped::Ignore,
        },
        Some("extension_ui_request") => map_ui_request(&value),
        // queue_update, compaction_*, auto_retry_*, anything unknown: ignore.
        _ => Mapped::Ignore,
    }
}

fn map_message_update(value: &Value) -> Mapped {
    let Some(event) = value.get("assistantMessageEvent") else {
        return Mapped::Ignore;
    };
    match event.get("type").and_then(Value::as_str) {
        Some("text_delta") => Mapped::Emit(ChatEvent::Delta {
            role: ChatRole::Assistant,
            text: delta_text(event),
        }),
        Some("thinking_delta") => Mapped::Emit(ChatEvent::Delta {
            role: ChatRole::Thinking,
            text: delta_text(event),
        }),
        Some("toolcall_end") => {
            let call = event.get("toolCall");
            let field = |key: &str| -> String {
                call.and_then(|c| c.get(key))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            Mapped::Emit(ChatEvent::ToolCall {
                id: field("id"),
                name: field("name"),
                args: call
                    .and_then(|c| c.get("arguments"))
                    .map(render_args)
                    .unwrap_or_default(),
            })
        }
        Some("error") => {
            let reason = event
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("error");
            let error = if reason == "aborted" {
                "aborted".to_string()
            } else {
                event
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or(reason)
                    .to_string()
            };
            Mapped::Emit(ChatEvent::TurnFinished {
                ok: false,
                error: Some(error),
            })
        }
        _ => Mapped::Ignore,
    }
}

fn map_ui_request(value: &Value) -> Mapped {
    let method = value.get("method").and_then(Value::as_str).unwrap_or("");
    let reply_value = match method {
        "confirm" => Value::Bool(true),
        "select" => first_option(value).unwrap_or_else(|| Value::String(String::new())),
        "input" | "editor" => Value::String(String::new()),
        // notify / setStatus / setWidget / setTitle and unknowns are
        // fire-and-forget: no reply expected.
        _ => return Mapped::Ignore,
    };
    let reply = serde_json::json!({
        "type": "extension_ui_response",
        "id": value.get("id").cloned().unwrap_or(Value::Null),
        "value": reply_value,
    })
    .to_string()
        + "\n";
    Mapped::AutoRespond {
        reply,
        notice: format!("auto-answered dialog: {method}"),
    }
}

fn first_option(value: &Value) -> Option<Value> {
    value
        .get("options")
        .or_else(|| value.get("params").and_then(|p| p.get("options")))
        .and_then(Value::as_array)
        .and_then(|options| options.first())
        .cloned()
}

fn delta_text(event: &Value) -> String {
    event
        .get("delta")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn tool_call_id(value: &Value) -> String {
    value
        .get("toolCallId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Render a tool result payload: plain string, `{content:[{text}]}` blocks,
/// or (fallback) the raw JSON.
fn result_text(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    if let Some(items) = value.get("content").and_then(Value::as_array) {
        return items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
    }
    value.to_string()
}

fn render_args(arguments: &Value) -> String {
    match arguments.as_str() {
        Some(text) => text.to_string(),
        None => arguments.to_string(),
    }
}

fn spawn_stdout_pump(
    stdout: ChildStdout,
    tx: Sender<ChatEvent>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    queue: Arc<Mutex<TurnQueue>>,
) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let clean = strip_ansi(line.trim_end_matches('\r'));
            match map_stdout_line(&clean) {
                Mapped::Emit(event) => {
                    let turn_finished = match &event {
                        ChatEvent::TurnFinished { ok, .. } => Some(*ok),
                        _ => None,
                    };
                    if tx.send(event).await.is_err() {
                        return;
                    }
                    match turn_finished {
                        Some(true) => send_next_queued_turn(&stdin, &queue, &tx).await,
                        Some(false) => abort_queued_turns(&queue, &tx, true).await,
                        None => {}
                    }
                }
                Mapped::AutoRespond { reply, notice } => {
                    let _ = write_line_to(&stdin, reply).await;
                    if tx.send(ChatEvent::Error(notice)).await.is_err() {
                        return;
                    }
                }
                Mapped::Ignore => {}
            }
        }
    });
}

async fn abort_queued_turns(
    queue: &Arc<Mutex<TurnQueue>>,
    tx: &Sender<ChatEvent>,
    clear_busy: bool,
) {
    let dropped = {
        let mut queue = queue.lock().await;
        let dropped = queue.pending.len();
        queue.pending.clear();
        if clear_busy {
            queue.busy = false;
        }
        dropped
    };
    send_failed_finishes(tx, dropped, "aborted").await;
}

async fn send_failed_finishes(tx: &Sender<ChatEvent>, count: usize, error: &str) {
    for _ in 0..count {
        if tx
            .send(ChatEvent::TurnFinished {
                ok: false,
                error: Some(error.to_string()),
            })
            .await
            .is_err()
        {
            break;
        }
    }
}

async fn send_next_queued_turn(
    stdin: &Arc<Mutex<Option<ChildStdin>>>,
    queue: &Arc<Mutex<TurnQueue>>,
    tx: &Sender<ChatEvent>,
) {
    let next = {
        let mut queue = queue.lock().await;
        match queue.pending.pop_front() {
            Some(line) => Some(line),
            None => {
                queue.busy = false;
                None
            }
        }
    };
    if let Some(line) = next {
        if write_line_to(stdin, line).await.is_err() {
            let dropped = {
                let mut queue = queue.lock().await;
                let dropped = 1 + queue.pending.len();
                queue.busy = false;
                queue.pending.clear();
                dropped
            };
            send_failed_finishes(tx, dropped, "send failed").await;
        }
    }
}

fn spawn_stderr_pump(stderr: ChildStderr, tx: Sender<ChatEvent>) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let clean = strip_ansi(line.trim_end_matches('\r'));
            if clean.trim().is_empty() {
                continue;
            }
            if tx.send(ChatEvent::Error(clean)).await.is_err() {
                return;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use tokio::sync::mpsc::Receiver;

    use super::*;

    // -- rpc command JSON shapes ---------------------------------------------

    #[test]
    fn prompt_command_is_one_jsonl_line_with_id_and_message() {
        let line = prompt_command("t1", "Fix the failing test");
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1);
        let value: Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(value["type"], "prompt");
        assert_eq!(value["id"], "t1");
        assert_eq!(value["message"], "Fix the failing test");
    }

    #[test]
    fn abort_command_is_one_jsonl_line_without_id() {
        let line = abort_command();
        assert!(line.ends_with('\n'));
        let value: Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(value["type"], "abort");
        assert!(value.get("id").is_none());
    }

    #[test]
    fn pi_args_carry_rpc_mode_session_dir_and_optional_model() {
        let root = Path::new("/agent");
        let sessions = Path::new("/agent/data/tui/sessions");
        let without_model = ChatSessionParams::builder("", root, sessions).build();
        assert_eq!(
            pi_args(&without_model),
            vec![
                OsString::from("--mode"),
                OsString::from("rpc"),
                OsString::from("--session-dir"),
                OsString::from("/agent/data/tui/sessions"),
            ]
        );

        let with_model = ChatSessionParams::builder("", root, sessions)
            .model(Some("gpt-5".into()))
            .build();
        let args = pi_args(&with_model);
        assert_eq!(args[4], OsString::from("--model"));
        assert_eq!(args[5], OsString::from("gpt-5"));
    }

    #[test]
    fn pi_args_appends_provider_when_set() {
        let root = Path::new("/agent");
        let sessions = Path::new("/agent/data/tui/sessions");
        let params = ChatSessionParams::builder("", root, sessions)
            .model(Some("gpt-5".into()))
            .provider(Some("my-provider".into()))
            .build();
        let args = pi_args(&params);
        // --model gpt-5 --provider my-provider
        assert!(args.contains(&OsString::from("--provider")));
        let provider_idx = args.iter().position(|a| a == "--provider").unwrap();
        assert_eq!(args[provider_idx + 1], OsString::from("my-provider"));
    }

    // -- event mapping --------------------------------------------------------

    fn mapped_event(line: &str) -> ChatEvent {
        match map_stdout_line(line) {
            Mapped::Emit(event) => event,
            Mapped::AutoRespond { .. } => panic!("expected Emit, got AutoRespond for {line}"),
            Mapped::Ignore => panic!("expected Emit, got Ignore for {line}"),
        }
    }

    fn assert_ignored(line: &str) {
        assert!(
            matches!(map_stdout_line(line), Mapped::Ignore),
            "expected Ignore for {line}"
        );
    }

    #[test]
    fn text_delta_maps_to_assistant_delta() {
        let line = r#"{"type":"message_update","message":{},"assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"pong"}}"#;
        match mapped_event(line) {
            ChatEvent::Delta { role, text } => {
                assert_eq!(role, ChatRole::Assistant);
                assert_eq!(text, "pong");
            }
            other => panic!("expected Delta, got {other:?}"),
        }
    }

    #[test]
    fn thinking_delta_maps_to_thinking_delta() {
        let line = r#"{"type":"message_update","message":{},"assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":"Let me look..."}}"#;
        match mapped_event(line) {
            ChatEvent::Delta { role, text } => {
                assert_eq!(role, ChatRole::Thinking);
                assert_eq!(text, "Let me look...");
            }
            other => panic!("expected Delta, got {other:?}"),
        }
    }

    #[test]
    fn toolcall_end_maps_to_tool_call_with_rendered_args() {
        let line = r#"{"type":"message_update","message":{},"assistantMessageEvent":{"type":"toolcall_end","contentIndex":1,"toolCall":{"id":"call_123","name":"bash","arguments":{"command":"ls"}}}}"#;
        match mapped_event(line) {
            ChatEvent::ToolCall { id, name, args } => {
                assert_eq!(id, "call_123");
                assert_eq!(name, "bash");
                assert_eq!(args, r#"{"command":"ls"}"#);
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn tool_execution_update_replaces_accumulated_partial_result() {
        // partialResult is ACCUMULATED, not a delta: each update carries the
        // full text so far and maps to a not-done ToolOutput that replaces
        // the previous one for the same id.
        let first = r#"{"type":"tool_execution_update","toolCallId":"call_123","partialResult":"total 48"}"#;
        let second = r#"{"type":"tool_execution_update","toolCallId":"call_123","partialResult":"total 48\nsrc"}"#;
        for (line, expected) in [(first, "total 48"), (second, "total 48\nsrc")] {
            match mapped_event(line) {
                ChatEvent::ToolOutput {
                    id,
                    text,
                    is_error,
                    done,
                } => {
                    assert_eq!(id, "call_123");
                    assert_eq!(text, expected);
                    assert!(!is_error);
                    assert!(!done);
                }
                other => panic!("expected ToolOutput, got {other:?}"),
            }
        }
    }

    #[test]
    fn tool_execution_end_maps_to_done_tool_output() {
        let line = r#"{"type":"tool_execution_end","toolCallId":"call_123","toolName":"bash","result":{"content":[{"type":"text","text":"total 48..."}]},"isError":false}"#;
        match mapped_event(line) {
            ChatEvent::ToolOutput {
                id,
                text,
                is_error,
                done,
            } => {
                assert_eq!(id, "call_123");
                assert_eq!(text, "total 48...");
                assert!(!is_error);
                assert!(done);
            }
            other => panic!("expected ToolOutput, got {other:?}"),
        }
    }

    #[test]
    fn tool_execution_end_error_flag_is_carried() {
        let line = r#"{"type":"tool_execution_end","toolCallId":"c1","result":{"content":[{"type":"text","text":"boom"}]},"isError":true}"#;
        match mapped_event(line) {
            ChatEvent::ToolOutput { is_error, done, .. } => {
                assert!(is_error);
                assert!(done);
            }
            other => panic!("expected ToolOutput, got {other:?}"),
        }
    }

    #[test]
    fn agent_end_maps_to_successful_turn_finished() {
        match mapped_event(r#"{"type":"agent_end"}"#) {
            ChatEvent::TurnFinished { ok, error } => {
                assert!(ok);
                assert!(error.is_none());
            }
            other => panic!("expected TurnFinished, got {other:?}"),
        }
    }

    #[test]
    fn aborted_assistant_error_maps_to_aborted_turn() {
        let line = r#"{"type":"message_update","message":{},"assistantMessageEvent":{"type":"error","reason":"aborted"}}"#;
        match mapped_event(line) {
            ChatEvent::TurnFinished { ok, error } => {
                assert!(!ok);
                assert_eq!(error.as_deref(), Some("aborted"));
            }
            other => panic!("expected TurnFinished, got {other:?}"),
        }
    }

    #[test]
    fn assistant_error_reason_error_maps_to_failed_turn() {
        let line = r#"{"type":"message_update","message":{},"assistantMessageEvent":{"type":"error","reason":"error","error":"context overflow"}}"#;
        match mapped_event(line) {
            ChatEvent::TurnFinished { ok, error } => {
                assert!(!ok);
                assert_eq!(error.as_deref(), Some("context overflow"));
            }
            other => panic!("expected TurnFinished, got {other:?}"),
        }
    }

    #[test]
    fn failed_response_maps_to_failed_turn_and_success_is_ignored() {
        let failed = r#"{"type":"response","id":"t1","success":false,"error":"no such command"}"#;
        match mapped_event(failed) {
            ChatEvent::TurnFinished { ok, error } => {
                assert!(!ok);
                assert_eq!(error.as_deref(), Some("no such command"));
            }
            other => panic!("expected TurnFinished, got {other:?}"),
        }
        assert_ignored(r#"{"type":"response","id":"t1","success":true}"#);
    }

    #[test]
    fn blocking_dialogs_are_auto_answered_with_a_notice() {
        let cases = [
            (
                r#"{"type":"extension_ui_request","id":"d1","method":"confirm","title":"Run?"}"#,
                Value::Bool(true),
            ),
            (
                r#"{"type":"extension_ui_request","id":"d2","method":"select","options":["a","b"]}"#,
                Value::String("a".into()),
            ),
            (
                r#"{"type":"extension_ui_request","id":"d3","method":"input"}"#,
                Value::String(String::new()),
            ),
            (
                r#"{"type":"extension_ui_request","id":"d4","method":"editor"}"#,
                Value::String(String::new()),
            ),
        ];
        for (line, expected_value) in cases {
            match map_stdout_line(line) {
                Mapped::AutoRespond { reply, notice } => {
                    assert!(reply.ends_with('\n'));
                    let value: Value = serde_json::from_str(reply.trim_end()).unwrap();
                    assert_eq!(value["type"], "extension_ui_response");
                    assert_eq!(
                        value["id"],
                        serde_json::from_str::<Value>(line).unwrap()["id"]
                    );
                    assert_eq!(value["value"], expected_value);
                    assert!(notice.starts_with("auto-answered dialog: "));
                }
                _ => panic!("expected AutoRespond for {line}"),
            }
        }
    }

    #[test]
    fn fire_and_forget_and_unknown_events_are_ignored() {
        assert_ignored(
            r#"{"type":"extension_ui_request","id":"n1","method":"notify","message":"hi"}"#,
        );
        assert_ignored(r#"{"type":"extension_ui_request","id":"n2","method":"setStatus"}"#);
        assert_ignored(r#"{"type":"extension_ui_request","id":"n3","method":"setWidget"}"#);
        assert_ignored(r#"{"type":"extension_ui_request","id":"n4","method":"setTitle"}"#);
        assert_ignored(r#"{"type":"queue_update","queued":2}"#);
        assert_ignored(r#"{"type":"compaction_start"}"#);
        assert_ignored(r#"{"type":"auto_retry_start","attempt":1}"#);
        assert_ignored(r#"{"type":"some_future_event"}"#);
        assert_ignored(r#"{"no_type_at_all":1}"#);
        assert_ignored("   ");
    }

    #[test]
    fn unparseable_line_maps_to_error() {
        match mapped_event("pi: something went sideways") {
            ChatEvent::Error(text) => assert_eq!(text, "pi: something went sideways"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    // -- registry -------------------------------------------------------------

    #[tokio::test]
    async fn registers_chat_backend_under_pi() {
        let temp = tempfile::tempdir().unwrap();
        let paths = host_api::HostPaths::new(temp.path()).unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let mut ctx = RegisterCtx {
            bus: host_api::EventBus::new(),
            http: host_api::HttpRegistry::disabled(),
            foreground: host_api::ForegroundRegistry::default(),
            services: host_api::ServiceRegistry::default(),
            paths,
            config: host_api::ConfigStore::default(),
            shutdown: host_api::ShutdownToken::new(rx),
        };

        ChatPiExtension.register(&mut ctx).await.unwrap();
        assert!(ctx.services.get_named::<dyn ChatBackend>("pi").is_ok());
        assert!(ctx.services.get_named::<dyn ChatBackend>("claude").is_err());
    }

    // -- spawn-level (stub script stands in for `pi --mode rpc`) ---------------

    fn write_stub(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("stub-pi.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn spawn_retries_while_stub_script_is_busy() {
        use std::io::Write;

        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("stub-pi.sh");
        let mut file = std::fs::File::create(&script).unwrap();
        file.write_all(b"#!/bin/sh\nwhile IFS= read -r line; do :; done\n")
            .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let params = stub_params(temp.path(), &script);
        let (tx, _rx) = tokio::sync::mpsc::channel(64);

        let task = tokio::spawn(async move { PiChatSession::spawn(&params, tx).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(file);

        let session = task.await.unwrap().unwrap();
        ChatSession::close(Box::new(session)).await.unwrap();
    }

    fn stub_params(temp: &Path, script: &Path) -> ChatSessionParams {
        let sessions = temp.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        ChatSessionParams::builder(script.to_str().unwrap(), temp, &sessions).build()
    }

    async fn next_event(rx: &mut Receiver<ChatEvent>) -> ChatEvent {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("timed out waiting for ChatEvent")
            .expect("event channel closed")
    }

    /// Echo stub: answers every prompt with thinking + text + agent_end, and
    /// every abort with the aborted error event (mirrors runner-fake's
    /// printf-canned-lines pattern).
    const ECHO_STUB: &str = r#"while IFS= read -r line; do
  case "$line" in
    *'"type":"prompt"'*)
      printf '%s\n' '{"type":"message_update","message":{},"assistantMessageEvent":{"type":"thinking_delta","delta":"hmm"}}'
      printf '%s\n' '{"type":"message_update","message":{},"assistantMessageEvent":{"type":"text_delta","delta":"pong"}}'
      printf '%s\n' '{"type":"agent_end"}'
      ;;
    *'"type":"abort"'*)
      printf '%s\n' '{"type":"message_update","message":{},"assistantMessageEvent":{"type":"error","reason":"aborted"}}'
      ;;
  esac
done"#;

    #[tokio::test(flavor = "multi_thread")]
    async fn open_send_turn_streams_events_then_turn_finished() {
        let temp = tempfile::tempdir().unwrap();
        let script = write_stub(temp.path(), ECHO_STUB);
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);

        // Go through the ChatBackend trait — the same path the TUI takes after
        // resolving `dyn ChatBackend @ "pi"` from the registry.
        let mut session = PiChatBackend
            .open(stub_params(temp.path(), &script), tx)
            .await
            .unwrap();
        session.send_turn("ping".to_string()).await.unwrap();

        match next_event(&mut rx).await {
            ChatEvent::Delta { role, text } => {
                assert_eq!(role, ChatRole::Thinking);
                assert_eq!(text, "hmm");
            }
            other => panic!("expected thinking Delta, got {other:?}"),
        }
        match next_event(&mut rx).await {
            ChatEvent::Delta { role, text } => {
                assert_eq!(role, ChatRole::Assistant);
                assert_eq!(text, "pong");
            }
            other => panic!("expected assistant Delta, got {other:?}"),
        }
        match next_event(&mut rx).await {
            ChatEvent::TurnFinished { ok, error } => {
                assert!(ok);
                assert!(error.is_none());
            }
            other => panic!("expected TurnFinished, got {other:?}"),
        }

        // Stub exits on stdin EOF, so close is clean and emits no SessionClosed.
        session.close().await.unwrap();
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn abort_finishes_turn_as_aborted_and_session_stays_usable() {
        // This stub never answers prompts: the turn stays in flight until abort.
        let stub = r#"while IFS= read -r line; do
  case "$line" in
    *'"type":"abort"'*)
      printf '%s\n' '{"type":"message_update","message":{},"assistantMessageEvent":{"type":"error","reason":"aborted"}}'
      ;;
  esac
done"#;
        let temp = tempfile::tempdir().unwrap();
        let script = write_stub(temp.path(), stub);
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);

        let mut session = PiChatSession::spawn(&stub_params(temp.path(), &script), tx)
            .await
            .unwrap();
        session.send_turn("hang forever".to_string()).await.unwrap();
        session.abort().await.unwrap();

        match next_event(&mut rx).await {
            ChatEvent::TurnFinished { ok, error } => {
                assert!(!ok);
                assert_eq!(error.as_deref(), Some("aborted"));
            }
            other => panic!("expected aborted TurnFinished, got {other:?}"),
        }

        // Session survives the abort: the next turn can still be written.
        session.send_turn("again".to_string()).await.unwrap();
        ChatSession::close(Box::new(session)).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn send_turn_while_busy_is_queued_until_turn_finished() {
        let stub = r#"while IFS= read -r line; do
  case "$line" in
    *'"type":"prompt"'*)
      printf '%s\n' '{"type":"message_update","message":{},"assistantMessageEvent":{"type":"text_delta","delta":"turn"}}'
      ( sleep 0.1; printf '%s\n' '{"type":"agent_end"}' ) &
      ;;
  esac
done
wait"#;
        let temp = tempfile::tempdir().unwrap();
        let script = write_stub(temp.path(), stub);
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);

        let mut session = PiChatSession::spawn(&stub_params(temp.path(), &script), tx)
            .await
            .unwrap();
        session.send_turn("first".to_string()).await.unwrap();
        session.send_turn("second".to_string()).await.unwrap();

        match next_event(&mut rx).await {
            ChatEvent::Delta { text, .. } => assert_eq!(text, "turn"),
            other => panic!("expected first turn delta, got {other:?}"),
        }
        match next_event(&mut rx).await {
            ChatEvent::TurnFinished { ok, .. } => assert!(ok),
            other => panic!("expected first TurnFinished, got {other:?}"),
        }
        match next_event(&mut rx).await {
            ChatEvent::Delta { text, .. } => assert_eq!(text, "turn"),
            other => panic!("expected queued turn delta, got {other:?}"),
        }
        match next_event(&mut rx).await {
            ChatEvent::TurnFinished { ok, .. } => assert!(ok),
            other => panic!("expected queued TurnFinished, got {other:?}"),
        }

        ChatSession::close(Box::new(session)).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failed_turn_does_not_release_queued_turn() {
        let stub = r#"while IFS= read -r line; do
  case "$line" in
    *'"type":"prompt"'*)
      printf '%s\n' '{"type":"message_update","message":{},"assistantMessageEvent":{"type":"error","reason":"boom"}}'
      ;;
  esac
done"#;
        let temp = tempfile::tempdir().unwrap();
        let script = write_stub(temp.path(), stub);
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);

        let mut session = PiChatSession::spawn(&stub_params(temp.path(), &script), tx)
            .await
            .unwrap();
        session.send_turn("first".to_string()).await.unwrap();
        session.send_turn("second".to_string()).await.unwrap();

        match next_event(&mut rx).await {
            ChatEvent::TurnFinished { ok, error } => {
                assert!(!ok);
                assert_eq!(error.as_deref(), Some("boom"));
            }
            other => panic!("expected failed TurnFinished, got {other:?}"),
        }
        match next_event(&mut rx).await {
            ChatEvent::TurnFinished { ok, error } => {
                assert!(!ok);
                assert_eq!(error.as_deref(), Some("aborted"));
            }
            other => panic!("expected queued aborted TurnFinished, got {other:?}"),
        }
        assert!(tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .is_err());

        ChatSession::close(Box::new(session)).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn abort_reports_queued_turns_as_aborted() {
        let stub = r#"while IFS= read -r line; do
  case "$line" in
    *'"type":"abort"'*)
      printf '%s\n' '{"type":"message_update","message":{},"assistantMessageEvent":{"type":"error","reason":"aborted"}}'
      ;;
  esac
done"#;
        let temp = tempfile::tempdir().unwrap();
        let script = write_stub(temp.path(), stub);
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);

        let mut session = PiChatSession::spawn(&stub_params(temp.path(), &script), tx)
            .await
            .unwrap();
        session.send_turn("first".to_string()).await.unwrap();
        session.send_turn("second".to_string()).await.unwrap();
        session.abort().await.unwrap();

        for _ in 0..2 {
            match next_event(&mut rx).await {
                ChatEvent::TurnFinished { ok, error } => {
                    assert!(!ok);
                    assert_eq!(error.as_deref(), Some("aborted"));
                }
                other => panic!("expected aborted TurnFinished, got {other:?}"),
            }
        }

        ChatSession::close(Box::new(session)).await.unwrap();
    }

    #[tokio::test]
    async fn queued_write_failure_reports_dropped_turns_finished() {
        let stdin = Arc::new(Mutex::new(None));
        let queue = Arc::new(Mutex::new(TurnQueue {
            busy: true,
            pending: VecDeque::from([
                prompt_command("t2", "second"),
                prompt_command("t3", "third"),
            ]),
        }));
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);

        send_next_queued_turn(&stdin, &queue, &tx).await;

        for _ in 0..2 {
            match next_event(&mut rx).await {
                ChatEvent::TurnFinished { ok, error } => {
                    assert!(!ok);
                    assert_eq!(error.as_deref(), Some("send failed"));
                }
                other => panic!("expected failed TurnFinished, got {other:?}"),
            }
        }
        assert!(tokio::time::timeout(Duration::from_millis(50), rx.recv())
            .await
            .is_err());
        let queue = queue.lock().await;
        assert!(!queue.busy);
        assert!(queue.pending.is_empty());
    }

    #[tokio::test]
    async fn first_write_failure_resets_busy_state() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let queue = Arc::new(Mutex::new(TurnQueue::default()));
        let session = PiChatSession {
            pid: 0,
            next_turn: 0,
            tx,
            stdin: Arc::new(Mutex::new(None)),
            queue: Arc::clone(&queue),
            closing: Arc::new(AtomicBool::new(true)),
            wait_handle: tokio::spawn(async {}),
        };

        assert!(session.accept_turn(prompt_command("t1", "first")).await.is_err());
        let queue = queue.lock().await;
        assert!(!queue.busy);
        assert!(queue.pending.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stderr_lines_arrive_as_error_events_stripped_and_skipping_blanks() {
        // Exercises spawn_stderr_pump: ANSI codes and trailing \r are stripped,
        // blank lines are dropped (proven by "second line" arriving next), and
        // each surviving line maps to ChatEvent::Error.
        let stub = r#"printf '\033[31mpi: boom\033[0m\r\n' >&2
printf '   \n' >&2
printf 'second line\n' >&2
while IFS= read -r line; do :; done"#;
        let temp = tempfile::tempdir().unwrap();
        let script = write_stub(temp.path(), stub);
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);

        let session = PiChatSession::spawn(&stub_params(temp.path(), &script), tx)
            .await
            .unwrap();

        match next_event(&mut rx).await {
            ChatEvent::Error(text) => assert_eq!(text, "pi: boom"),
            other => panic!("expected Error, got {other:?}"),
        }
        match next_event(&mut rx).await {
            ChatEvent::Error(text) => assert_eq!(text, "second line"),
            other => panic!("expected Error, got {other:?}"),
        }

        ChatSession::close(Box::new(session)).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn close_kills_the_whole_process_group_on_overrun() {
        // Ignores stdin entirely and keeps a grandchild alive: only a process
        // group signal can take both down.
        let temp = tempfile::tempdir().unwrap();
        let script = write_stub(temp.path(), "sleep 300 &\nsleep 300\n");
        let (tx, _rx) = tokio::sync::mpsc::channel(64);

        let session = PiChatSession::spawn(&stub_params(temp.path(), &script), tx)
            .await
            .unwrap();
        let pid = session.pid;
        ChatSession::close(Box::new(session)).await.unwrap();

        let pgid = nix::unistd::Pid::from_raw(-(pid as i32));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if nix::sys::signal::kill(pgid, None).is_err() {
                break; // no group member left alive
            }
            assert!(
                std::time::Instant::now() < deadline,
                "process group {pid} still alive after close"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    // -- live smoke (manual; needs `pi` on PATH and spends API) ----------------

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires a real `pi` binary on PATH and spends API; run manually"]
    async fn live_pi_smoke_second_turn_recalls_the_first() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let params = ChatSessionParams::builder("", temp.path(), &sessions).build();
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);

        let mut session = PiChatSession::spawn(&params, tx).await.unwrap();

        session
            .send_turn("Reply with exactly the word: ping".to_string())
            .await
            .unwrap();
        let mut text = String::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(120), rx.recv())
                .await
                .expect("timed out waiting for pi")
                .expect("pi event channel closed")
            {
                ChatEvent::Delta {
                    role: ChatRole::Assistant,
                    text: delta,
                } => text.push_str(&delta),
                ChatEvent::TurnFinished { ok, error } => {
                    assert!(ok, "first turn failed: {error:?}");
                    break;
                }
                ChatEvent::SessionClosed { error } => panic!("pi died: {error:?}"),
                _ => {}
            }
        }
        assert!(text.to_lowercase().contains("ping"), "got: {text}");

        // Session continuity: the second turn must see the first.
        session
            .send_turn(
                "What word did I just ask you to reply with? Answer with that word only."
                    .to_string(),
            )
            .await
            .unwrap();
        let mut recall = String::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(120), rx.recv())
                .await
                .expect("timed out waiting for pi")
                .expect("pi event channel closed")
            {
                ChatEvent::Delta {
                    role: ChatRole::Assistant,
                    text: delta,
                } => recall.push_str(&delta),
                ChatEvent::TurnFinished { ok, error } => {
                    assert!(ok, "second turn failed: {error:?}");
                    break;
                }
                ChatEvent::SessionClosed { error } => panic!("pi died: {error:?}"),
                _ => {}
            }
        }
        assert!(recall.to_lowercase().contains("ping"), "got: {recall}");

        ChatSession::close(Box::new(session)).await.unwrap();
    }
}
