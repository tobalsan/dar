use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use cap_runner::{
    ExitKind, KillReason, RunnerEventSink, RunnerEventStore, RunnerHandle, SpawnParams,
};
use chrono::Utc;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use tokio::io::AsyncWriteExt;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::oneshot;

static FILE_LOADED_KEYS: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();

pub trait RunnerSpec {
    fn command(&self, params: &SpawnParams<'_>) -> OsString;
    fn args(&self, params: &SpawnParams<'_>) -> Vec<OsString>;
    fn stdin_payload(&self, params: &SpawnParams<'_>) -> Option<Vec<u8>>;
    fn session_dir(&self, params: &SpawnParams<'_>) -> Option<PathBuf>;
    fn env(&self, params: &SpawnParams<'_>) -> Vec<(OsString, OsString)>;
    fn event_kind(&self) -> &'static str;
}

pub fn effective_command(params: &SpawnParams<'_>, default_command: &str) -> OsString {
    if params.command.trim().is_empty() {
        OsString::from(default_command)
    } else {
        OsString::from(params.command)
    }
}

pub fn common_env(params: &SpawnParams<'_>) -> Vec<(OsString, OsString)> {
    let mut env = vec![
        (
            OsString::from(cap_runner::AGENT_ISSUE_IDENTIFIER),
            OsString::from(&params.issue_id),
        ),
        (
            OsString::from(cap_runner::AGENT_ISSUE_ID),
            OsString::from(&params.issue_id),
        ),
        (
            OsString::from(cap_runner::AGENT_RUN_ID),
            OsString::from(&params.run_id),
        ),
        (
            OsString::from(cap_runner::AGENT_PROJECT_ID),
            OsString::from(&params.issue_id),
        ),
        (
            OsString::from(cap_runner::AGENT_WORKSPACE),
            params.workspace.as_os_str().to_os_string(),
        ),
        (
            OsString::from(cap_runner::AGENT_WORKSPACE_ROOT),
            params.workspace_root.as_os_str().to_os_string(),
        ),
        (
            OsString::from(cap_runner::AGENT_PROMPT),
            OsString::from(&params.prompt),
        ),
        (
            OsString::from(cap_runner::AGENT_WORKER_PROMPT),
            OsString::from(&params.prompt),
        ),
    ];
    if let Some(model) = &params.model {
        env.push((
            OsString::from(cap_runner::AGENT_MODEL),
            OsString::from(model),
        ));
        env.push((
            OsString::from(cap_runner::AGENT_WORKER_MODEL),
            OsString::from(model),
        ));
    }
    if params.expose_linear_graphql_tool {
        env.push((
            OsString::from(cap_runner::AGENT_LINEAR_GRAPHQL_TOOL),
            OsString::from("1"),
        ));
    }
    env
}

pub fn env_with_session_dir(
    mut env: Vec<(OsString, OsString)>,
    session_dir: &Path,
) -> Vec<(OsString, OsString)> {
    env.push((
        OsString::from(cap_runner::AGENT_SESSION_DIR),
        session_dir.as_os_str().to_os_string(),
    ));
    env
}

pub fn worker_tools(params: &SpawnParams<'_>) -> Vec<serde_json::Value> {
    if params.expose_linear_graphql_tool {
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

pub fn record_loaded_env_key(key: impl Into<String>) {
    FILE_LOADED_KEYS
        .get_or_init(|| Mutex::new(std::collections::HashSet::new()))
        .lock()
        .unwrap()
        .insert(key.into());
}

pub fn scrub_loaded_env(cmd: &mut Command) {
    let Some(keys) = FILE_LOADED_KEYS.get() else {
        return;
    };
    for key in keys.lock().unwrap().iter() {
        cmd.env_remove(key);
    }
}

pub fn assert_contained(root: &Path, child: &Path) -> Result<()> {
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", root.display()))?;
    let child = child
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", child.display()))?;
    if !child.starts_with(&root) {
        bail!("{} is outside {}", child.display(), root.display());
    }
    Ok(())
}

pub async fn spawn_spec<S>(spec: S, params: SpawnParams<'_>) -> Result<RunnerHandle>
where
    S: RunnerSpec,
{
    assert_contained(params.workspace_root, params.workspace)
        .context("workspace containment check failed; refusing to spawn child")?;

    let command = spec.command(&params);
    let args = spec.args(&params);
    let stdin_payload = spec.stdin_payload(&params);
    let session_dir = spec.session_dir(&params);
    let env = spec.env(&params);
    let event_kind = spec.event_kind();

    if let Some(dir) = &session_dir {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating runner session dir {}", dir.display()))?;
    }

    let mut cmd = Command::new(&command);
    for arg in &args {
        cmd.arg(arg);
    }
    scrub_loaded_env(&mut cmd);
    cmd.envs(env);
    cmd.current_dir(params.workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    setup_process_group(&mut cmd);

    let mut child = cmd.spawn().with_context(|| {
        format!(
            "spawning `{}` in {}",
            command.to_string_lossy(),
            params.workspace.display()
        )
    })?;

    let pid = child
        .id()
        .context("child has no pid immediately after spawn")?;
    tracing::info!(
        issue = %params.issue_id,
        runner = %params.runner_kind,
        pid,
        cwd = %params.workspace.display(),
        "runner spawned"
    );
    persist_runner_event(
        params.store.as_ref(),
        Some(&params.run_id),
        &params.issue_id,
        event_kind,
        serde_json::json!({
            "type": "spawn",
            "runner": params.runner_kind,
            "pid": pid,
            "command": command.to_string_lossy(),
            "args": args.iter().map(|a| a.to_string_lossy().to_string()).collect::<Vec<_>>(),
            "session_dir": session_dir.as_ref().map(|d| d.display().to_string()),
        }),
    );

    if let (Some(mut stdin), Some(payload)) = (child.stdin.take(), stdin_payload) {
        tokio::spawn(async move {
            let _ = stdin.write_all(&payload).await;
            let _ = stdin.flush().await;
        });
    }

    if let Some(out) = child.stdout.take() {
        spawn_line_pump(
            out,
            params.issue_id.clone(),
            params.run_id.clone(),
            "stdout",
            event_kind,
            std::sync::Arc::clone(&params.events),
            std::sync::Arc::clone(&params.store),
            std::sync::Arc::clone(&params.last_event_at),
            |issue, stream, line| tracing::info!(%issue, %stream, %line, "runner output"),
        );
    }
    if let Some(err) = child.stderr.take() {
        spawn_line_pump(
            err,
            params.issue_id.clone(),
            params.run_id.clone(),
            "stderr",
            event_kind,
            std::sync::Arc::clone(&params.events),
            std::sync::Arc::clone(&params.store),
            std::sync::Arc::clone(&params.last_event_at),
            |issue, stream, line| tracing::info!(%issue, %stream, %line, "runner output"),
        );
    }

    let (kill_tx, kill_rx) = oneshot::channel::<KillReason>();
    let timeout = Duration::from_millis(params.max_run_timeout_ms);
    let issue_id = params.issue_id.clone();

    let done = tokio::spawn(async move {
        supervise(child, pid, timeout, kill_rx, move |kind, message| {
            tracing::info!(issue = %issue_id, %kind, %message, "runner supervise");
        })
        .await
    });

    Ok(RunnerHandle::new(pid, kill_tx, done))
}

/// Configure a child command to run in its own process group, so signalling the
/// negative pid reaches the whole subtree.
pub fn setup_process_group(cmd: &mut Command) -> &mut Command {
    cmd.process_group(0)
}

/// Classify a protocol output line into a UI log row type and display text.
/// Tries JSON parsing first (for pi/claude/codex protocol events); falls back
/// to text heuristics for plain text output.
pub fn classify_protocol_line(stream: &str, text: &str) -> (&'static str, String) {
    if stream == "stderr" {
        return ("error", text.to_string());
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
        let row_type = map_event_type(&value);
        let display = extract_display_text(&value).unwrap_or_else(|| text.to_string());
        (row_type, display)
    } else {
        (normalize_log_row(stream, text), text.to_string())
    }
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
                    let (row_type, display) = classify_protocol_line(stream, &clean);
                    let formatted = format!("child[{issue_id}]: {clean}");
                    events.push(formatted);
                    if let Ok(mut t) = last_event_at.lock() {
                        *t = ts;
                    }
                    log(&issue_id, stream, &clean);
                    let payload = serde_json::json!({
                        "type": "protocol_event",
                        "stream": stream,
                        "log_row": row_type,
                        "text": display,
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
        let (row, display) =
            classify_protocol_line("stdout", r#"{"type":"assistant","text":"Hello"}"#);
        assert_eq!(row, "assistant");
        assert_eq!(display, "Hello");

        let (row, _) = classify_protocol_line("stdout", r#"{"type":"thinking","text":"hmm"}"#);
        assert_eq!(row, "thinking");

        let (row, _) = classify_protocol_line("stdout", r#"{"type":"tool_use","name":"bash"}"#);
        assert_eq!(row, "tool_call");

        let (row, display) =
            classify_protocol_line("stdout", r#"{"type":"tool_result","content":"ok"}"#);
        assert_eq!(row, "tool_output");
        assert_eq!(display, "ok");

        let (row, _) = classify_protocol_line("stdout", r#"{"type":"error","message":"oops"}"#);
        assert_eq!(row, "error");
    }

    #[test]
    fn stderr_lines_always_classified_as_error() {
        let (row, _) = classify_protocol_line("stderr", r#"{"type":"assistant","text":"x"}"#);
        assert_eq!(row, "error");
        let (row, _) = classify_protocol_line("stderr", "plain text");
        assert_eq!(row, "error");
    }

    #[test]
    fn jsonrpc_result_unwrapped_for_type_mapping() {
        let rpc = r#"{"jsonrpc":"2.0","id":"r1","result":{"type":"assistant","text":"Done"}}"#;
        let (row, display) = classify_protocol_line("stdout", rpc);
        assert_eq!(row, "assistant");
        assert_eq!(display, "Done");
    }

    #[test]
    fn non_json_falls_back_to_heuristic() {
        let (row, _) = classify_protocol_line("stdout", "thinking about the problem");
        assert_eq!(row, "thinking");
        let (row, _) = classify_protocol_line("stdout", "tool_call: bash");
        assert_eq!(row, "tool_call");
    }
}
