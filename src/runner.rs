//! Agent runner abstraction.
//!
//! Spawns an agent process in its OWN process group (so SIGTERM/SIGKILL reach
//! the whole subtree), cwd = the per-issue workspace (containment asserted
//! first), pipes the rendered prompt / turn-request to stdin, and streams
//! stdout+stderr line-by-line into the log and the recent-events ring.
//!
//! Five runner backends implement the `RunnerSpec` trait:
//!   - `PiRunner`    — JSON-RPC over stdio with a per-issue session dir
//!   - `ClaudeRunner`— Claude Code CLI (`-p --permission-mode bypassPermissions --add-dir`)
//!   - `CodexRunner` — `codex app-server` + JSON-RPC turn request
//!   - `CliRunner`   — arbitrary command with `AIHUB_*` env
//!   - `FakeRunner`  — test shim (echo $AIHUB_PROMPT)
//!
//! `spawn` returns a `RunnerHandle` the orchestrator awaits and can kill.
//! On timeout or operator/reconcile kill the child's process group is sent
//! SIGTERM, granted a 5 s grace, then SIGKILL.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::oneshot;

use crate::logging;
use crate::paths::assert_contained;
use crate::state::EventRing;
use crate::store::{NewEvent, Store};

/// Grace period between SIGTERM and SIGKILL of the child process group.
const KILL_GRACE: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// How an attempt finished, from the orchestrator's point of view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExitKind {
    /// Process exited with code 0.
    Normal,
    /// Non-zero exit, killed by signal, or timed out. Carries the OS exit code
    /// when the process exited on its own (non-zero status), or `None` when
    /// killed by signal, timed out, or the wait call failed.
    Abnormal(Option<i32>),
    /// The runner was interrupted by an orchestrator-level condition.
    Interrupted { reason: &'static str },
}

/// Why the orchestrator asked to kill a running child.
pub enum KillReason {
    #[allow(dead_code)]
    Timeout,
    OperatorStop,
    Reconcile,
}

/// Parameters for spawning one agent run.
pub struct SpawnParams<'a> {
    pub command: &'a str,
    /// Runner kind (e.g. "pi", "claude"). Selects which backend is used.
    pub runner_kind: &'a str,
    /// Model override; passed to runners that support a model flag.
    pub model: Option<String>,
    pub workspace: &'a Path,
    pub workspace_root: &'a Path,
    pub agent_root: &'a Path,
    pub prompt: String,
    pub issue_id: String,
    /// SQLite run_id for this dispatch attempt. Used to tag event rows.
    pub run_id: String,
    pub max_run_timeout_ms: u64,
    /// Expose the optional Linear GraphQL worker tool to compatible protocol runners.
    pub expose_linear_graphql_tool: bool,
    pub events: Arc<EventRing>,
    pub store: Arc<Store>,
    pub last_event_at: Arc<Mutex<DateTime<Utc>>>,
}

// ---------------------------------------------------------------------------
// RunnerKind
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum RunnerKind {
    Pi,
    Claude,
    Codex,
    Cli,
    Fake,
}

impl RunnerKind {
    fn parse(raw: &str) -> Result<Self> {
        match raw {
            "" | "pi" => Ok(Self::Pi),
            "claude" | "claude-code" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "cli" => Ok(Self::Cli),
            "fake" => Ok(Self::Fake),
            other => {
                bail!("unsupported runner kind {other:?}; expected pi, claude, codex, cli, or fake")
            }
        }
    }

    fn default_command(&self) -> &'static str {
        match self {
            Self::Pi => "pi",
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Cli => "sh",
            Self::Fake => "sh",
        }
    }

    fn event_kind(&self) -> &'static str {
        match self {
            Self::Pi => "runner.pi",
            Self::Claude => "runner.claude",
            Self::Codex => "runner.codex",
            Self::Cli => "runner.cli",
            Self::Fake => "runner.fake",
        }
    }
}

// ---------------------------------------------------------------------------
// RunnerSpec trait
// ---------------------------------------------------------------------------

trait RunnerSpec {
    fn kind(&self) -> RunnerKind;
    fn command(&self) -> OsString;
    fn args(&self) -> Vec<OsString>;
    fn stdin_payload(&self) -> Option<Vec<u8>>;
    fn session_dir(&self) -> Option<PathBuf>;
    fn env(&self) -> Vec<(OsString, OsString)>;
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn effective_command(p: &SpawnParams<'_>, kind: &RunnerKind) -> OsString {
    if p.command.trim().is_empty() {
        OsString::from(kind.default_command())
    } else {
        OsString::from(p.command)
    }
}

/// `AIHUB_*` env vars that every runner receives.
fn common_env(p: &SpawnParams<'_>) -> Vec<(OsString, OsString)> {
    let mut env = vec![
        (
            OsString::from("AIHUB_ISSUE_IDENTIFIER"),
            OsString::from(&p.issue_id),
        ),
        (
            OsString::from("AIHUB_ISSUE_ID"),
            OsString::from(&p.issue_id),
        ),
        (OsString::from("AIHUB_RUN_ID"), OsString::from(&p.run_id)),
        (
            OsString::from("AIHUB_PROJECT_ID"),
            OsString::from(&p.issue_id),
        ),
        (
            OsString::from("AIHUB_WORKSPACE"),
            p.workspace.as_os_str().to_os_string(),
        ),
        (
            OsString::from("AIHUB_WORKSPACE_ROOT"),
            p.workspace_root.as_os_str().to_os_string(),
        ),
        (OsString::from("AIHUB_PROMPT"), OsString::from(&p.prompt)),
        (
            OsString::from("AIHUB_WORKER_PROMPT"),
            OsString::from(&p.prompt),
        ),
    ];
    if let Some(model) = &p.model {
        env.push((OsString::from("AIHUB_MODEL"), OsString::from(model)));
        env.push((OsString::from("AIHUB_WORKER_MODEL"), OsString::from(model)));
    }
    if p.expose_linear_graphql_tool {
        env.push((
            OsString::from("AIHUB_LINEAR_GRAPHQL_TOOL"),
            OsString::from("1"),
        ));
    }
    env
}

fn env_with_session_dir(
    mut env: Vec<(OsString, OsString)>,
    session_dir: &Path,
) -> Vec<(OsString, OsString)> {
    env.push((
        OsString::from("AIHUB_SESSION_DIR"),
        session_dir.as_os_str().to_os_string(),
    ));
    env
}

// ---------------------------------------------------------------------------
// Per-runner implementations
// ---------------------------------------------------------------------------

struct PiRunner<'a> {
    p: &'a SpawnParams<'a>,
}

impl RunnerSpec for PiRunner<'_> {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Pi
    }
    fn command(&self) -> OsString {
        effective_command(self.p, &RunnerKind::Pi)
    }
    fn args(&self) -> Vec<OsString> {
        vec![]
    }
    fn stdin_payload(&self) -> Option<Vec<u8>> {
        Some(pi_turn_request(self.p).into_bytes())
    }
    fn session_dir(&self) -> Option<PathBuf> {
        Some(self.p.agent_root.join("pi-sessions").join(&self.p.issue_id))
    }
    fn env(&self) -> Vec<(OsString, OsString)> {
        env_with_session_dir(common_env(self.p), &self.session_dir().unwrap())
    }
}

struct ClaudeRunner<'a> {
    p: &'a SpawnParams<'a>,
}

impl RunnerSpec for ClaudeRunner<'_> {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Claude
    }
    fn command(&self) -> OsString {
        effective_command(self.p, &RunnerKind::Claude)
    }
    fn args(&self) -> Vec<OsString> {
        claude_args(self.p)
    }
    fn stdin_payload(&self) -> Option<Vec<u8>> {
        Some(self.p.prompt.clone().into_bytes())
    }
    fn session_dir(&self) -> Option<PathBuf> {
        Some(
            self.p
                .agent_root
                .join("claude-sessions")
                .join(&self.p.issue_id),
        )
    }
    fn env(&self) -> Vec<(OsString, OsString)> {
        env_with_session_dir(common_env(self.p), &self.session_dir().unwrap())
    }
}

struct CodexRunner<'a> {
    p: &'a SpawnParams<'a>,
}

impl RunnerSpec for CodexRunner<'_> {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Codex
    }
    fn command(&self) -> OsString {
        effective_command(self.p, &RunnerKind::Codex)
    }
    fn args(&self) -> Vec<OsString> {
        vec![OsString::from("app-server")]
    }
    fn stdin_payload(&self) -> Option<Vec<u8>> {
        Some(codex_turn_request(self.p).into_bytes())
    }
    fn session_dir(&self) -> Option<PathBuf> {
        None
    }
    fn env(&self) -> Vec<(OsString, OsString)> {
        common_env(self.p)
    }
}

struct CliRunner<'a> {
    p: &'a SpawnParams<'a>,
}

impl RunnerSpec for CliRunner<'_> {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Cli
    }
    fn command(&self) -> OsString {
        effective_command(self.p, &RunnerKind::Cli)
    }
    fn args(&self) -> Vec<OsString> {
        vec![]
    }
    fn stdin_payload(&self) -> Option<Vec<u8>> {
        None
    }
    fn session_dir(&self) -> Option<PathBuf> {
        None
    }
    fn env(&self) -> Vec<(OsString, OsString)> {
        common_env(self.p)
    }
}

struct FakeRunner<'a> {
    p: &'a SpawnParams<'a>,
}

impl RunnerSpec for FakeRunner<'_> {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Fake
    }
    fn command(&self) -> OsString {
        effective_command(self.p, &RunnerKind::Fake)
    }
    fn args(&self) -> Vec<OsString> {
        vec![
            OsString::from("-c"),
            OsString::from("printf '%s\\n' \"$AIHUB_PROMPT\""),
        ]
    }
    fn stdin_payload(&self) -> Option<Vec<u8>> {
        None
    }
    fn session_dir(&self) -> Option<PathBuf> {
        None
    }
    fn env(&self) -> Vec<(OsString, OsString)> {
        common_env(self.p)
    }
}

/// Collected outputs from a `RunnerSpec` — all owned so the spec (and its
/// borrow of `SpawnParams`) can be dropped immediately after construction.
struct RunnerParams {
    command: OsString,
    args: Vec<OsString>,
    stdin_payload: Option<Vec<u8>>,
    event_kind: &'static str,
    session_dir: Option<PathBuf>,
    env: Vec<(OsString, OsString)>,
}

fn collect_runner_params<R: RunnerSpec>(r: R) -> RunnerParams {
    RunnerParams {
        command: r.command(),
        args: r.args(),
        stdin_payload: r.stdin_payload(),
        event_kind: r.kind().event_kind(),
        session_dir: r.session_dir(),
        env: r.env(),
    }
}

// ---------------------------------------------------------------------------
// Protocol helpers
// ---------------------------------------------------------------------------

fn pi_turn_request(p: &SpawnParams<'_>) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": p.run_id,
        "method": "turn",
        "params": {
            "prompt": p.prompt,
            "session_dir": p.agent_root.join("pi-sessions").join(&p.issue_id),
            "issue_identifier": p.issue_id,
            "run_id": p.run_id,
            "model": p.model,
            "tools": worker_tools(p),
        }
    })
    .to_string()
        + "\n"
}

fn codex_turn_request(p: &SpawnParams<'_>) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": p.run_id,
        "method": "turn",
        "params": {
            "prompt": p.prompt,
            "issue_identifier": p.issue_id,
            "run_id": p.run_id,
            "model": p.model,
            "tools": worker_tools(p),
        }
    })
    .to_string()
        + "\n"
}

fn worker_tools(p: &SpawnParams<'_>) -> Vec<serde_json::Value> {
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

fn claude_args(p: &SpawnParams<'_>) -> Vec<OsString> {
    let mut args = Vec::new();
    // Autonomous runner: no human is present to answer Claude's permission
    // prompts, and the workflow needs the child to edit its issue file, which
    // lives outside the workspace cwd (under the agent folder). Bypass the
    // permission sandbox and widen the allowed dirs to the agent folder.
    args.extend([
        OsString::from("-p"),
        OsString::from("--permission-mode"),
        OsString::from("bypassPermissions"),
        OsString::from("--add-dir"),
        p.agent_root.as_os_str().to_os_string(),
    ]);
    if let Some(ref model) = p.model {
        args.push(OsString::from("--model"));
        args.push(OsString::from(model));
    }
    args
}

// ---------------------------------------------------------------------------
// Event normalization
// ---------------------------------------------------------------------------

/// Classify a protocol output line into a UI log row type and display text.
/// Tries JSON parsing first (for pi/claude/codex protocol events); falls back
/// to text heuristics for plain text output.
fn classify_protocol_line(stream: &str, text: &str) -> (&'static str, String) {
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
fn map_event_type(v: &serde_json::Value) -> &'static str {
    match v.get("type").and_then(|t| t.as_str()) {
        Some("assistant") => "assistant",
        Some("thinking") | Some("thought") => "thinking",
        Some("user") => "user",
        Some("tool_use") | Some("tool_call") => "tool_call",
        Some("tool_result") | Some("tool_output") => "tool_output",
        Some("error") => "error",
        _ => {
            // JSON-RPC response envelope: look inside "result"
            if let Some(result) = v.get("result") {
                return map_event_type(result);
            }
            "assistant"
        }
    }
}

/// Extract a human-readable text snippet from a protocol event.
fn extract_display_text(v: &serde_json::Value) -> Option<String> {
    // Direct "text" field
    if let Some(t) = v.get("text").and_then(|t| t.as_str()) {
        return Some(t.to_string());
    }
    // Direct "content" as a string
    if let Some(c) = v.get("content").and_then(|c| c.as_str()) {
        return Some(c.to_string());
    }
    // JSON-RPC envelope: recurse into "result"
    if let Some(t) = v.get("result").and_then(extract_display_text) {
        return Some(t);
    }
    // Claude streaming NDJSON: message.content[0].text
    v.get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first())
        .and_then(|item| item.get("text"))
        .and_then(|t| t.as_str())
        .map(|s| s.to_string())
}

/// Text-based heuristic fallback for non-JSON lines.
fn normalize_log_row(stream: &str, text: &str) -> &'static str {
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

// ---------------------------------------------------------------------------
// RunnerHandle
// ---------------------------------------------------------------------------

/// Handle to a running child. Owns the kill channel and the supervising task's
/// join handle. `wait` / `request_kill` consume the handle; the orchestrator
/// stores it via `Option::take`.
pub struct RunnerHandle {
    pub pid: u32,
    kill_tx: oneshot::Sender<KillReason>,
    done: tokio::task::JoinHandle<ExitKind>,
}

impl RunnerHandle {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Non-consuming completion check so the orchestrator can poll a stored
    /// handle each tick, then `take()` + `wait()` to collect the `ExitKind`.
    pub fn is_finished(&self) -> bool {
        self.done.is_finished()
    }

    /// Await the run to completion and return its classified exit.
    pub async fn wait(self) -> ExitKind {
        // The supervising task always resolves to an ExitKind; a JoinError
        // (panic/cancel) is treated as abnormal.
        self.done.await.unwrap_or(ExitKind::Abnormal(None))
    }

    /// Ask the supervising task to terminate the child for the given reason.
    pub fn request_kill(self, why: KillReason) {
        let _ = self.kill_tx.send(why);
        drop(self.done);
    }

    pub async fn request_kill_and_wait(self, why: KillReason) -> ExitKind {
        let Self { kill_tx, done, .. } = self;
        let _ = kill_tx.send(why);
        done.await.unwrap_or(ExitKind::Abnormal(None))
    }

    #[cfg(test)]
    pub(crate) fn finished_for_test(pid: u32, kind: ExitKind) -> Self {
        let (kill_tx, _kill_rx) = oneshot::channel::<KillReason>();
        let done = tokio::spawn(async move { kind });
        Self { pid, kill_tx, done }
    }

    #[cfg(test)]
    pub(crate) fn pending_for_test(pid: u32, kind: ExitKind) -> Self {
        let (kill_tx, kill_rx) = oneshot::channel::<KillReason>();
        let done = tokio::spawn(async move {
            let _ = kill_rx.await;
            kind
        });
        Self { pid, kill_tx, done }
    }
}

// ---------------------------------------------------------------------------
// spawn
// ---------------------------------------------------------------------------

/// Spawn an agent child for one issue. Asserts workspace containment, sets up
/// its own process group, pipes the turn request/prompt to stdin, and
/// supervises the child in a background task that streams output and enforces
/// the per-turn timeout.
pub async fn spawn(p: SpawnParams<'_>) -> Result<RunnerHandle> {
    assert_contained(p.workspace_root, p.workspace)
        .context("workspace containment check failed; refusing to spawn child")?;

    // Build runner and collect all owned data before borrowing p further.
    let rp = match RunnerKind::parse(p.runner_kind)? {
        RunnerKind::Pi => collect_runner_params(PiRunner { p: &p }),
        RunnerKind::Claude => collect_runner_params(ClaudeRunner { p: &p }),
        RunnerKind::Codex => collect_runner_params(CodexRunner { p: &p }),
        RunnerKind::Cli => collect_runner_params(CliRunner { p: &p }),
        RunnerKind::Fake => collect_runner_params(FakeRunner { p: &p }),
    };

    if let Some(dir) = &rp.session_dir {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating runner session dir {}", dir.display()))?;
    }

    let mut cmd = Command::new(&rp.command);
    for arg in &rp.args {
        cmd.arg(arg);
    }
    cmd.envs(rp.env);
    cmd.current_dir(p.workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Own process group so signalling the negative pid hits the whole tree.
        .process_group(0);

    let mut child = cmd.spawn().with_context(|| {
        format!(
            "spawning `{}` in {}",
            rp.command.to_string_lossy(),
            p.workspace.display()
        )
    })?;

    let pid = child
        .id()
        .context("child has no pid immediately after spawn")?;
    logging::ev(
        &p.issue_id,
        "spawn",
        &format!(
            "runner={} pid={pid} cwd={}",
            p.runner_kind,
            p.workspace.display()
        ),
    );
    persist_runner_event(
        &p.store,
        Some(&p.run_id),
        &p.issue_id,
        rp.event_kind,
        serde_json::json!({
            "type": "spawn",
            "runner": p.runner_kind,
            "pid": pid,
            "command": rp.command.to_string_lossy(),
            "args": rp.args.iter().map(|a| a.to_string_lossy().to_string()).collect::<Vec<_>>(),
            "session_dir": rp.session_dir.as_ref().map(|d| d.display().to_string()),
        }),
    );

    // Write the turn request / prompt to stdin, then close it (EOF).
    if let (Some(mut stdin), Some(payload)) = (child.stdin.take(), rp.stdin_payload) {
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
            rp.event_kind,
            Arc::clone(&p.events),
            Arc::clone(&p.store),
            Arc::clone(&p.last_event_at),
        );
    }
    if let Some(err) = stderr {
        spawn_line_pump(
            err,
            p.issue_id.clone(),
            p.run_id.clone(),
            "stderr",
            rp.event_kind,
            Arc::clone(&p.events),
            Arc::clone(&p.store),
            Arc::clone(&p.last_event_at),
        );
    }

    let (kill_tx, kill_rx) = oneshot::channel::<KillReason>();
    let timeout = Duration::from_millis(p.max_run_timeout_ms);
    let issue_id = p.issue_id.clone();

    let done = tokio::spawn(async move { supervise(child, pid, issue_id, timeout, kill_rx).await });

    Ok(RunnerHandle { pid, kill_tx, done })
}

// ---------------------------------------------------------------------------
// Internal
// ---------------------------------------------------------------------------

/// Drive the child: wait for exit, the per-turn timeout, or a kill request.
async fn supervise(
    mut child: tokio::process::Child,
    pid: u32,
    issue_id: String,
    timeout: Duration,
    kill_rx: oneshot::Receiver<KillReason>,
) -> ExitKind {
    let kind = tokio::select! {
        status = child.wait() => {
            match status {
                Ok(s) if s.success() => {
                    logging::ev(&issue_id, "exit", "code=0 (normal)");
                    return ExitKind::Normal;
                }
                Ok(s) => {
                    let code = s.code();
                    logging::ev(&issue_id, "exit", &format!("status={s} (abnormal)"));
                    return ExitKind::Abnormal(code);
                }
                Err(e) => {
                    logging::ev(&issue_id, "exit", &format!("wait error: {e} (abnormal)"));
                    return ExitKind::Abnormal(None);
                }
            }
        }
        _ = tokio::time::sleep(timeout) => {
            logging::ev(&issue_id, "timeout", "turn_timeout_ms exceeded; killing");
            ExitKind::Interrupted { reason: "turn_timeout" }
        }
        reason = kill_rx => {
            match reason {
                Ok(KillReason::Timeout) => logging::ev(&issue_id, "kill", "reason=timeout"),
                Ok(KillReason::OperatorStop) => logging::ev(&issue_id, "kill", "reason=operator_stop"),
                Ok(KillReason::Reconcile) => logging::ev(&issue_id, "kill", "reason=reconcile"),
                Err(_) => logging::ev(&issue_id, "kill", "handle dropped"),
            }
            ExitKind::Abnormal(None)
        }
    };

    term_then_kill(pid, KILL_GRACE);
    let _ = child.wait().await;
    kind
}

/// Stream one byte source line-by-line into the event ring + SQLite + log.
/// Each line is ANSI-stripped, classified via JSON parsing (or text heuristic
/// fallback), then stored with a normalized `log_row` type.
#[allow(clippy::too_many_arguments)]
fn spawn_line_pump<R>(
    reader: R,
    issue_id: String,
    run_id: String,
    stream: &'static str,
    runner_event_kind: &'static str,
    events: Arc<EventRing>,
    store: Arc<Store>,
    last_event_at: Arc<Mutex<DateTime<Utc>>>,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
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
                    logging::ev(&issue_id, stream, &clean);
                    let payload = serde_json::json!({
                        "type": "protocol_event",
                        "stream": stream,
                        "log_row": row_type,
                        "text": display,
                    })
                    .to_string();
                    let _ = store.insert_event(&NewEvent {
                        run_id: Some(&run_id),
                        issue_identifier: &issue_id,
                        kind: runner_event_kind,
                        payload: &payload,
                        ts,
                    });
                }
                Ok(None) => break, // EOF
                Err(e) => {
                    logging::ev(&issue_id, stream, &format!("read error: {e}"));
                    break;
                }
            }
        }
    });
}

fn persist_runner_event(
    store: &Store,
    run_id: Option<&str>,
    issue_id: &str,
    kind: &'static str,
    value: serde_json::Value,
) {
    let payload = value.to_string();
    let _ = store.insert_event(&NewEvent {
        run_id,
        issue_identifier: issue_id,
        kind,
        payload: &payload,
        ts: Utc::now(),
    });
}

fn strip_ansi(input: &str) -> String {
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

/// Send SIGTERM to the child's process group, wait `grace`, then SIGKILL.
pub fn term_then_kill(pid: u32, grace: std::time::Duration) {
    let pgid = Pid::from_raw(-(pid as i32));
    let _ = kill(pgid, Signal::SIGTERM);
    std::thread::spawn(move || {
        std::thread::sleep(grace);
        let _ = kill(pgid, Signal::SIGKILL);
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::EventRing;
    use crate::store::Store;
    use chrono::Utc;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    fn params<'a>(
        runner_kind: &'a str,
        model: Option<String>,
        workspace: &'a Path,
        workspace_root: &'a Path,
    ) -> SpawnParams<'a> {
        SpawnParams {
            command: "runner",
            runner_kind,
            model,
            workspace,
            workspace_root,
            agent_root: workspace_root.parent().unwrap_or(workspace_root),
            prompt: String::new(),
            issue_id: "ISSUE-1".to_string(),
            run_id: "ISSUE-1-test".to_string(),
            max_run_timeout_ms: 1000,
            expose_linear_graphql_tool: false,
            events: Arc::new(EventRing::new()),
            store: Arc::new(Store::open(&PathBuf::from(":memory:")).unwrap()),
            last_event_at: Arc::new(Mutex::new(Utc::now())),
        }
    }

    fn arg_strings(args: Vec<OsString>) -> Vec<String> {
        args.into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    /// Build command args via the appropriate per-runner struct.
    fn build_command_args(p: &SpawnParams<'_>) -> Vec<OsString> {
        match RunnerKind::parse(p.runner_kind).unwrap_or(RunnerKind::Pi) {
            RunnerKind::Pi => PiRunner { p }.args(),
            RunnerKind::Claude => ClaudeRunner { p }.args(),
            RunnerKind::Codex => CodexRunner { p }.args(),
            RunnerKind::Cli => CliRunner { p }.args(),
            RunnerKind::Fake => FakeRunner { p }.args(),
        }
    }

    #[test]
    fn claude_code_model_is_passed_to_spawn_args() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let p = params(
            "claude-code",
            Some("claude-opus-4-6".to_string()),
            workspace,
            workspace_root,
        );

        let args = arg_strings(build_command_args(&p));

        assert_eq!(
            args,
            vec![
                "-p",
                "--permission-mode",
                "bypassPermissions",
                "--add-dir",
                "/tmp/agent",
                "--model",
                "claude-opus-4-6"
            ]
        );
    }

    #[test]
    fn non_claude_runner_does_not_get_claude_spawn_args() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        // Unknown kind falls back to Pi which has no special args.
        let p = params("gemini-code", None, workspace, workspace_root);

        let args = arg_strings(build_command_args(&p));

        assert!(!args.iter().any(|arg| {
            matches!(
                arg.as_str(),
                "-p" | "--permission-mode" | "bypassPermissions" | "--add-dir" | "--model"
            )
        }));
        assert!(args.is_empty());
    }

    #[test]
    fn runner_specs_create_session_dirs_for_pi_and_claude() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let pi = params("pi", None, workspace, workspace_root);
        let claude = params("claude", None, workspace, workspace_root);

        let pi_spec = PiRunner { p: &pi };
        let claude_spec = ClaudeRunner { p: &claude };

        assert_eq!(
            pi_spec.session_dir().unwrap(),
            PathBuf::from("/tmp/agent/pi-sessions/ISSUE-1")
        );
        assert_eq!(
            claude_spec.session_dir().unwrap(),
            PathBuf::from("/tmp/agent/claude-sessions/ISSUE-1")
        );
    }

    #[test]
    fn linear_graphql_tool_is_gated_in_turn_request() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let mut p = params("pi", None, workspace, workspace_root);

        let without_tool: serde_json::Value = serde_json::from_str(&pi_turn_request(&p)).unwrap();
        assert_eq!(without_tool["params"]["tools"].as_array().unwrap().len(), 0);

        p.expose_linear_graphql_tool = true;
        let with_tool: serde_json::Value = serde_json::from_str(&pi_turn_request(&p)).unwrap();
        assert_eq!(with_tool["params"]["tools"][0]["name"], "linear_graphql");
    }

    #[test]
    fn codex_sends_turn_request_and_cli_gets_aihub_env() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let codex = params(
            "codex",
            Some("codex-1".to_string()),
            workspace,
            workspace_root,
        );
        let cli = params(
            "cli",
            Some("model-a".to_string()),
            workspace,
            workspace_root,
        );

        let codex_spec = CodexRunner { p: &codex };
        let cli_spec = CliRunner { p: &cli };

        assert_eq!(arg_strings(codex_spec.args()), vec!["app-server"]);
        // Codex must send a JSON-RPC turn request (not None).
        let payload = codex_spec.stdin_payload().unwrap();
        let json: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["method"], "turn");
        assert_eq!(json["params"]["issue_identifier"], "ISSUE-1");
        assert_eq!(json["params"]["model"], "codex-1");

        let env: Vec<(String, String)> = cli_spec
            .env()
            .into_iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect();
        assert!(env.contains(&("AIHUB_ISSUE_IDENTIFIER".into(), "ISSUE-1".into())));
        assert!(env.contains(&("AIHUB_MODEL".into(), "model-a".into())));
        assert!(env.contains(&("AIHUB_WORKER_MODEL".into(), "model-a".into())));
        assert!(env.iter().any(|(k, _)| k == "AIHUB_WORKER_PROMPT"));
    }

    #[test]
    fn pi_spec_writes_json_rpc_turn_request() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let pi = params(
            "pi",
            Some("pi-model".to_string()),
            workspace,
            workspace_root,
        );
        let pi_spec = PiRunner { p: &pi };

        let payload = String::from_utf8(pi_spec.stdin_payload().unwrap()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["method"], "turn");
        assert_eq!(value["params"]["issue_identifier"], "ISSUE-1");
        assert_eq!(value["params"]["model"], "pi-model");
    }

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
