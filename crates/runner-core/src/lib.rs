use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use cap_runner::{
    ExitKind, KillReason, RunnerEventSink, RunnerEventStore, RunnerHandle, SpawnParams,
};
use chrono::Utc;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::oneshot;

pub mod bridge;
pub use bridge::{
    codex_mcp_bridge_args, opencode_mcp_block, pi_mcp_config_args, BridgeInvocation,
    BRIDGE_SERVER_NAME,
};

/// Configure a child command to run in its own process group, so signalling the
/// negative pid reaches the whole subtree.
pub fn setup_process_group(cmd: &mut Command) -> &mut Command {
    cmd.process_group(0)
}

/// Classified output from one protocol line.
pub struct ProtocolLine {
    pub row_type: &'static str,
    pub text: String,
    pub detail: String,
}

// ---------------------------------------------------------------------------
// Structured event logging hook
// ---------------------------------------------------------------------------

/// Structured event logger: `(issue, event, msg)`.
pub type LogHook = fn(&str, &str, &str);

static LOG_HOOK: Mutex<Option<LogHook>> = Mutex::new(None);

/// Install the structured event logger used by the spawn/supervision path
/// (typically the host's `logging::ev`). Falls back to plain `tracing` when
/// unset.
pub fn set_log_hook(hook: LogHook) {
    *LOG_HOOK.lock().expect("log hook mutex poisoned") = Some(hook);
}

/// Emit one structured runner event via the installed hook (or `tracing`).
pub fn log_ev(issue: &str, event: &str, msg: &str) {
    let hook = *LOG_HOOK.lock().expect("log hook mutex poisoned");
    match hook {
        Some(f) => f(issue, event, msg),
        None => tracing::info!(issue = %issue, event = %event, "{msg}"),
    }
}

// ---------------------------------------------------------------------------
// Child env scrubbing (.env-loaded keys never reach spawned children)
// ---------------------------------------------------------------------------

static SCRUBBED_ENV_KEYS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn scrubbed_key_set() -> &'static Mutex<HashSet<String>> {
    SCRUBBED_ENV_KEYS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Record an env key that was loaded from `.env` so child spawns scrub it.
pub fn register_scrubbed_env_key(key: String) {
    scrubbed_key_set().lock().unwrap().insert(key);
}

/// Remove env vars that came from `.env` from a child command environment.
pub fn scrub_loaded_env<C>(cmd: &mut C)
where
    C: EnvRemove,
{
    let Some(keys) = SCRUBBED_ENV_KEYS.get() else {
        return;
    };
    for key in keys.lock().unwrap().iter() {
        cmd.env_remove(key);
    }
}

pub trait EnvRemove {
    fn env_remove<K: AsRef<std::ffi::OsStr>>(&mut self, key: K) -> &mut Self;
}

impl EnvRemove for std::process::Command {
    fn env_remove<K: AsRef<std::ffi::OsStr>>(&mut self, key: K) -> &mut Self {
        std::process::Command::env_remove(self, key)
    }
}

impl EnvRemove for tokio::process::Command {
    fn env_remove<K: AsRef<std::ffi::OsStr>>(&mut self, key: K) -> &mut Self {
        tokio::process::Command::env_remove(self, key)
    }
}

// ---------------------------------------------------------------------------
// Shared backend helpers
// ---------------------------------------------------------------------------

/// Resolve the configured runner command, falling back to the backend default.
pub fn effective_command(command: &str, default: &str) -> OsString {
    if command.trim().is_empty() {
        OsString::from(default)
    } else {
        OsString::from(command)
    }
}

/// `AGENT_*` env vars that every runner receives.
///
/// Full list:
///   - `AGENT_ISSUE_IDENTIFIER` — issue identifier (e.g. `PROJ-42`)
///   - `AGENT_ISSUE_ID`         — same value (alias for back-compat with scripts)
///   - `AGENT_RUN_ID`           — unique run attempt ID
///   - `AGENT_PROJECT_ID`       — project/agent ID
///   - `AGENT_WORKSPACE`        — absolute path to the per-issue workspace dir
///   - `AGENT_WORKSPACE_ROOT`   — absolute path to the shared workspaces root
///   - `AGENT_PROMPT`           — rendered prompt text
///   - `AGENT_WORKER_PROMPT`    — same as `AGENT_PROMPT` (alias)
///   - `AGENT_MODEL`            — model name (only when configured)
///   - `AGENT_WORKER_MODEL`     — same as `AGENT_MODEL` (alias, only when configured)
///   - `AGENT_LINEAR_GRAPHQL_TOOL` — set to `1` when the Linear GraphQL tool is enabled
///   - `AGENT_SESSION_DIR`      — path to the per-issue session directory (session runners only)
pub fn common_env(p: &SpawnParams<'_>) -> Vec<(OsString, OsString)> {
    let mut env = vec![
        (
            OsString::from("AGENT_ISSUE_IDENTIFIER"),
            OsString::from(&p.issue_id),
        ),
        (
            OsString::from("AGENT_ISSUE_ID"),
            OsString::from(&p.issue_id),
        ),
        (OsString::from("AGENT_RUN_ID"), OsString::from(&p.run_id)),
        (
            OsString::from("AGENT_PROJECT_ID"),
            OsString::from(&p.issue_id),
        ),
        (
            OsString::from("AGENT_WORKSPACE"),
            p.workspace.as_os_str().to_os_string(),
        ),
        (
            OsString::from("AGENT_WORKSPACE_ROOT"),
            p.workspace_root.as_os_str().to_os_string(),
        ),
        (OsString::from("AGENT_PROMPT"), OsString::from(&p.prompt)),
        (
            OsString::from("AGENT_WORKER_PROMPT"),
            OsString::from(&p.prompt),
        ),
    ];
    if let Some(model) = &p.model {
        env.push((OsString::from("AGENT_MODEL"), OsString::from(model)));
        env.push((OsString::from("AGENT_WORKER_MODEL"), OsString::from(model)));
    }
    if p.expose_linear_graphql_tool {
        env.push((
            OsString::from("AGENT_LINEAR_GRAPHQL_TOOL"),
            OsString::from("1"),
        ));
    }
    env
}

/// Append `AGENT_SESSION_DIR` to a backend env.
pub fn env_with_session_dir(
    mut env: Vec<(OsString, OsString)>,
    session_dir: &Path,
) -> Vec<(OsString, OsString)> {
    env.push((
        OsString::from("AGENT_SESSION_DIR"),
        session_dir.as_os_str().to_os_string(),
    ));
    env
}

/// Optional worker tool definitions shared by protocol runners (pi, codex).
pub fn worker_tools(p: &SpawnParams<'_>) -> Vec<serde_json::Value> {
    if p.expose_linear_graphql_tool {
        vec![serde_json::json!({
            "name": "linear_graphql",
            "description": "Execute a Linear GraphQL operation against the configured Linear API endpoint.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "GraphQL query or mutation document."
                    },
                    "variables": {
                        "type": "object",
                        "description": "GraphQL variables object."
                    }
                },
                "required": ["query"]
            }
        })]
    } else {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// Generic backend spawn
// ---------------------------------------------------------------------------

/// One backend's fully-built spawn recipe: command/args/stdin/env/session-dir.
/// Backend crates construct this; `spawn_backend` does everything else.
pub struct BackendSpec {
    pub command: OsString,
    pub args: Vec<OsString>,
    pub stdin_payload: Option<Vec<u8>>,
    pub event_kind: &'static str,
    pub session_dir: Option<PathBuf>,
    pub env: Vec<(OsString, OsString)>,
}

/// Spawn an agent child for one issue. Asserts workspace containment, sets up
/// its own process group, pipes the turn request/prompt to stdin, and
/// supervises the child in a background task that streams output and enforces
/// the per-turn timeout.
pub async fn spawn_backend(spec: BackendSpec, p: SpawnParams<'_>) -> Result<RunnerHandle> {
    host_api::assert_contained(p.workspace_root, p.workspace)
        .context("workspace containment check failed; refusing to spawn child")?;

    if let Some(dir) = &spec.session_dir {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating runner session dir {}", dir.display()))?;
    }

    let mut cmd = Command::new(&spec.command);
    for arg in &spec.args {
        cmd.arg(arg);
    }
    scrub_loaded_env(&mut cmd);
    cmd.envs(spec.env);
    cmd.current_dir(p.workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    setup_process_group(&mut cmd);

    let mut child = cmd.spawn().with_context(|| {
        format!(
            "spawning `{}` in {}",
            spec.command.to_string_lossy(),
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
    persist_runner_event(
        p.store.as_ref(),
        Some(&p.run_id),
        &p.issue_id,
        spec.event_kind,
        serde_json::json!({
            "type": "spawn",
            "runner": p.runner_kind,
            "pid": pid,
            "command": spec.command.to_string_lossy(),
            "args": spec.args.iter().map(|a| a.to_string_lossy().to_string()).collect::<Vec<_>>(),
            "session_dir": spec.session_dir.as_ref().map(|d| d.display().to_string()),
        }),
    );

    // Write the turn request / prompt to stdin, then close it (EOF).
    if let (Some(mut stdin), Some(payload)) = (child.stdin.take(), spec.stdin_payload) {
        tokio::spawn(async move {
            let _ = stdin.write_all(&payload).await;
            let _ = stdin.flush().await;
            // drop(stdin) closes the pipe → child sees EOF.
        });
    }

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    if let Some(out) = stdout {
        spawn_line_pump(
            out,
            p.issue_id.clone(),
            p.run_id.clone(),
            "stdout",
            spec.event_kind,
            Arc::clone(&p.events),
            Arc::clone(&p.store),
            Arc::clone(&p.last_event_at),
            log_ev,
        );
    }
    if let Some(err) = stderr {
        spawn_line_pump(
            err,
            p.issue_id.clone(),
            p.run_id.clone(),
            "stderr",
            spec.event_kind,
            Arc::clone(&p.events),
            Arc::clone(&p.store),
            Arc::clone(&p.last_event_at),
            log_ev,
        );
    }

    let (kill_tx, kill_rx) = oneshot::channel::<KillReason>();
    let timeout = Duration::from_millis(p.max_run_timeout_ms);
    let issue_id = p.issue_id.clone();

    let done = tokio::spawn(async move {
        supervise(child, pid, timeout, kill_rx, move |kind, message| {
            log_ev(&issue_id, kind, &message);
        })
        .await
    });

    Ok(RunnerHandle::new(pid, kill_tx, done))
}

fn persist_runner_event(
    store: &dyn RunnerEventStore,
    run_id: Option<&str>,
    issue_id: &str,
    kind: &'static str,
    value: serde_json::Value,
) {
    let payload = value.to_string();
    store.insert_event(run_id, issue_id, kind, &payload, Utc::now());
}

/// Classify a protocol output line into a UI log row type and display text.
/// Tries JSON parsing first (for pi/codex protocol events); falls back
/// to text heuristics for plain text output.
pub fn classify_protocol_line(stream: &str, text: &str) -> ProtocolLine {
    if stream == "stderr" {
        return ProtocolLine { row_type: "error", text: text.to_string(), detail: String::new() };
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
        // Codex JSON-RPC path: has "method" OR has "id" + ("result"|"error") without top-level "type"
        let is_jsonrpc = value.get("method").is_some()
            || (value.get("id").is_some()
                && (value.get("result").is_some() || value.get("error").is_some())
                && value.get("type").is_none());
        if is_jsonrpc {
            return classify_jsonrpc_line(&value);
        }
        // Pi JSONL path: top-level "type" with one of the known pi event kinds.
        // Recognized here so deltas collapse to liveness-only and thinking /
        // tool-call / tool-output get proper row_type + text. Unknown pi event
        // types fall through to the default JSON classifier.
        if let Some(pi) = classify_pi_line(&value) {
            return pi;
        }
        let row_type = map_event_type(&value);
        let display = extract_display_text(&value).unwrap_or_else(|| text.to_string());
        ProtocolLine { row_type, text: display, detail: String::new() }
    } else {
        ProtocolLine { row_type: normalize_log_row(stream, text), text: text.to_string(), detail: String::new() }
    }
}

/// Whether `v` looks like a pi JSONL protocol event. Heuristic: a JSON object
/// with a top-level `type` string that is one of the well-known pi event
/// families. Returns `false` for generic JSON like `{"type":"assistant",...}`
/// (the default classifier handles those).
fn is_pi_event(v: &serde_json::Value) -> bool {
    let Some(t) = v.get("type").and_then(serde_json::Value::as_str) else {
        return false;
    };
    matches!(
        t,
        "message_update"
            | "message_start"
            | "message_end"
            | "turn_start"
            | "turn_end"
            | "agent_start"
            | "agent_end"
            | "tool_execution_start"
            | "tool_execution_update"
            | "tool_execution_end"
            | "queue_update"
            | "compaction_start"
            | "compaction_end"
            | "auto_retry_start"
            | "auto_retry_end"
            | "model_change"
            | "thinking_level_change"
            | "session"
            | "custom"
    )
}

/// Classify a pi JSONL event. Returns:
/// - `Some(pl)` with `row_type: ""` for events the dashboard should hide
///   (deltas, lifecycle, queue/compaction/retry noise) — still log/push
///   upstream as a liveness signal if the caller wants.
/// - `Some(pl)` with a real `row_type` for events the dashboard should
///   render as a card (assistant, thinking, tool_call, tool_output, error).
/// - `None` when the event is not a recognized pi shape, so the caller
///   falls through to the default JSON classifier.
fn classify_pi_line(v: &serde_json::Value) -> Option<ProtocolLine> {
    if !is_pi_event(v) {
        return None;
    }
    let t = v.get("type").and_then(serde_json::Value::as_str).unwrap_or("");
    let pl = match t {
        // Turn boundary / lifecycle: hidden. `runner-pi`'s turn loop also
        // watches these for `agent_end` to end the current turn.
        "agent_start" | "agent_end" | "turn_start" | "turn_end" => hidden(),
        // Queue / compaction / retry: noise.
        "queue_update"
        | "compaction_start"
        | "compaction_end"
        | "auto_retry_start"
        | "auto_retry_end"
        | "model_change"
        | "thinking_level_change"
        | "session"
        | "custom" => hidden(),
        // message_start: emit only the user-side content; assistant start
        // produces no text yet (deltas and the matching message_end carry
        // the body).
        "message_start" => classify_message_start(v),
        // message_end: assistant gets a card with the assembled text; user
        // prompts are also surfaced.
        "message_end" => classify_message_end(v),
        // message_update: a single nested `assistantMessageEvent` whose
        // `type` selects the row.
        "message_update" => classify_message_update(v),
        // tool_execution_*: the final `end` carries the result; the
        // intermediate `start` and `update` are liveness / partial-result
        // churn we don't want to render.
        "tool_execution_start" | "tool_execution_update" => hidden(),
        "tool_execution_end" => classify_tool_execution_end(v),
        _ => return None,
    };
    Some(pl)
}

fn hidden() -> ProtocolLine {
    ProtocolLine { row_type: "", text: String::new(), detail: String::new() }
}

fn classify_message_start(v: &serde_json::Value) -> ProtocolLine {
    let role = role_of(v);
    match role.as_deref() {
        Some("user") => message_text(v).map_or_else(hidden, |text| ProtocolLine {
            row_type: "user",
            text,
            detail: String::new(),
        }),
        _ => hidden(),
    }
}

fn classify_message_end(v: &serde_json::Value) -> ProtocolLine {
    let role = role_of(v);
    let text = match message_text(v) {
        Some(t) if !t.is_empty() => t,
        _ => return hidden(),
    };
    match role.as_deref() {
        Some("assistant") => ProtocolLine { row_type: "assistant", text, detail: String::new() },
        Some("user") => ProtocolLine { row_type: "user", text, detail: String::new() },
        _ => hidden(),
    }
}

fn classify_message_update(v: &serde_json::Value) -> ProtocolLine {
    let Some(inner) = v.get("assistantMessageEvent").and_then(serde_json::Value::as_object) else {
        return hidden();
    };
    let inner_type = inner.get("type").and_then(serde_json::Value::as_str).unwrap_or("");
    match inner_type {
        // Streaming tokens: not interesting as a card, but the caller still
        // bumps `last_event_at` and pushes a line to the live log.
        "text_delta" | "thinking_delta" | "toolcall_delta" | "signature_delta" => hidden(),
        "thinking" | "thinking_end" => {
            let text = inner
                .get("thinking")
                .or_else(|| inner.get("text"))
                .or_else(|| inner.get("delta"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
            if text.is_empty() {
                hidden()
            } else {
                ProtocolLine { row_type: "thinking", text: strip_ansi(&text), detail: String::new() }
            }
        }
        "toolcall_start" | "toolcall_end" => {
            let call = inner.get("toolCall").and_then(serde_json::Value::as_object);
            let name = call
                .and_then(|c| c.get("name").and_then(serde_json::Value::as_str))
                .or_else(|| inner.get("name").and_then(serde_json::Value::as_str))
                .unwrap_or("tool")
                .to_string();
            let detail = call
                .and_then(|c| c.get("arguments"))
                .map(|a| if a.is_string() { a.as_str().unwrap().to_string() } else { a.to_string() })
                .unwrap_or_default();
            ProtocolLine { row_type: "tool_call", text: name, detail: strip_ansi(&detail) }
        }
        "error" => {
            let msg = inner
                .get("error")
                .and_then(serde_json::Value::as_str)
                .or_else(|| inner.get("reason").and_then(serde_json::Value::as_str))
                .unwrap_or("assistant error")
                .to_string();
            ProtocolLine { row_type: "error", text: msg, detail: String::new() }
        }
        _ => hidden(),
    }
}

fn classify_tool_execution_end(v: &serde_json::Value) -> ProtocolLine {
    let result = v.get("result").and_then(serde_json::Value::as_object);
    let mut text = result
        .and_then(|r| r.get("content"))
        .and_then(|c| join_text_entries(Some(c)))
        .unwrap_or_default();
    if text.is_empty() {
        if let Some(t) = v.get("partialResult").and_then(serde_json::Value::as_str) {
            text = t.to_string();
        } else if let Some(t) = v.get("resultText").and_then(serde_json::Value::as_str) {
            text = t.to_string();
        }
    }
    if text.is_empty() {
        return hidden();
    }
    let is_error = v
        .get("isError")
        .and_then(serde_json::Value::as_bool)
        .or_else(|| result.and_then(|r| r.get("isError")).and_then(serde_json::Value::as_bool))
        .unwrap_or(false);
    if is_error {
        return ProtocolLine { row_type: "error", text: strip_ansi(&text), detail: String::new() };
    }
    ProtocolLine { row_type: "tool_output", text: strip_ansi(&text), detail: String::new() }
}

/// Classify an opencode SSE event payload (the UNWRAPPED value returned by the
/// runner's `event_payload`, shaped `{id, type, properties:{sessionID, part}}`).
///
/// Returns:
/// - `Some(pl)` with `row_type: ""` for anything the dashboard should hide
///   (streaming text/reasoning still in flight, pending/running tools, lifecycle
///   `step-*` events, and any non-`message.part.updated` event such as
///   `session.idle` / permission churn). The caller still pushes these to the
///   live log as a liveness signal but skips the store insert.
/// - `Some(pl)` with a real `row_type` for a finalized part that should render
///   as a card (`assistant`, `thinking`, `tool_call`, `error`). The matching
///   `tool_output` row for a completed tool is emitted separately by the caller.
/// - `None` is never returned: an unrecognized payload collapses to hidden so a
///   raw JSON dump never reaches the dashboard.
pub fn classify_opencode_event(payload: &serde_json::Value) -> Option<ProtocolLine> {
    let inner_type = payload.get("type").and_then(serde_json::Value::as_str);
    if inner_type != Some("message.part.updated") {
        return Some(hidden());
    }
    let Some(part) = payload
        .get("properties")
        .and_then(|p| p.get("part"))
        .and_then(serde_json::Value::as_object)
    else {
        return Some(hidden());
    };
    let part_type = part.get("type").and_then(serde_json::Value::as_str).unwrap_or("");
    let pl = match part_type {
        "text" => {
            if part_time_ended(part) {
                let text = part.get("text").and_then(serde_json::Value::as_str).unwrap_or("");
                ProtocolLine { row_type: "assistant", text: strip_ansi(text), detail: String::new() }
            } else {
                hidden()
            }
        }
        "reasoning" => {
            if part_time_ended(part) {
                let text = part.get("text").and_then(serde_json::Value::as_str).unwrap_or("");
                ProtocolLine { row_type: "thinking", text: strip_ansi(text), detail: String::new() }
            } else {
                hidden()
            }
        }
        "tool" => {
            let state = part.get("state").and_then(serde_json::Value::as_object);
            let status = state
                .and_then(|s| s.get("status"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            match status {
                "completed" => {
                    let tool = part.get("tool").and_then(serde_json::Value::as_str).unwrap_or("tool");
                    let detail = state
                        .and_then(|s| s.get("input"))
                        .map(serde_json::Value::to_string)
                        .unwrap_or_default();
                    ProtocolLine { row_type: "tool_call", text: strip_ansi(tool), detail }
                }
                "error" => {
                    let msg = state
                        .and_then(|s| s.get("output").and_then(serde_json::Value::as_str))
                        .or_else(|| state.and_then(|s| s.get("error").and_then(serde_json::Value::as_str)))
                        .unwrap_or("error");
                    ProtocolLine { row_type: "error", text: strip_ansi(msg), detail: String::new() }
                }
                _ => hidden(),
            }
        }
        _ => hidden(),
    };
    Some(pl)
}

/// Whether an opencode part has a non-null `time.end`, marking it as finalized
/// (text/reasoning stream `time.end` only once the part is complete).
fn part_time_ended(part: &serde_json::Map<String, serde_json::Value>) -> bool {
    part.get("time")
        .and_then(|t| t.get("end"))
        .is_some_and(|e| !e.is_null())
}

fn role_of(v: &serde_json::Value) -> Option<String> {
    v.get("message")
        .and_then(|m| m.get("role"))
        .and_then(serde_json::Value::as_str)
        .map(|s| s.to_string())
}

/// Extract the assembled text from a pi `message` envelope
/// (`{type: "message_end", message: {role, content: [{type:"text", text:...}, ...]}}`).
/// Joins all `text`-typed content entries; ignores tool-call / image / etc.
fn message_text(v: &serde_json::Value) -> Option<String> {
    join_text_entries(v.get("message").and_then(|m| m.get("content")))
}

/// Join text entries from a JSON array. Each entry may be a plain string or an
/// object with a `"text"` field. Returns `None` when the array is absent or all
/// entries produce empty text.
fn join_text_entries(arr: Option<&serde_json::Value>) -> Option<String> {
    let parts: Vec<&str> = arr?
        .as_array()?
        .iter()
        .filter_map(|entry| {
            if let Some(s) = entry.as_str() {
                if !s.is_empty() { Some(s) } else { None }
            } else {
                entry.get("text").and_then(|t| t.as_str()).filter(|s| !s.is_empty())
            }
        })
        .collect();
    if parts.is_empty() { None } else { Some(parts.join("\n")) }
}

fn classify_jsonrpc_line(v: &serde_json::Value) -> ProtocolLine {
    let method = v.get("method").and_then(|m| m.as_str());
    let params = v.get("params");

    // RPC response: has "id" but no "method"
    if method.is_none() {
        if let Some(err) = v.get("error") {
            let msg = err.get("message").and_then(|m| m.as_str())
                .unwrap_or("rpc error")
                .to_string();
            return ProtocolLine { row_type: "error", text: msg, detail: String::new() };
        }
        // id+result or anything else with id → hidden
        return ProtocolLine { row_type: "", text: String::new(), detail: String::new() };
    }

    let method = method.unwrap();
    let params_obj = params.and_then(|p| p.as_object());

    // Extract item and item_type from params
    let item = params_obj.and_then(|p| p.get("item")).and_then(|i| i.as_object());
    let item_type = item.and_then(|i| i.get("type")).and_then(|t| t.as_str()).unwrap_or("");

    // Hidden conditions
    if method.starts_with("turn/")
        || method.starts_with("thread/")
        || method.starts_with("remoteControl/")
        || method.ends_with("/started")
        || method.ends_with("/created")
        || method.ends_with("/delta")
        || method.contains("/delta/")
        || method == "item/started"
    {
        return ProtocolLine { row_type: "", text: String::new(), detail: String::new() };
    }

    // agentMessage that is NOT /completed → hidden
    if item_type == "agentMessage" && !method.ends_with("/completed") {
        return ProtocolLine { row_type: "", text: String::new(), detail: String::new() };
    }

    // error notification
    if method == "error" {
        let msg = params_obj
            .and_then(|p| p.get("error"))
            .and_then(|e| e.get("message").and_then(|m| m.as_str()))
            .or_else(|| params_obj.and_then(|p| p.get("message")).and_then(|m| m.as_str()))
            .unwrap_or("server error");
        return ProtocolLine { row_type: "error", text: msg.to_string(), detail: String::new() };
    }
    if let Some(p) = params_obj {
        if p.contains_key("error") {
            let msg = p.get("error")
                .and_then(|e| e.get("message").and_then(|m| m.as_str()))
                .unwrap_or("server error");
            return ProtocolLine { row_type: "error", text: msg.to_string(), detail: String::new() };
        }
    }

    // assistant: /completed + agentMessage
    if method.ends_with("/completed") && item_type == "agentMessage" {
        let text = item
            .and_then(|i| i.get("text").and_then(|t| t.as_str()))
            .unwrap_or("")
            .to_string();
        return ProtocolLine { row_type: "assistant", text: strip_ansi(&text), detail: String::new() };
    }

    // thinking: summary/content are arrays of strings or {text} objects
    if item_type == "reasoning" {
        let item_val = item.map(|i| serde_json::Value::Object(i.clone()));
        let text = item
            .and_then(|i| i.get("text").and_then(|t| t.as_str()))
            .map(|s| s.to_string())
            .or_else(|| join_text_entries(item_val.as_ref().and_then(|v| v.get("summary"))))
            .or_else(|| join_text_entries(item_val.as_ref().and_then(|v| v.get("content"))))
            .unwrap_or_default();
        if text.is_empty() {
            return ProtocolLine { row_type: "", text: String::new(), detail: String::new() };
        }
        return ProtocolLine { row_type: "thinking", text: strip_ansi(&text), detail: String::new() };
    }

    // userMessage: content is array of {type:"text", text:…}
    if item_type == "userMessage" {
        let item_val = item.map(|i| serde_json::Value::Object(i.clone()));
        let text = join_text_entries(item_val.as_ref().and_then(|v| v.get("content")))
            .unwrap_or_default();
        if text.is_empty() {
            return ProtocolLine { row_type: "", text: String::new(), detail: String::new() };
        }
        return ProtocolLine { row_type: "user", text: strip_ansi(&text), detail: String::new() };
    }

    // tool_call
    let tool_types = ["commandExecution", "fileChange", "mcpToolCall", "webSearch", "dynamicToolCall", "collabToolCall", "collabAgentToolCall"];
    if tool_types.contains(&item_type) {
        let (text, detail) = if item_type == "commandExecution" {
            let cmd = item.and_then(|i| i.get("command").and_then(|c| c.as_str())).unwrap_or("").to_string();
            // camelCase first, snake_case fallback
            let raw_out = item.and_then(|i| {
                i.get("aggregatedOutput")
                    .or_else(|| i.get("aggregated_output"))
                    .and_then(|o| o.as_str())
            }).unwrap_or("").to_string();
            let exit_code = item.and_then(|i| {
                i.get("exitCode")
                    .or_else(|| i.get("exit_code"))
                    .and_then(|e| e.as_i64())
            });
            let mut out = raw_out;
            if let Some(code) = exit_code {
                if code != 0 {
                    out.push_str(&format!(" [exit {}]", code));
                }
            }
            (strip_ansi(&cmd), strip_ansi(&out))
        } else if item_type == "mcpToolCall" {
            let server = item.and_then(|i| i.get("server").and_then(|s| s.as_str()));
            let tool = item.and_then(|i| i.get("tool").and_then(|t| t.as_str()));
            let label = match (server, tool) {
                (Some(s), Some(t)) => format!("{}.{}", s, t),
                (None, Some(t)) => t.to_string(),
                _ => item.and_then(|i| i.get("name").or_else(|| i.get("toolName")).and_then(|n| n.as_str())).unwrap_or(item_type).to_string(),
            };
            let item_val = item.map(|i| serde_json::Value::Object(i.clone()));
            let detail = item_val.as_ref()
                .and_then(|v| v.get("result"))
                .and_then(|r| r.get("content"))
                .and_then(|c| join_text_entries(Some(c)))
                .or_else(|| {
                    item.and_then(|i| i.get("arguments")).map(|a| {
                        serde_json::to_string(a).unwrap_or_default()
                    })
                })
                .unwrap_or_default();
            (strip_ansi(&label), strip_ansi(&detail))
        } else if item_type == "collabAgentToolCall" {
            let label = item.and_then(|i| i.get("tool").and_then(|t| t.as_str())).unwrap_or(item_type).to_string();
            let detail = item.and_then(|i| i.get("prompt").and_then(|p| p.as_str())).unwrap_or("").to_string();
            (strip_ansi(&label), strip_ansi(&detail))
        } else {
            let label = match item_type {
                "fileChange" => item.and_then(|i| i.get("filename").or_else(|| i.get("path")).and_then(|n| n.as_str())).unwrap_or(item_type).to_string(),
                "webSearch" => item.and_then(|i| i.get("query").and_then(|q| q.as_str())).unwrap_or(item_type).to_string(),
                _ => item.and_then(|i| i.get("name").and_then(|n| n.as_str())).unwrap_or(item_type).to_string(),
            };
            let detail = item.and_then(|i| i.get("output").or_else(|| i.get("result")))
                .map(|v| if v.is_string() { v.as_str().unwrap().to_string() } else { v.to_string() })
                .unwrap_or_default();
            (strip_ansi(&label), strip_ansi(&detail))
        };
        return ProtocolLine { row_type: "tool_call", text, detail };
    }

    // Default: hidden for unrecognized
    ProtocolLine { row_type: "", text: String::new(), detail: String::new() }
}

/// Map a parsed JSON protocol event's `type` field to a normalized UI row type.
/// Handles direct events and JSON-RPC response envelopes (`result.*`).
pub fn map_event_type(v: &serde_json::Value) -> &'static str {
    match v.get("type").and_then(|t| t.as_str()) {
        Some("assistant") => "assistant",
        Some("thinking") | Some("thought") => "thinking",
        Some("user") => "user",
        Some("tool_use") | Some("tool_call") => "tool_call",
        Some("tool_result") | Some("tool_output") => "tool_output",
        Some("error") => "error",
        _ => {
            if let Some(result) = v.get("result") {
                return map_event_type(result);
            }
            "assistant"
        }
    }
}

/// Extract a human-readable text snippet from a protocol event.
pub fn extract_display_text(v: &serde_json::Value) -> Option<String> {
    if let Some(t) = v.get("text").and_then(|t| t.as_str()) {
        return Some(t.to_string());
    }
    if let Some(c) = v.get("content").and_then(|c| c.as_str()) {
        return Some(c.to_string());
    }
    if let Some(t) = v.get("result").and_then(extract_display_text) {
        return Some(t);
    }
    v.get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first())
        .and_then(|item| item.get("text"))
        .and_then(|t| t.as_str())
        .map(|s| s.to_string())
}

/// Text-based heuristic fallback for non-JSON lines.
pub fn normalize_log_row(stream: &str, text: &str) -> &'static str {
    let lower = text.to_ascii_lowercase();
    if stream == "stderr" || lower.contains("error") || lower.contains("\"type\":\"error\"") {
        "error"
    } else if lower.contains("thinking") || lower.contains("thought") {
        "thinking"
    } else if lower.contains("tool_call") || lower.contains("tool use") {
        "tool_call"
    } else if lower.contains("tool_output") || lower.contains("tool result") {
        "tool_output"
    } else if lower.contains("\"role\":\"user\"") {
        "user"
    } else {
        "assistant"
    }
}

pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// Drive the child: wait for exit, the per-turn timeout, or a kill request.
pub async fn supervise<F>(
    mut child: tokio::process::Child,
    pid: u32,
    timeout: Duration,
    kill_rx: oneshot::Receiver<KillReason>,
    mut log: F,
) -> ExitKind
where
    F: FnMut(&str, String) + Send,
{
    let kind = tokio::select! {
        status = child.wait() => {
            match status {
                Ok(s) if s.success() => {
                    log("exit", "code=0 (normal)".to_string());
                    return ExitKind::Normal;
                }
                Ok(s) => {
                    let code = s.code();
                    log("exit", format!("status={s} (abnormal)"));
                    return ExitKind::Abnormal(code);
                }
                Err(e) => {
                    log("exit", format!("wait error: {e} (abnormal)"));
                    return ExitKind::Abnormal(None);
                }
            }
        }
        _ = tokio::time::sleep(timeout) => {
            log("timeout", "turn_timeout_ms exceeded; killing".to_string());
            ExitKind::Interrupted { reason: "turn_timeout" }
        }
        reason = kill_rx => {
            match reason {
                Ok(KillReason::Timeout) => log("kill", "reason=timeout".to_string()),
                Ok(KillReason::OperatorStop) => log("kill", "reason=operator_stop".to_string()),
                Ok(KillReason::Reconcile) => log("kill", "reason=reconcile".to_string()),
                Err(_) => log("kill", "handle dropped".to_string()),
            }
            ExitKind::Abnormal(None)
        }
    };

    term_then_kill(pid, Duration::from_secs(5));
    let _ = child.wait().await;
    kind
}

/// Stream one byte source line-by-line into an event sink + event store.
/// Each line is ANSI-stripped, classified via JSON parsing (or text heuristic
/// fallback), then stored with a normalized `log_row` type.
#[allow(clippy::too_many_arguments)]
pub fn spawn_line_pump<R, F>(
    reader: R,
    issue_id: String,
    run_id: String,
    stream: &'static str,
    runner_event_kind: &'static str,
    events: std::sync::Arc<dyn RunnerEventSink>,
    store: std::sync::Arc<dyn RunnerEventStore>,
    last_event_at: std::sync::Arc<std::sync::Mutex<chrono::DateTime<Utc>>>,
    mut log: F,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    F: FnMut(&str, &str, &str) + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let ts = Utc::now();
                    let clean = strip_ansi(&line);
                    let pl = classify_protocol_line(stream, &clean);
                    let formatted = format!("child[{issue_id}]: {clean}");
                    events.push(formatted);
                    if let Ok(mut t) = last_event_at.lock() {
                        *t = ts;
                    }
                    log(&issue_id, stream, &clean);
                    let payload = serde_json::json!({
                        "type": "protocol_event",
                        "stream": stream,
                        "log_row": pl.row_type,
                        "text": pl.text,
                        "detail": pl.detail,
                    })
                    .to_string();
                    store.insert_event(Some(&run_id), &issue_id, runner_event_kind, &payload, ts);
                }
                Ok(None) => break,
                Err(e) => {
                    let message = format!("read error: {e}");
                    log(&issue_id, stream, &message);
                    break;
                }
            }
        }
    });
}

/// Send SIGTERM to the child's process group, wait `grace`, then SIGKILL.
pub fn term_then_kill(pid: u32, grace: Duration) {
    let pgid = Pid::from_raw(-(pid as i32));
    let _ = kill(pgid, Signal::SIGTERM);
    std::thread::spawn(move || {
        std::thread::sleep(grace);
        let _ = kill(pgid, Signal::SIGKILL);
    });
}

/// Poll until all PIDs in `pids` are no longer alive, or until `timeout` elapses.
pub fn wait_for_pids_dead(pids: &[u32], timeout: Duration) {
    let deadline = std::time::Instant::now() + timeout;
    let poll_interval = Duration::from_millis(100);
    loop {
        let alive: Vec<u32> = pids
            .iter()
            .copied()
            .filter(|&pid| {
                let p = Pid::from_raw(pid as i32);
                kill(p, None).is_ok()
            })
            .collect();
        if alive.is_empty() {
            break;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            tracing::warn!(
                pids = ?alive,
                "stale PIDs still alive after wait timeout; proceeding anyway"
            );
            break;
        }
        std::thread::sleep(poll_interval.min(remaining));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_is_stripped_before_log_row_normalization() {
        let clean = strip_ansi("\u{1b}[31mtool_call\u{1b}[0m: run");
        assert_eq!(clean, "tool_call: run");
        assert_eq!(normalize_log_row("stdout", &clean), "tool_call");
        assert_eq!(normalize_log_row("stderr", "plain failure"), "error");
    }

    #[test]
    fn json_protocol_events_normalized_by_type_field() {
        let pl = classify_protocol_line("stdout", r#"{"type":"assistant","text":"Hello"}"#);
        assert_eq!(pl.row_type, "assistant");
        assert_eq!(pl.text, "Hello");

        let pl = classify_protocol_line("stdout", r#"{"type":"thinking","text":"hmm"}"#);
        assert_eq!(pl.row_type, "thinking");

        let pl = classify_protocol_line("stdout", r#"{"type":"tool_use","name":"bash"}"#);
        assert_eq!(pl.row_type, "tool_call");

        let pl = classify_protocol_line("stdout", r#"{"type":"tool_result","content":"ok"}"#);
        assert_eq!(pl.row_type, "tool_output");
        assert_eq!(pl.text, "ok");

        let pl = classify_protocol_line("stdout", r#"{"type":"error","message":"oops"}"#);
        assert_eq!(pl.row_type, "error");
    }

    #[test]
    fn stderr_lines_always_classified_as_error() {
        let pl = classify_protocol_line("stderr", r#"{"type":"assistant","text":"x"}"#);
        assert_eq!(pl.row_type, "error");
        let pl = classify_protocol_line("stderr", "plain text");
        assert_eq!(pl.row_type, "error");
    }

    #[test]
    fn jsonrpc_result_unwrapped_for_type_mapping() {
        // JSON-RPC result (no top-level type) → hidden in new classification
        let rpc = r#"{"jsonrpc":"2.0","id":"r1","result":{"type":"assistant","text":"Done"}}"#;
        let pl = classify_protocol_line("stdout", rpc);
        assert_eq!(pl.row_type, "");
    }

    #[test]
    fn non_json_falls_back_to_heuristic() {
        let pl = classify_protocol_line("stdout", "thinking about the problem");
        assert_eq!(pl.row_type, "thinking");
        let pl = classify_protocol_line("stdout", "tool_call: bash");
        assert_eq!(pl.row_type, "tool_call");
    }

    #[test]
    fn jsonrpc_agentmessage_completed_is_assistant() {
        let line = r#"{"jsonrpc":"2.0","method":"item/completed","params":{"item":{"type":"agentMessage","text":"Done with task"}}}"#;
        let pl = classify_protocol_line("stdout", line);
        assert_eq!(pl.row_type, "assistant");
        assert_eq!(pl.text, "Done with task");
    }

    #[test]
    fn jsonrpc_command_execution_camel_case_output_and_exit_code() {
        // real camelCase shape with non-zero exitCode
        let line = r#"{"jsonrpc":"2.0","method":"item/completed","params":{"item":{"type":"commandExecution","command":"ls -la","aggregatedOutput":"total 4\nfile1","exitCode":1}}}"#;
        let pl = classify_protocol_line("stdout", line);
        assert_eq!(pl.row_type, "tool_call");
        assert_eq!(pl.text, "ls -la");
        assert!(pl.detail.contains("total 4"), "detail={}", pl.detail);
        assert!(pl.detail.contains("[exit 1]"), "detail={}", pl.detail);
    }

    #[test]
    fn jsonrpc_command_execution_snake_case_fallback() {
        // old snake_case shape still works
        let line = r#"{"jsonrpc":"2.0","method":"item/completed","params":{"item":{"type":"commandExecution","command":"ls -la","aggregated_output":"total 4\nfile1"}}}"#;
        let pl = classify_protocol_line("stdout", line);
        assert_eq!(pl.row_type, "tool_call");
        assert_eq!(pl.text, "ls -la");
        assert!(pl.detail.contains("total 4"));
    }

    #[test]
    fn jsonrpc_command_execution_exit_zero_no_suffix() {
        let line = r#"{"jsonrpc":"2.0","method":"item/completed","params":{"item":{"type":"commandExecution","command":"echo hi","aggregatedOutput":"hi","exitCode":0}}}"#;
        let pl = classify_protocol_line("stdout", line);
        assert_eq!(pl.row_type, "tool_call");
        assert!(!pl.detail.contains("[exit"), "detail should not have exit suffix: {}", pl.detail);
    }

    #[test]
    fn jsonrpc_item_started_is_hidden() {
        let line = r#"{"jsonrpc":"2.0","method":"item/started","params":{"item":{"type":"agentMessage"}}}"#;
        let pl = classify_protocol_line("stdout", line);
        assert_eq!(pl.row_type, "");
    }

    #[test]
    fn jsonrpc_turn_started_is_hidden() {
        let line = r#"{"jsonrpc":"2.0","method":"turn/started","params":{}}"#;
        let pl = classify_protocol_line("stdout", line);
        assert_eq!(pl.row_type, "");
    }

    #[test]
    fn jsonrpc_id_result_no_type_is_hidden() {
        let line = r#"{"jsonrpc":"2.0","id":1,"result":{"foo":"bar"}}"#;
        let pl = classify_protocol_line("stdout", line);
        assert_eq!(pl.row_type, "");
    }

    #[test]
    fn jsonrpc_id_error_is_error() {
        let line = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"Invalid request"}}"#;
        let pl = classify_protocol_line("stdout", line);
        assert_eq!(pl.row_type, "error");
        assert!(pl.text.contains("Invalid request"));
    }

    #[test]
    fn jsonrpc_delta_is_hidden() {
        let line = r#"{"jsonrpc":"2.0","method":"item/delta","params":{"item":{"type":"agentMessage","delta":{"text":"par"}}}}"#;
        let pl = classify_protocol_line("stdout", line);
        assert_eq!(pl.row_type, "");
    }

    #[test]
    fn jsonrpc_reasoning_empty_arrays_is_hidden() {
        let line = r#"{"jsonrpc":"2.0","method":"item/completed","params":{"item":{"type":"reasoning","summary":[],"content":[]}}}"#;
        let pl = classify_protocol_line("stdout", line);
        assert_eq!(pl.row_type, "");
    }

    #[test]
    fn jsonrpc_reasoning_summary_object_entries() {
        let line = r#"{"jsonrpc":"2.0","method":"item/completed","params":{"item":{"type":"reasoning","summary":[{"text":"thinking..."}],"content":[]}}}"#;
        let pl = classify_protocol_line("stdout", line);
        assert_eq!(pl.row_type, "thinking");
        assert_eq!(pl.text, "thinking...");
    }

    #[test]
    fn jsonrpc_reasoning_falls_back_to_content() {
        let line = r#"{"jsonrpc":"2.0","method":"item/completed","params":{"item":{"type":"reasoning","summary":[],"content":[{"text":"from content"}]}}}"#;
        let pl = classify_protocol_line("stdout", line);
        assert_eq!(pl.row_type, "thinking");
        assert_eq!(pl.text, "from content");
    }

    #[test]
    fn jsonrpc_reasoning_text_field_wins() {
        // if item has a top-level `text` string, it takes priority
        let line = r#"{"jsonrpc":"2.0","method":"item/completed","params":{"item":{"type":"reasoning","text":"direct text","summary":[{"text":"summary text"}]}}}"#;
        let pl = classify_protocol_line("stdout", line);
        assert_eq!(pl.row_type, "thinking");
        assert_eq!(pl.text, "direct text");
    }

    #[test]
    fn jsonrpc_user_message_is_user_row() {
        let line = r#"{"jsonrpc":"2.0","method":"item/completed","params":{"item":{"type":"userMessage","content":[{"type":"text","text":"prompt text"}]}}}"#;
        let pl = classify_protocol_line("stdout", line);
        assert_eq!(pl.row_type, "user");
        assert_eq!(pl.text, "prompt text");
    }

    #[test]
    fn jsonrpc_user_message_empty_is_hidden() {
        let line = r#"{"jsonrpc":"2.0","method":"item/completed","params":{"item":{"type":"userMessage","content":[]}}}"#;
        let pl = classify_protocol_line("stdout", line);
        assert_eq!(pl.row_type, "");
    }

    #[test]
    fn jsonrpc_mcp_tool_call_server_dot_tool_label() {
        let line = r#"{"jsonrpc":"2.0","method":"item/completed","params":{"item":{"type":"mcpToolCall","server":"codex_apps","tool":"linear_fetch","arguments":{"id":"LIN-1"},"result":{"content":[{"type":"text","text":"issue body"}]}}}}"#;
        let pl = classify_protocol_line("stdout", line);
        assert_eq!(pl.row_type, "tool_call");
        assert_eq!(pl.text, "codex_apps.linear_fetch");
        assert_eq!(pl.detail, "issue body");
    }

    #[test]
    fn jsonrpc_mcp_tool_call_no_server_falls_back_to_tool() {
        let line = r#"{"jsonrpc":"2.0","method":"item/completed","params":{"item":{"type":"mcpToolCall","tool":"search","arguments":{"q":"foo"}}}}"#;
        let pl = classify_protocol_line("stdout", line);
        assert_eq!(pl.row_type, "tool_call");
        assert_eq!(pl.text, "search");
        // no result → arguments as JSON
        assert!(pl.detail.contains("foo"), "detail={}", pl.detail);
    }

    #[test]
    fn jsonrpc_collab_agent_tool_call() {
        let line = r#"{"jsonrpc":"2.0","method":"item/completed","params":{"item":{"type":"collabAgentToolCall","tool":"spawnAgent","prompt":"do the thing"}}}"#;
        let pl = classify_protocol_line("stdout", line);
        assert_eq!(pl.row_type, "tool_call");
        assert_eq!(pl.text, "spawnAgent");
        assert_eq!(pl.detail, "do the thing");
    }

    // -----------------------------------------------------------------
    // Pi JSONL protocol events (ALG-236)
    // -----------------------------------------------------------------

    fn pi_pl(text: &str) -> ProtocolLine {
        classify_protocol_line("stdout", text)
    }

    #[test]
    fn pi_text_delta_is_hidden() {
        let line = r#"{"type":"message_update","message":{},"assistantMessageEvent":{"type":"text_delta","delta":"pong"}}"#;
        assert_eq!(pi_pl(line).row_type, "");
    }

    #[test]
    fn pi_thinking_delta_is_hidden_but_full_thinking_is_thinking_row() {
        let delta = r#"{"type":"message_update","message":{},"assistantMessageEvent":{"type":"thinking_delta","delta":"hmm"}}"#;
        assert_eq!(pi_pl(delta).row_type, "");

        let full = r#"{"type":"message_update","message":{},"assistantMessageEvent":{"type":"thinking","thinking":"let me check the file"}}"#;
        let pl = pi_pl(full);
        assert_eq!(pl.row_type, "thinking");
        assert_eq!(pl.text, "let me check the file");
    }

    #[test]
    fn pi_toolcall_end_maps_to_tool_call_with_args() {
        let line = r#"{"type":"message_update","message":{},"assistantMessageEvent":{"type":"toolcall_end","contentIndex":1,"toolCall":{"id":"call_123","name":"bash","arguments":{"command":"ls"}}}}"#;
        let pl = pi_pl(line);
        assert_eq!(pl.row_type, "tool_call");
        assert_eq!(pl.text, "bash");
        assert!(pl.detail.contains("ls"), "detail={}", pl.detail);
    }

    #[test]
    fn pi_toolcall_delta_is_hidden() {
        let line = r#"{"type":"message_update","message":{},"assistantMessageEvent":{"type":"toolcall_delta","delta":{"arguments":"{\"command\":\"ls\"}"}}}"#;
        assert_eq!(pi_pl(line).row_type, "");
    }

    #[test]
    fn pi_tool_execution_end_maps_to_tool_output() {
        let line = r#"{"type":"tool_execution_end","toolCallId":"call_123","toolName":"bash","result":{"content":[{"type":"text","text":"total 48"}]},"isError":false}"#;
        let pl = pi_pl(line);
        assert_eq!(pl.row_type, "tool_output");
        assert_eq!(pl.text, "total 48");
    }

    #[test]
    fn pi_tool_execution_end_with_error_flag_is_error_row() {
        let line = r#"{"type":"tool_execution_end","toolCallId":"c1","result":{"content":[{"type":"text","text":"boom"}]},"isError":true}"#;
        let pl = pi_pl(line);
        assert_eq!(pl.row_type, "error");
        assert_eq!(pl.text, "boom");
    }

    #[test]
    fn pi_tool_execution_start_and_update_are_hidden() {
        let start = r#"{"type":"tool_execution_start","toolCallId":"c1","toolName":"bash","args":{"command":"ls"}}"#;
        let update = r#"{"type":"tool_execution_update","toolCallId":"c1","partialResult":"total 4"}"#;
        assert_eq!(pi_pl(start).row_type, "");
        assert_eq!(pi_pl(update).row_type, "");
    }

    #[test]
    fn pi_message_end_assistant_yields_assistant_row_with_full_text() {
        let line = r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"Done with task."}]}}"#;
        let pl = pi_pl(line);
        assert_eq!(pl.row_type, "assistant");
        assert_eq!(pl.text, "Done with task.");
    }

    #[test]
    fn pi_message_end_user_yields_user_row() {
        let line = r#"{"type":"message_end","message":{"role":"user","content":[{"type":"text","text":"please go"}]}}"#;
        let pl = pi_pl(line);
        assert_eq!(pl.row_type, "user");
        assert_eq!(pl.text, "please go");
    }

    #[test]
    fn pi_message_start_assistant_is_hidden_until_end() {
        // Body is delivered in message_end; message_start is just the marker.
        let line = r#"{"type":"message_start","message":{"role":"assistant","content":[]}}"#;
        assert_eq!(pi_pl(line).row_type, "");
    }

    #[test]
    fn pi_message_start_user_emits_user_row() {
        // Prompt text is in the same message envelope, so we surface it.
        let line = r#"{"type":"message_start","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#;
        let pl = pi_pl(line);
        assert_eq!(pl.row_type, "user");
        assert_eq!(pl.text, "hi");
    }

    #[test]
    fn pi_lifecycle_events_are_hidden() {
        for line in [
            r#"{"type":"agent_start"}"#,
            r#"{"type":"agent_end"}"#,
            r#"{"type":"turn_start"}"#,
            r#"{"type":"turn_end"}"#,
            r#"{"type":"queue_update","queued":2}"#,
            r#"{"type":"compaction_start"}"#,
            r#"{"type":"compaction_end"}"#,
            r#"{"type":"auto_retry_start","attempt":1}"#,
            r#"{"type":"model_change","provider":"openai","modelId":"gpt-5"}"#,
            r#"{"type":"thinking_level_change","thinkingLevel":"low"}"#,
            r#"{"type":"session","version":3,"id":"abc"}"#,
            r#"{"type":"custom","customType":"caveman-level","data":{"level":"full"}}"#,
        ] {
            assert_eq!(pi_pl(line).row_type, "", "expected hidden for {line}");
        }
    }

    #[test]
    fn pi_message_update_error_yields_error_row() {
        let line = r#"{"type":"message_update","message":{},"assistantMessageEvent":{"type":"error","reason":"aborted"}}"#;
        let pl = pi_pl(line);
        assert_eq!(pl.row_type, "error");
        assert_eq!(pl.text, "aborted");
    }

    #[test]
    fn pi_unknown_message_update_inner_type_is_hidden() {
        // Some future pi event kind we don't know about: don't pretend it's
        // an assistant card; let the dashboard filter it out.
        let line = r#"{"type":"message_update","message":{},"assistantMessageEvent":{"type":"future_kind","foo":1}}"#;
        assert_eq!(pi_pl(line).row_type, "");
    }

    // -----------------------------------------------------------------
    // opencode SSE protocol events
    // -----------------------------------------------------------------

    fn oc_pl(json: &str) -> ProtocolLine {
        let payload: serde_json::Value = serde_json::from_str(json).unwrap();
        classify_opencode_event(&payload).unwrap()
    }

    #[test]
    fn opencode_text_streaming_no_end_is_hidden() {
        let line = r#"{"type":"message.part.updated","properties":{"sessionID":"s1","part":{"id":"p1","type":"text","text":"par","time":{"start":1}}}}"#;
        assert_eq!(oc_pl(line).row_type, "");
    }

    #[test]
    fn opencode_text_final_is_assistant() {
        let line = r#"{"type":"message.part.updated","properties":{"sessionID":"s1","part":{"id":"p1","type":"text","text":"all done","time":{"start":1,"end":2}}}}"#;
        let pl = oc_pl(line);
        assert_eq!(pl.row_type, "assistant");
        assert_eq!(pl.text, "all done");
    }

    #[test]
    fn opencode_reasoning_final_is_thinking() {
        let line = r#"{"type":"message.part.updated","properties":{"sessionID":"s1","part":{"id":"p1","type":"reasoning","text":"let me think","time":{"start":1,"end":2}}}}"#;
        let pl = oc_pl(line);
        assert_eq!(pl.row_type, "thinking");
        assert_eq!(pl.text, "let me think");
    }

    #[test]
    fn opencode_tool_running_is_hidden() {
        let line = r#"{"type":"message.part.updated","properties":{"sessionID":"s1","part":{"id":"p1","type":"tool","tool":"bash","state":{"status":"running"}}}}"#;
        assert_eq!(oc_pl(line).row_type, "");
    }

    #[test]
    fn opencode_tool_completed_is_tool_call_with_input_detail() {
        let line = r#"{"type":"message.part.updated","properties":{"sessionID":"s1","part":{"id":"p1","type":"tool","tool":"bash","state":{"status":"completed","input":{"command":"ls"},"output":"file1"}}}}"#;
        let pl = oc_pl(line);
        assert_eq!(pl.row_type, "tool_call");
        assert_eq!(pl.text, "bash");
        assert!(pl.detail.contains("ls"), "detail={}", pl.detail);
    }

    #[test]
    fn opencode_tool_error_is_error_row() {
        let line = r#"{"type":"message.part.updated","properties":{"sessionID":"s1","part":{"id":"p1","type":"tool","tool":"bash","state":{"status":"error","output":"boom"}}}}"#;
        let pl = oc_pl(line);
        assert_eq!(pl.row_type, "error");
        assert_eq!(pl.text, "boom");
    }

    #[test]
    fn opencode_step_start_is_hidden() {
        let line = r#"{"type":"message.part.updated","properties":{"sessionID":"s1","part":{"id":"p1","type":"step-start"}}}"#;
        assert_eq!(oc_pl(line).row_type, "");
    }

    #[test]
    fn opencode_unknown_inner_part_type_is_hidden() {
        let line = r#"{"type":"message.part.updated","properties":{"sessionID":"s1","part":{"id":"p1","type":"future_part"}}}"#;
        assert_eq!(oc_pl(line).row_type, "");
    }

    #[test]
    fn opencode_non_message_part_updated_event_is_hidden() {
        let line = r#"{"id":"e1","type":"session.idle","properties":{"sessionID":"s1"}}"#;
        assert_eq!(oc_pl(line).row_type, "");
    }
}
