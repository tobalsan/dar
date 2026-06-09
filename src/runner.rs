//! Agent runner abstraction.
//!
//! Spawns `claude -p` in its OWN process group (so SIGTERM/SIGKILL reach the
//! whole subtree), cwd = the per-issue workspace (containment asserted first),
//! pipes the rendered prompt to the child's stdin then closes it, and streams
//! the child's stdout+stderr line-by-line into the log and the recent-events
//! ring (prefixed `child[ID]:`). Tracks pid and last_event_at.
//!
//! `spawn` returns a `RunnerHandle` the orchestrator awaits and can signal-kill.
//! On timeout or operator/reconcile kill the child's process group is sent
//! SIGTERM, granted a 5s grace, then SIGKILL.

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
    /// Runner kind (e.g. "claude-code"). Determines which CLI flags are added.
    pub runner_kind: &'a str,
    /// Model override; passed to runners that support model flags.
    pub model: Option<String>,
    pub workspace: &'a Path,
    pub workspace_root: &'a Path,
    pub agent_root: &'a Path,
    pub prompt: String,
    pub issue_id: String,
    /// SQLite run_id for this dispatch attempt. Used to tag event rows.
    pub run_id: String,
    pub max_run_timeout_ms: u64,
    pub events: Arc<EventRing>,
    pub store: Arc<Store>,
    pub last_event_at: Arc<Mutex<DateTime<Utc>>>,
}

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

trait RunnerSpec {
    fn kind(&self) -> RunnerKind;
    fn command(&self) -> OsString;
    fn args(&self) -> Vec<OsString>;
    fn stdin_payload(&self) -> Option<Vec<u8>>;
    fn session_dir(&self) -> Option<PathBuf>;
    fn env(&self) -> Vec<(OsString, OsString)>;
}

struct ProcessRunnerSpec<'a> {
    p: &'a SpawnParams<'a>,
    kind: RunnerKind,
}

impl<'a> ProcessRunnerSpec<'a> {
    fn configured_command(&self) -> OsString {
        if self.p.command.trim().is_empty() {
            OsString::from(self.kind.default_command())
        } else {
            OsString::from(self.p.command)
        }
    }
}

impl RunnerSpec for ProcessRunnerSpec<'_> {
    fn kind(&self) -> RunnerKind {
        self.kind.clone()
    }

    fn command(&self) -> OsString {
        self.configured_command()
    }

    fn args(&self) -> Vec<OsString> {
        match self.kind {
            RunnerKind::Pi => vec![],
            RunnerKind::Claude => claude_args(self.p),
            RunnerKind::Codex => vec![OsString::from("app-server")],
            RunnerKind::Cli => vec![],
            RunnerKind::Fake => vec![
                OsString::from("-c"),
                OsString::from("printf '%s\\n' \"$AIHUB_PROMPT\""),
            ],
        }
    }

    fn stdin_payload(&self) -> Option<Vec<u8>> {
        match self.kind {
            RunnerKind::Pi => Some(pi_turn_request(self.p).into_bytes()),
            RunnerKind::Codex | RunnerKind::Cli | RunnerKind::Fake => None,
            _ => Some(self.p.prompt.clone().into_bytes()),
        }
    }

    fn session_dir(&self) -> Option<PathBuf> {
        match self.kind {
            RunnerKind::Pi => Some(self.p.agent_root.join("pi-sessions").join(&self.p.issue_id)),
            RunnerKind::Claude => Some(
                self.p
                    .agent_root
                    .join("claude-sessions")
                    .join(&self.p.issue_id),
            ),
            RunnerKind::Codex | RunnerKind::Cli | RunnerKind::Fake => None,
        }
    }

    fn env(&self) -> Vec<(OsString, OsString)> {
        let mut env = vec![
            (
                OsString::from("AIHUB_ISSUE_IDENTIFIER"),
                OsString::from(&self.p.issue_id),
            ),
            (
                OsString::from("AIHUB_ISSUE_ID"),
                OsString::from(&self.p.issue_id),
            ),
            (
                OsString::from("AIHUB_RUN_ID"),
                OsString::from(&self.p.run_id),
            ),
            (
                OsString::from("AIHUB_PROJECT_ID"),
                OsString::from(&self.p.issue_id),
            ),
            (
                OsString::from("AIHUB_WORKSPACE"),
                self.p.workspace.as_os_str().to_os_string(),
            ),
            (
                OsString::from("AIHUB_WORKSPACE_ROOT"),
                self.p.workspace_root.as_os_str().to_os_string(),
            ),
            (
                OsString::from("AIHUB_PROMPT"),
                OsString::from(&self.p.prompt),
            ),
            (
                OsString::from("AIHUB_WORKER_PROMPT"),
                OsString::from(&self.p.prompt),
            ),
        ];
        if let Some(model) = &self.p.model {
            env.push((OsString::from("AIHUB_MODEL"), OsString::from(model)));
            env.push((OsString::from("AIHUB_WORKER_MODEL"), OsString::from(model)));
        }
        if let Some(session_dir) = self.session_dir() {
            env.push((
                OsString::from("AIHUB_SESSION_DIR"),
                session_dir.as_os_str().to_os_string(),
            ));
        }
        env
    }
}

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
        }
    })
    .to_string()
        + "\n"
}

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
    /// The orchestrator should then `wait` (or drop) to collect the exit.
    pub fn request_kill(self, why: KillReason) {
        // If the receiver is gone the child already exited; ignore the error.
        let _ = self.kill_tx.send(why);
        // Detach the supervising task; it will run the kill sequence and finish.
        // The orchestrator typically holds `wait` separately, but request_kill
        // consumes the handle per the contract, so we drop `done` here.
        drop(self.done);
    }

    #[cfg(test)]
    pub(crate) fn finished_for_test(pid: u32, kind: ExitKind) -> Self {
        let (kill_tx, _kill_rx) = oneshot::channel::<KillReason>();
        let done = tokio::spawn(async move { kind });
        Self { pid, kill_tx, done }
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

#[cfg(test)]
fn build_command_args(p: &SpawnParams<'_>) -> Vec<OsString> {
    ProcessRunnerSpec {
        p,
        kind: RunnerKind::parse(p.runner_kind).unwrap_or(RunnerKind::Pi),
    }
    .args()
}

/// Spawn `claude -p` for one issue. Asserts workspace containment, sets up its
/// own process group, pipes the prompt to stdin, and supervises the child in a
/// background task that streams output and enforces timeout/kill.
pub async fn spawn(p: SpawnParams<'_>) -> Result<RunnerHandle> {
    // Hard invariant: the child cwd MUST live inside the workspace root.
    assert_contained(p.workspace_root, p.workspace)
        .context("workspace containment check failed; refusing to spawn child")?;

    let kind = RunnerKind::parse(p.runner_kind)?;
    let spec = ProcessRunnerSpec { p: &p, kind };
    let session_dir = spec.session_dir();
    if let Some(dir) = &session_dir {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating runner session dir {}", dir.display()))?;
    }

    let command = spec.command();
    let args = spec.args();
    let stdin_payload = spec.stdin_payload();
    let runner_event_kind = spec.kind().event_kind();

    let mut cmd = Command::new(&command);
    for arg in &args {
        cmd.arg(arg);
    }
    cmd.envs(spec.env());
    if let Some(dir) = &session_dir {
        cmd.env("AIHUB_SESSION_DIR", dir);
    }

    cmd.current_dir(p.workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Own process group so signalling the negative pid hits the whole tree.
        .process_group(0);

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
        runner_event_kind,
        serde_json::json!({
            "type": "spawn",
            "runner": p.runner_kind,
            "pid": pid,
            "command": command.to_string_lossy(),
            "args": args.iter().map(|a| a.to_string_lossy().to_string()).collect::<Vec<_>>(),
            "session_dir": session_dir.as_ref().map(|d| d.display().to_string()),
        }),
    );

    // Write the rendered prompt to stdin, then drop it to deliver EOF.
    if let (Some(mut stdin), Some(payload)) = (child.stdin.take(), stdin_payload) {
        // Write in a task so a slow/blocked child can't deadlock spawn().
        tokio::spawn(async move {
            let _ = stdin.write_all(&payload).await;
            let _ = stdin.flush().await;
            // drop(stdin) here closes the pipe -> child sees EOF.
        });
    }

    // Stream stdout + stderr concurrently into the log + event ring.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    if let Some(out) = stdout {
        spawn_line_pump(
            out,
            p.issue_id.clone(),
            p.run_id.clone(),
            "stdout",
            runner_event_kind,
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
            runner_event_kind,
            Arc::clone(&p.events),
            Arc::clone(&p.store),
            Arc::clone(&p.last_event_at),
        );
    }

    let (kill_tx, kill_rx) = oneshot::channel::<KillReason>();
    let timeout = Duration::from_millis(p.max_run_timeout_ms);
    let issue_id = p.issue_id.clone();

    // Supervising task: race child exit against timeout and kill requests.
    let done = tokio::spawn(async move { supervise(child, pid, issue_id, timeout, kill_rx).await });

    Ok(RunnerHandle { pid, kill_tx, done })
}

/// Drive the child: wait for exit, the per-attempt timeout, or a kill request.
/// On timeout/kill, SIGTERM the process group, grace, then SIGKILL. Classify
/// the final exit as Normal (code 0) or Abnormal (everything else).
async fn supervise(
    mut child: tokio::process::Child,
    pid: u32,
    issue_id: String,
    timeout: Duration,
    kill_rx: oneshot::Receiver<KillReason>,
) -> ExitKind {
    let kind = tokio::select! {
        // Child exited on its own.
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
        // Per-attempt timeout.
        _ = tokio::time::sleep(timeout) => {
            logging::ev(&issue_id, "timeout", "turn_timeout_ms exceeded; killing");
            ExitKind::Interrupted { reason: "turn_timeout" }
        }
        // Operator stop / reconcile / timeout-from-orchestrator.
        reason = kill_rx => {
            match reason {
                Ok(KillReason::Timeout) => logging::ev(&issue_id, "kill", "reason=timeout"),
                Ok(KillReason::OperatorStop) => logging::ev(&issue_id, "kill", "reason=operator_stop"),
                Ok(KillReason::Reconcile) => logging::ev(&issue_id, "kill", "reason=reconcile"),
                // Sender dropped without sending: handle was dropped; fall
                // through and just ensure the child is gone.
                Err(_) => logging::ev(&issue_id, "kill", "handle dropped"),
            }
            ExitKind::Abnormal(None)
        }
    };

    // We reach here only for timeout/kill paths: terminate the group, then
    // reap so the child does not become a zombie.
    term_then_kill(pid, KILL_GRACE);
    let _ = child.wait().await;
    kind
}

/// Stream one byte source line-by-line into the event ring + SQLite + log,
/// updating `last_event_at` per line. Lines are prefixed `child[ID]:` per PRD.
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
                    let row = normalize_log_row(stream, &clean);
                    let formatted = format!("child[{issue_id}]: {clean}");
                    events.push(formatted.clone());
                    if let Ok(mut t) = last_event_at.lock() {
                        *t = ts;
                    }
                    logging::ev(&issue_id, stream, &clean);
                    // Best-effort SQLite write; don't stall the pump on failure.
                    let payload = serde_json::json!({
                        "type": "protocol_event",
                        "stream": stream,
                        "log_row": row,
                        "text": clean,
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

/// Send SIGTERM to the child's process group, wait `grace`, then SIGKILL the
/// group. Signalling the NEGATIVE pid targets the whole process group.
pub fn term_then_kill(pid: u32, grace: std::time::Duration) {
    let pgid = Pid::from_raw(-(pid as i32));

    // SIGTERM the group. ESRCH (no such process) means it already exited.
    let _ = kill(pgid, Signal::SIGTERM);

    // Grace, then SIGKILL the group. Run synchronously on a blocking thread so
    // callers (including the async supervisor) don't have to await it; this is
    // a best-effort cleanup path.
    std::thread::spawn(move || {
        std::thread::sleep(grace);
        let _ = kill(pgid, Signal::SIGKILL);
    });
}

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

        let pi_spec = ProcessRunnerSpec {
            p: &pi,
            kind: RunnerKind::Pi,
        };
        let claude_spec = ProcessRunnerSpec {
            p: &claude,
            kind: RunnerKind::Claude,
        };

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
    fn codex_spec_runs_app_server_and_cli_gets_aihub_env() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let codex = params("codex", None, workspace, workspace_root);
        let cli = params(
            "cli",
            Some("model-a".to_string()),
            workspace,
            workspace_root,
        );

        let codex_spec = ProcessRunnerSpec {
            p: &codex,
            kind: RunnerKind::Codex,
        };
        let cli_spec = ProcessRunnerSpec {
            p: &cli,
            kind: RunnerKind::Cli,
        };

        assert_eq!(arg_strings(codex_spec.args()), vec!["app-server"]);
        assert!(codex_spec.stdin_payload().is_none());
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
        let pi_spec = ProcessRunnerSpec {
            p: &pi,
            kind: RunnerKind::Pi,
        };

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
}
