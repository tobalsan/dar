//! Codex chat backend: one long-lived `codex app-server` JSON-RPC child per
//! TUI chat session.

use std::collections::{HashSet, VecDeque};
use std::ffi::OsString;
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

const CLOSE_WAIT: Duration = Duration::from_secs(2);
const KILL_GRACE: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub struct ChatCodexExtension;

impl Extension for ChatCodexExtension {
    fn id(&self) -> &'static str {
        "chat-codex"
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            ctx.services
                .service::<dyn ChatBackend>("codex", Arc::new(CodexChatBackend))?;
            Ok(())
        })
    }
}

pub struct CodexChatBackend;

impl ChatBackend for CodexChatBackend {
    fn open<'a>(
        &'a self,
        params: ChatSessionParams,
        tx: Sender<ChatEvent>,
    ) -> cap_chat::BoxFuture<'a, Result<Box<dyn ChatSession>>> {
        Box::pin(async move {
            let session = CodexChatSession::spawn(&params, tx).await?;
            Ok(Box::new(session) as Box<dyn ChatSession>)
        })
    }
}

pub struct CodexChatSession {
    pid: u32,
    workspace: String,
    model: Option<String>,
    thread_id: String,
    next_id: u64,
    tx: Sender<ChatEvent>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    queue: Arc<Mutex<TurnQueue>>,
    closing: Arc<AtomicBool>,
    wait_handle: tokio::task::JoinHandle<()>,
}

#[derive(Default)]
struct TurnQueue {
    busy: bool,
    pending: VecDeque<Turn>,
    in_flight: HashSet<(String, String)>,
    pending_interrupt: Option<u64>,
}

struct Turn {
    request_id: u64,
    prompt: String,
    workspace: String,
    model: Option<String>,
}

/// codex reads credentials/config from `$CODEX_HOME`, defaulting to `~/.codex`.
/// The chat backend points CODEX_HOME at an isolated per-session dir, so we
/// resolve the *host* location here to copy global auth into the sandbox.
fn host_codex_home() -> Option<std::path::PathBuf> {
    if let Some(home) = std::env::var_os("CODEX_HOME") {
        let p = std::path::PathBuf::from(home);
        if !p.as_os_str().is_empty() {
            return Some(p);
        }
    }
    std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".codex"))
}

/// Copy the host's global codex credentials into the isolated CODEX_HOME so the
/// chat child authenticates. Only `auth.json` is seeded — NOT `config.toml`,
/// which would drag the user's plugins/skills/MCP/personality into the chat
/// session (the model recites skill docs instead of replying). The model comes
/// from `agent.yaml` via the `-c model=` flag, so no host config is needed.
/// Best-effort: a missing source file is skipped, not an error.
fn seed_codex_auth(session_dir: &std::path::Path) -> Result<()> {
    let Some(src) = host_codex_home() else {
        return Ok(());
    };
    for file in ["auth.json"] {
        let from = src.join(file);
        if from.exists() {
            std::fs::copy(&from, session_dir.join(file))
                .with_context(|| format!("seeding codex {file}"))?;
        }
    }
    Ok(())
}

impl CodexChatSession {
    async fn spawn(params: &ChatSessionParams, tx: Sender<ChatEvent>) -> Result<Self> {
        host_api::assert_contained(&params.agent_root, &params.session_dir)
            .context("session_dir containment check failed; refusing to spawn codex chat")?;
        std::fs::create_dir_all(&params.session_dir)?;
        seed_codex_auth(&params.session_dir)?;

        let command = effective_command(&params.command, "codex");
        let mut cmd = Command::new(&command);
        cmd.args(codex_args(params));
        scrub_loaded_env(&mut cmd);
        cmd.env("CODEX_HOME", &params.session_dir);
        cmd.current_dir(&params.agent_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        setup_process_group(&mut cmd);

        let mut child = cmd.spawn().with_context(|| {
            format!(
                "spawning `{}` in {}",
                command.to_string_lossy(),
                params.agent_root.display()
            )
        })?;
        let pid = child
            .id()
            .context("codex chat child has no pid immediately after spawn")?;
        let mut stdin = child.stdin.take().context("codex chat stdin not piped")?;
        let stdout = child.stdout.take().context("codex chat stdout not piped")?;
        let stderr = child.stderr.take().context("codex chat stderr not piped")?;
        let workspace = params.agent_root.to_string_lossy().into_owned();

        let mut lines = BufReader::new(stdout).lines();
        send_value(&mut stdin, make_initialize(1, &workspace)).await?;
        wait_for_response(&mut lines, 1).await?;
        send_value(&mut stdin, make_initialized()).await?;
        send_value(&mut stdin, make_thread_start(2, &workspace, params.model.as_deref())).await?;
        let thread_result = wait_for_response(&mut lines, 2).await?;
        let thread_id = extract_thread_id(&thread_result)
            .with_context(|| format!("thread/start: no thread.id in result: {thread_result}"))?;

        let stdin = Arc::new(Mutex::new(Some(stdin)));
        let queue = Arc::new(Mutex::new(TurnQueue::default()));
        spawn_stdout_pump(
            lines,
            tx.clone(),
            Arc::clone(&stdin),
            Arc::clone(&queue),
            thread_id.clone(),
        );
        spawn_stderr_pump(stderr, tx.clone());

        let closing = Arc::new(AtomicBool::new(false));
        let wait_handle = {
            let closing = Arc::clone(&closing);
            let tx = tx.clone();
            tokio::spawn(async move {
                let status = child.wait().await;
                if !closing.load(Ordering::SeqCst) {
                    let error = match status {
                        Ok(status) => format!("codex exited unexpectedly: {status}"),
                        Err(e) => format!("codex wait failed: {e}"),
                    };
                    let _ = tx
                        .send(ChatEvent::SessionClosed { error: Some(error) })
                        .await;
                }
            })
        };

        Ok(Self {
            pid,
            workspace,
            model: params.model.clone(),
            thread_id,
            next_id: 3,
            tx,
            stdin,
            queue,
            closing,
            wait_handle,
        })
    }

    async fn accept_turn(&self, turn: Turn) -> Result<()> {
        {
            let mut queue = self.queue.lock().await;
            if queue.busy {
                queue.pending.push_back(turn);
                return Ok(());
            }
            queue.busy = true;
        }
        if let Err(error) = write_turn(
            &self.stdin,
            turn.request_id,
            &self.thread_id,
            &self.workspace,
            &turn.prompt,
            self.model.as_deref(),
        )
        .await
        {
            self.queue.lock().await.busy = false;
            return Err(error);
        }
        Ok(())
    }
}

impl ChatSession for CodexChatSession {
    fn send_turn(&mut self, prompt: String) -> cap_chat::BoxFuture<'_, Result<()>> {
        let request_id = self.next_id;
        self.next_id += 1;
        Box::pin(async move {
            self.accept_turn(Turn {
                request_id,
                prompt,
                workspace: self.workspace.clone(),
                model: self.model.clone(),
            })
            .await
        })
    }

    fn abort(&mut self) -> cap_chat::BoxFuture<'_, Result<()>> {
        let request_id = self.next_id;
        self.next_id += 1;
        Box::pin(async move {
            if let Some(turn_id) = current_main_turn_id(&self.queue, &self.thread_id).await {
                write_value_to(
                    &self.stdin,
                    make_turn_interrupt(request_id, &self.thread_id, &turn_id),
                )
                .await?;
            } else {
                self.queue.lock().await.pending_interrupt = Some(request_id);
            }
            abort_queued_turns(&self.queue, &self.tx).await;
            Ok(())
        })
    }

    fn close(self: Box<Self>) -> cap_chat::BoxFuture<'static, Result<()>> {
        let this = *self;
        Box::pin(async move {
            this.closing.store(true, Ordering::SeqCst);
            this.stdin.lock().await.take();
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

fn codex_args(params: &ChatSessionParams) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("app-server"),
        OsString::from("-c"),
        OsString::from("approval_policy=\"never\""),
        OsString::from("-c"),
        OsString::from("sandbox_permissions=[\"disk-full-read-access\", \"disk-write-access\"]"),
    ];
    if let Some(model) = &params.model {
        args.push(OsString::from("-c"));
        args.push(OsString::from(format!("model={model:?}")));
    }
    if let Some(provider) = &params.provider {
        args.push(OsString::from("-c"));
        args.push(OsString::from(format!("model_provider={provider:?}")));
    }
    // Wire the host MCP bridge as a codex MCP server on the same app-server
    // invocation so the chat agent calls the same registry tools an issue
    // worker does; results return inside the same thread/turn. Reuses the
    // runner's `-c mcp_servers.agentropy.*` writer.
    if let Some(bridge) = &params.host_tool_bridge {
        args.extend(runner_core::codex_mcp_bridge_args(bridge));
    }
    args
}

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
        }
    })
}

fn make_turn_interrupt(id: u64, thread_id: &str, turn_id: &str) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "turn/interrupt",
        "params": { "threadId": thread_id, "turnId": turn_id }
    })
}

async fn send_value(stdin: &mut ChildStdin, value: Value) -> Result<()> {
    let line = value.to_string() + "\n";
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await?;
    Ok(())
}

async fn write_value_to(stdin: &Arc<Mutex<Option<ChildStdin>>>, value: Value) -> Result<()> {
    let mut guard = stdin.lock().await;
    let stdin = guard.as_mut().context("codex chat session stdin is closed")?;
    send_value(stdin, value).await
}

async fn write_turn(
    stdin: &Arc<Mutex<Option<ChildStdin>>>,
    request_id: u64,
    thread_id: &str,
    workspace: &str,
    prompt: &str,
    model: Option<&str>,
) -> Result<()> {
    write_value_to(
        stdin,
        make_turn_start(request_id, thread_id, workspace, prompt, model),
    )
    .await
}

async fn wait_for_response(
    lines: &mut tokio::io::Lines<BufReader<ChildStdout>>,
    expected_id: u64,
) -> Result<Value> {
    let deadline = tokio::time::Instant::now() + REQUEST_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        anyhow::ensure!(
            !remaining.is_zero(),
            "request {expected_id} timed out after 30s"
        );
        let line = tokio::time::timeout(remaining, lines.next_line())
            .await
            .with_context(|| format!("request {expected_id} timed out after 30s"))??
            .context("stdout EOF while awaiting response")?;
        let msg: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if msg.get("id").and_then(Value::as_u64) != Some(expected_id) {
            continue;
        }
        if let Some(error) = msg.get("error") {
            anyhow::bail!("request {expected_id} failed: {error}");
        }
        return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
    }
}

fn extract_thread_id(result: &Value) -> Option<String> {
    result
        .get("thread")
        .and_then(|t| t.get("id"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn spawn_stdout_pump(
    mut lines: tokio::io::Lines<BufReader<ChildStdout>>,
    tx: Sender<ChatEvent>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    queue: Arc<Mutex<TurnQueue>>,
    main_thread_id: String,
) {
    tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            let clean = strip_ansi(line.trim_end_matches('\r'));
            if respond_to_server_request(&clean, &stdin).await.is_err() {
                return;
            }
            for event in map_stdout_line(&clean) {
                if tx.send(event).await.is_err() {
                    return;
                }
            }
            if handle_turn_accounting(&clean, &stdin, &queue, &main_thread_id, &tx)
                .await
                .is_err()
            {
                return;
            }
        }
    });
}

async fn respond_to_server_request(
    line: &str,
    stdin: &Arc<Mutex<Option<ChildStdin>>>,
) -> Result<()> {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Ok(());
    };
    if !is_server_request(&value) {
        return Ok(());
    }
    let id = value.get("id").cloned().unwrap_or(Value::Null);
    let method = value.get("method").and_then(Value::as_str).unwrap_or("");
    let response = if method == "item/commandExecution/requestApproval"
        || method == "item/fileChange/requestApproval"
    {
        serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": "cancel" })
    } else {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("method not found: {method}") }
        })
    };
    write_value_to(stdin, response).await
}

async fn handle_turn_accounting(
    line: &str,
    stdin: &Arc<Mutex<Option<ChildStdin>>>,
    queue: &Arc<Mutex<TurnQueue>>,
    main_thread_id: &str,
    tx: &Sender<ChatEvent>,
) -> Result<()> {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Ok(());
    };
    match value.get("method").and_then(Value::as_str) {
        Some("turn/started") => {
            if let Some(key) = turn_key(&value) {
                let pending_interrupt = {
                    let mut queue = queue.lock().await;
                    queue.in_flight.insert(key.clone());
                    if key.0 == main_thread_id {
                        queue.pending_interrupt.take().map(|id| (id, key.1))
                    } else {
                        None
                    }
                };
                if let Some((request_id, turn_id)) = pending_interrupt {
                    write_value_to(
                        stdin,
                        make_turn_interrupt(request_id, main_thread_id, &turn_id),
                    )
                    .await?;
                }
            }
        }
        Some("turn/completed") => {
            if let Some(key) = turn_key(&value) {
                queue.lock().await.in_flight.remove(&key);
            }
            if notification_thread_id(&value).as_deref() == Some(main_thread_id) {
                let status = turn_status(&value);
                if status == Some("completed") {
                    tx.send(ChatEvent::TurnFinished {
                        ok: true,
                        error: None,
                    })
                    .await?;
                    send_next_queued_turn(stdin, queue, tx, main_thread_id).await;
                } else {
                    let error = normalize_turn_error(status);
                    tx.send(ChatEvent::TurnFinished {
                        ok: false,
                        error: Some(error),
                    })
                    .await?;
                    abort_queued_turns(queue, tx).await;
                    queue.lock().await.busy = false;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn map_stdout_line(line: &str) -> Vec<ChatEvent> {
    if line.trim().is_empty() {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return vec![ChatEvent::Error(line.to_string())];
    };
    let method = value.get("method").and_then(Value::as_str);
    if method.is_none() {
        if let Some(error) = value.get("error") {
            return vec![ChatEvent::Error(error_message(error))];
        }
        return Vec::new();
    }
    if method == Some("error") {
        return vec![ChatEvent::Error(error_message(
            value.get("params").unwrap_or(&value),
        ))];
    }
    if is_server_request(&value) {
        return Vec::new();
    }
    let item = value.get("params").and_then(|p| p.get("item"));
    let item_type = item
        .and_then(|i| i.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if method.unwrap_or("").ends_with("/delta") {
        let delta_type = if !item_type.is_empty() {
            item_type
        } else {
            method
                .unwrap_or("")
                .strip_suffix("/delta")
                .and_then(|s| s.rsplit('/').next())
                .unwrap_or("")
        };
        return match delta_type {
            "agentMessage" => text_from_delta(&value)
                .map(|text| ChatEvent::Delta {
                    role: ChatRole::Assistant,
                    text,
                })
                .into_iter()
                .collect(),
            "reasoning" => text_from_delta(&value)
                .map(|text| ChatEvent::Delta {
                    role: ChatRole::Thinking,
                    text,
                })
                .into_iter()
                .collect(),
            _ => Vec::new(),
        };
    }
    if method == Some("item/completed") {
        return map_completed_item(item);
    }
    Vec::new()
}

fn map_completed_item(item: Option<&Value>) -> Vec<ChatEvent> {
    let Some(item) = item else {
        return Vec::new();
    };
    match item.get("type").and_then(Value::as_str).unwrap_or("") {
        "agentMessage" | "reasoning" => Vec::new(),
        "commandExecution" | "fileChange" | "mcpToolCall" | "webSearch" | "dynamicToolCall"
        | "collabToolCall" | "collabAgentToolCall" => tool_events(item),
        _ => Vec::new(),
    }
}

fn tool_events(item: &Value) -> Vec<ChatEvent> {
    let id = item_id(item);
    let name = tool_name(item);
    let args = tool_args(item);
    let output = tool_output(item);
    let mut events = vec![ChatEvent::ToolCall {
        id: id.clone(),
        name,
        args,
    }];
    events.push(ChatEvent::ToolOutput {
        id,
        text: output,
        is_error: tool_is_error(item),
        done: true,
    });
    events
}

fn is_server_request(value: &Value) -> bool {
    value.get("id").is_some() && value.get("method").is_some()
}

fn text_from_delta(value: &Value) -> Option<String> {
    value
        .get("params")
        .and_then(|p| p.get("delta"))
        .or_else(|| value.get("params").and_then(|p| p.get("text")))
        .or_else(|| {
            value
                .get("params")
                .and_then(|p| p.get("item"))
                .and_then(|i| i.get("text"))
        })
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(ToString::to_string)
}

fn join_text_entries(value: Option<&Value>) -> Option<String> {
    let parts = value?
        .as_array()?
        .iter()
        .filter_map(|entry| {
            entry
                .as_str()
                .or_else(|| entry.get("text").and_then(Value::as_str))
                .filter(|text| !text.is_empty())
        })
        .collect::<Vec<_>>();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

fn item_id(item: &Value) -> String {
    item.get("id")
        .or_else(|| item.get("callId"))
        .or_else(|| item.get("toolCallId"))
        .and_then(Value::as_str)
        .unwrap_or("tool")
        .to_string()
}

fn tool_name(item: &Value) -> String {
    match item.get("type").and_then(Value::as_str).unwrap_or("") {
        "commandExecution" => item
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("command")
            .to_string(),
        "mcpToolCall" => match (
            item.get("server").and_then(Value::as_str),
            item.get("tool").and_then(Value::as_str),
        ) {
            (Some(server), Some(tool)) => format!("{server}.{tool}"),
            (_, Some(tool)) => tool.to_string(),
            _ => item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("mcpToolCall")
                .to_string(),
        },
        "fileChange" => item
            .get("filename")
            .or_else(|| item.get("path"))
            .and_then(Value::as_str)
            .unwrap_or("fileChange")
            .to_string(),
        "webSearch" => item
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("webSearch")
            .to_string(),
        other => item
            .get("name")
            .or_else(|| item.get("tool"))
            .and_then(Value::as_str)
            .unwrap_or(other)
            .to_string(),
    }
}

fn tool_args(item: &Value) -> String {
    item.get("arguments")
        .or_else(|| item.get("command"))
        .or_else(|| item.get("query"))
        .or_else(|| item.get("prompt"))
        .map(|value| {
            value
                .as_str()
                .map(ToString::to_string)
                .unwrap_or_else(|| value.to_string())
        })
        .unwrap_or_default()
}

fn tool_output(item: &Value) -> String {
    item.get("aggregatedOutput")
        .or_else(|| item.get("aggregated_output"))
        .or_else(|| item.get("output"))
        .or_else(|| item.get("result"))
        .map(result_text)
        .unwrap_or_default()
}

fn result_text(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    if let Some(content) = value.get("content") {
        if let Some(text) = join_text_entries(Some(content)) {
            return text;
        }
    }
    value.to_string()
}

fn tool_is_error(item: &Value) -> bool {
    item.get("isError")
        .or_else(|| item.get("is_error"))
        .and_then(Value::as_bool)
        .unwrap_or_else(|| {
            item.get("exitCode")
                .or_else(|| item.get("exit_code"))
                .and_then(Value::as_i64)
                .map(|code| code != 0)
                .unwrap_or(false)
        })
}

fn error_message(value: &Value) -> String {
    value
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| value.get("error").and_then(Value::as_str))
        .or_else(|| {
            value
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
        })
        .unwrap_or("server error")
        .to_string()
}

fn notification_thread_id(msg: &Value) -> Option<String> {
    msg.get("params")
        .and_then(|p| p.get("threadId"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn turn_key(msg: &Value) -> Option<(String, String)> {
    let thread_id = notification_thread_id(msg)?;
    let turn_id = msg
        .get("params")
        .and_then(|p| p.get("turn"))
        .and_then(|t| t.get("id"))
        .and_then(Value::as_str)
        .map(ToString::to_string)?;
    Some((thread_id, turn_id))
}

fn turn_status(msg: &Value) -> Option<&str> {
    msg.get("params")
        .and_then(|p| p.get("turn"))
        .and_then(|t| t.get("status"))
        .and_then(Value::as_str)
}

fn normalize_turn_error(status: Option<&str>) -> String {
    match status {
        Some("aborted" | "cancelled" | "canceled" | "interrupted") => "aborted".to_string(),
        Some(status) => status.to_string(),
        None => "failed".to_string(),
    }
}

async fn abort_queued_turns(queue: &Arc<Mutex<TurnQueue>>, tx: &Sender<ChatEvent>) {
    let dropped = {
        let mut queue = queue.lock().await;
        let dropped = queue.pending.len();
        queue.pending.clear();
        dropped
    };
    send_failed_finishes(tx, dropped, "aborted").await;
}

async fn current_main_turn_id(
    queue: &Arc<Mutex<TurnQueue>>,
    thread_id: &str,
) -> Option<String> {
    queue
        .lock()
        .await
        .in_flight
        .iter()
        .find(|(candidate_thread, _)| candidate_thread == thread_id)
        .map(|(_, turn_id)| turn_id.clone())
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
    thread_id: &str,
) {
    let next = {
        let mut queue = queue.lock().await;
        match queue.pending.pop_front() {
            Some(turn) => Some(turn),
            None => {
                queue.busy = false;
                None
            }
        }
    };
    if let Some(turn) = next {
        if write_turn(
            stdin,
            turn.request_id,
            thread_id,
            &turn.workspace,
            &turn.prompt,
            turn.model.as_deref(),
        )
        .await
        .is_err()
        {
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

    use super::*;

    fn one(line: &str) -> ChatEvent {
        let events = map_stdout_line(line);
        assert_eq!(events.len(), 1, "{events:?}");
        events.into_iter().next().unwrap()
    }

    #[test]
    fn maps_assistant_delta() {
        let event = one(
            r#"{"jsonrpc":"2.0","method":"item/delta","params":{"item":{"type":"agentMessage"},"delta":"hi"}}"#,
        );
        match event {
            ChatEvent::Delta { role, text } => {
                assert_eq!(role, ChatRole::Assistant);
                assert_eq!(text, "hi");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn maps_thinking_delta() {
        let event = one(
            r#"{"jsonrpc":"2.0","method":"item/delta","params":{"item":{"type":"reasoning"},"delta":"thinking"}}"#,
        );
        match event {
            ChatEvent::Delta { role, text } => {
                assert_eq!(role, ChatRole::Thinking);
                assert_eq!(text, "thinking");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn maps_new_schema_assistant_delta() {
        let event = one(
            r#"{"jsonrpc":"2.0","method":"item/agentMessage/delta","params":{"threadId":"t","turnId":"u","itemId":"m","delta":"Hi"}}"#,
        );
        match event {
            ChatEvent::Delta { role, text } => {
                assert_eq!(role, ChatRole::Assistant);
                assert_eq!(text, "Hi");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn maps_new_schema_reasoning_delta() {
        let event = one(
            r#"{"jsonrpc":"2.0","method":"item/reasoning/delta","params":{"delta":"thinking"}}"#,
        );
        match event {
            ChatEvent::Delta { role, text } => {
                assert_eq!(role, ChatRole::Thinking);
                assert_eq!(text, "thinking");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn maps_tool_lifecycle_from_completed_command() {
        let events = map_stdout_line(
            r#"{"jsonrpc":"2.0","method":"item/completed","params":{"item":{"id":"cmd-1","type":"commandExecution","command":"cargo test","aggregatedOutput":"ok","exitCode":0}}}"#,
        );
        assert_eq!(events.len(), 2);
        match &events[0] {
            ChatEvent::ToolCall { id, name, args } => {
                assert_eq!(id, "cmd-1");
                assert_eq!(name, "cargo test");
                assert_eq!(args, "cargo test");
            }
            other => panic!("unexpected event: {other:?}"),
        }
        match &events[1] {
            ChatEvent::ToolOutput {
                id,
                text,
                is_error,
                done,
            } => {
                assert_eq!(id, "cmd-1");
                assert_eq!(text, "ok");
                assert!(!is_error);
                assert!(*done);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn completed_tool_with_empty_output_is_marked_done() {
        let events = map_stdout_line(
            r#"{"jsonrpc":"2.0","method":"item/completed","params":{"item":{"id":"cmd-1","type":"commandExecution","command":"true","aggregatedOutput":"","exitCode":0}}}"#,
        );
        assert_eq!(events.len(), 2);
        match &events[1] {
            ChatEvent::ToolOutput {
                id,
                text,
                is_error,
                done,
            } => {
                assert_eq!(id, "cmd-1");
                assert_eq!(text, "");
                assert!(!is_error);
                assert!(*done);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn maps_mcp_tool_result_text() {
        let events = map_stdout_line(
            r#"{"jsonrpc":"2.0","method":"item/completed","params":{"item":{"id":"m1","type":"mcpToolCall","server":"linear","tool":"fetch","arguments":{"id":"ALG-1"},"result":{"content":[{"type":"text","text":"issue body"}]}}}}"#,
        );
        assert_eq!(events.len(), 2);
        match &events[0] {
            ChatEvent::ToolCall { id, name, args } => {
                assert_eq!(id, "m1");
                assert_eq!(name, "linear.fetch");
                assert_eq!(args, r#"{"id":"ALG-1"}"#);
            }
            other => panic!("unexpected event: {other:?}"),
        }
        match &events[1] {
            ChatEvent::ToolOutput { text, .. } => assert_eq!(text, "issue body"),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn maps_error_cases() {
        let error = one(r#"{"jsonrpc":"2.0","method":"error","params":{"message":"boom"}}"#);
        match error {
            ChatEvent::Error(text) => assert_eq!(text, "boom"),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn main_thread_turn_completed_emits_finished_but_subagent_does_not() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let stdin = Arc::new(Mutex::new(None));
        let queue = Arc::new(Mutex::new(TurnQueue::default()));

        handle_turn_accounting(
            r#"{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"sub","turn":{"id":"s1","status":"completed"}}}"#,
            &stdin,
            &queue,
            "main",
            &tx,
        )
        .await
        .unwrap();
        assert!(rx.try_recv().is_err());

        handle_turn_accounting(
            r#"{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"main","turn":{"id":"t1","status":"completed"}}}"#,
            &stdin,
            &queue,
            "main",
            &tx,
        )
        .await
        .unwrap();
        match rx.recv().await.unwrap() {
            ChatEvent::TurnFinished { ok, error } => {
                assert!(ok);
                assert_eq!(error, None);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn interrupted_cancelled_and_aborted_statuses_normalize_to_aborted() {
        for status in ["interrupted", "cancelled", "canceled", "aborted"] {
            assert_eq!(normalize_turn_error(Some(status)), "aborted");
        }
        assert_eq!(normalize_turn_error(Some("failed")), "failed");
        assert_eq!(normalize_turn_error(None), "failed");
    }

    #[test]
    fn turn_interrupt_request_shape_matches_app_server_schema() {
        let req = make_turn_interrupt(9, "thread-1", "turn-1");
        assert_eq!(req["jsonrpc"], "2.0");
        assert_eq!(req["id"], 9);
        assert_eq!(req["method"], "turn/interrupt");
        assert_eq!(req["params"]["threadId"], "thread-1");
        assert_eq!(req["params"]["turnId"], "turn-1");
    }

    #[test]
    fn codex_provider_is_passed_as_config_flag() {
        let root = Path::new("/agent");
        let sessions = Path::new("/agent/data/tui/sessions");
        let params = ChatSessionParams::builder("", root, sessions)
            .model(Some("o4-mini".into()))
            .provider(Some("azure".into()))
            .build();
        let args = codex_args(&params);
        assert!(args.iter().any(|a| a == "-c"), "must contain -c flags");
        let provider_flag_idx = args
            .windows(2)
            .position(|w| w[0] == "-c" && w[1].to_string_lossy().starts_with("model_provider="));
        assert!(
            provider_flag_idx.is_some(),
            "model_provider flag missing: {args:?}"
        );
        let flag = args[provider_flag_idx.unwrap() + 1].to_string_lossy();
        assert!(flag.contains("azure"), "provider value missing: {flag}");
    }

    #[tokio::test]
    async fn registers_chat_backend_under_codex() {
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = RegisterCtx {
            bus: host_api::EventBus::new(),
            http: host_api::HttpRegistry::disabled(),
            foreground: host_api::ForegroundRegistry::default(),
            services: host_api::ServiceRegistry::default(),
            paths: host_api::HostPaths::new(temp.path()).unwrap(),
            config: host_api::ConfigStore::default(),
            shutdown: host_api::ShutdownToken::new(shutdown_rx),
        };
        ChatCodexExtension.register(&mut ctx).await.unwrap();
        assert!(ctx.services.get_named::<dyn ChatBackend>("codex").is_ok());
    }

    fn write_script(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("fake-codex.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    async fn next_event(rx: &mut tokio::sync::mpsc::Receiver<ChatEvent>) -> ChatEvent {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("timed out waiting for ChatEvent")
            .expect("chat event channel closed")
    }

    fn is_alive(pid: u32) -> bool {
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn spawned_stub_streams_a_turn_and_close_reaps_child() {
        let temp = tempfile::tempdir().unwrap();
        let script = write_script(
            temp.path(),
            r#"set -e
while IFS= read -r line; do
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
            printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
            printf '{"jsonrpc":"2.0","method":"turn/started","params":{"threadId":"main","turn":{"id":"turn-1"}}}\n'
            printf '{"jsonrpc":"2.0","method":"item/delta","params":{"item":{"type":"agentMessage"},"delta":"pong"}}\n'
            printf '{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"main","turn":{"id":"turn-1","status":"completed"}}}\n'
            ;;
    esac
done
"#,
        );
        let sessions = temp.path().join("data").join("tui").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let params = ChatSessionParams::builder(script.to_str().unwrap(), temp.path(), &sessions)
            .build();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut session = CodexChatSession::spawn(&params, tx).await.unwrap();
        let pid = session.pid;

        session.send_turn("ping".to_string()).await.unwrap();
        match next_event(&mut rx).await {
            ChatEvent::Delta { role, text } => {
                assert_eq!(role, ChatRole::Assistant);
                assert_eq!(text, "pong");
            }
            other => panic!("unexpected event: {other:?}"),
        }
        match next_event(&mut rx).await {
            ChatEvent::TurnFinished { ok, error } => {
                assert!(ok);
                assert_eq!(error, None);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        ChatSession::close(Box::new(session)).await.unwrap();
        assert!(!is_alive(pid), "codex chat child still alive after close");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn abort_before_turn_started_interrupts_when_turn_id_arrives() {
        let temp = tempfile::tempdir().unwrap();
        let script = write_script(
            temp.path(),
            r#"set -e
while IFS= read -r line; do
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
            printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
            sleep 0.2
            printf '{"jsonrpc":"2.0","method":"turn/started","params":{"threadId":"main","turn":{"id":"turn-1"}}}\n'
            ;;
        turn/interrupt)
            printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
            printf '{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"main","turn":{"id":"turn-1","status":"interrupted"}}}\n'
            ;;
    esac
done
"#,
        );
        let sessions = temp.path().join("data").join("tui").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let params = ChatSessionParams::builder(script.to_str().unwrap(), temp.path(), &sessions)
            .build();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut session = CodexChatSession::spawn(&params, tx).await.unwrap();

        session.send_turn("ping".to_string()).await.unwrap();
        session.abort().await.unwrap();
        match next_event(&mut rx).await {
            ChatEvent::TurnFinished { ok, error } => {
                assert!(!ok);
                assert_eq!(error.as_deref(), Some("aborted"));
            }
            other => panic!("unexpected event: {other:?}"),
        }

        ChatSession::close(Box::new(session)).await.unwrap();
    }
}
