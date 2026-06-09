//! Claude Code runner.
//!
//! Spawns `claude -p` in its OWN process group (so SIGTERM/SIGKILL reach the
//! whole subtree), cwd = the per-issue workspace (containment asserted first),
//! pipes the rendered prompt to the child's stdin then closes it, and streams
//! the child's stdout+stderr line-by-line into the log and the recent-events
//! ring (prefixed `child[ID]:`). Tracks pid/started_at/last_event_at.
//!
//! `spawn` returns a `RunnerHandle` the orchestrator awaits and can signal-kill.
//! On timeout or operator/reconcile kill the child's process group is sent
//! SIGTERM, granted a 5s grace, then SIGKILL.

use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::oneshot;

use crate::logging;
use crate::paths::assert_contained;
use crate::state::EventRing;

/// Grace period between SIGTERM and SIGKILL of the child process group.
const KILL_GRACE: Duration = Duration::from_secs(5);

/// How an attempt finished, from the orchestrator's point of view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExitKind {
    /// Process exited with code 0.
    Normal,
    /// Non-zero exit, killed by signal, or timed out.
    Abnormal,
}

/// Why the orchestrator asked to kill a running child.
pub enum KillReason {
    Timeout,
    OperatorStop,
    Reconcile,
}

/// Parameters for spawning one agent run.
pub struct SpawnParams<'a> {
    pub command: &'a str,
    /// Runner kind (e.g. "claude-code"). Determines which CLI flags are added.
    pub runner_kind: &'a str,
    /// Model override; passed as `--model <model>` for claude-code runners.
    pub model: Option<String>,
    pub workspace: &'a Path,
    pub workspace_root: &'a Path,
    pub prompt: String,
    pub issue_id: String,
    pub max_run_timeout_ms: u64,
    pub events: Arc<EventRing>,
    pub last_event_at: Arc<Mutex<DateTime<Utc>>>,
}

/// Handle to a running child. Owns the kill channel and the supervising task's
/// join handle. `wait` / `request_kill` consume the handle; the orchestrator
/// stores it via `Option::take`.
pub struct RunnerHandle {
    pub pid: u32,
    pub started_at: DateTime<Utc>,
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
        self.done.await.unwrap_or(ExitKind::Abnormal)
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
        Self {
            pid,
            started_at: Utc::now(),
            kill_tx,
            done,
        }
    }
}

fn build_command_args(p: &SpawnParams<'_>) -> Vec<OsString> {
    let mut args = Vec::new();
    if p.runner_kind == "claude-code" {
        // Autonomous runner: no human is present to answer Claude's permission
        // prompts, and the workflow needs the child to edit its issue file, which
        // lives outside the workspace cwd (under the agent folder). Bypass the
        // permission sandbox and widen the allowed dirs to the agent folder
        // (parent of the workspace root). See PRD open question on Claude flags.
        let agent_dir = p.workspace_root.parent().unwrap_or(p.workspace_root);
        args.extend([
            OsString::from("-p"),
            OsString::from("--permission-mode"),
            OsString::from("bypassPermissions"),
            OsString::from("--add-dir"),
            agent_dir.as_os_str().to_os_string(),
        ]);
        if let Some(ref model) = p.model {
            args.push(OsString::from("--model"));
            args.push(OsString::from(model));
        }
    }
    args
}

/// Spawn `claude -p` for one issue. Asserts workspace containment, sets up its
/// own process group, pipes the prompt to stdin, and supervises the child in a
/// background task that streams output and enforces timeout/kill.
pub async fn spawn(p: SpawnParams<'_>) -> Result<RunnerHandle> {
    // Hard invariant: the child cwd MUST live inside the workspace root.
    assert_contained(p.workspace_root, p.workspace)
        .context("workspace containment check failed; refusing to spawn child")?;

    let mut cmd = Command::new(p.command);
    for arg in build_command_args(&p) {
        cmd.arg(arg);
    }

    cmd.current_dir(p.workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Own process group so signalling the negative pid hits the whole tree.
        .process_group(0);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning `{} -p` in {}", p.command, p.workspace.display()))?;

    let pid = child
        .id()
        .context("child has no pid immediately after spawn")?;
    let started_at = Utc::now();

    logging::ev(
        &p.issue_id,
        "spawn",
        &format!("pid={pid} cwd={}", p.workspace.display()),
    );

    // Write the rendered prompt to stdin, then drop it to deliver EOF.
    if let Some(mut stdin) = child.stdin.take() {
        let prompt = p.prompt.clone();
        // Write in a task so a slow/blocked child can't deadlock spawn().
        tokio::spawn(async move {
            let _ = stdin.write_all(prompt.as_bytes()).await;
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
            "stdout",
            Arc::clone(&p.events),
            Arc::clone(&p.last_event_at),
        );
    }
    if let Some(err) = stderr {
        spawn_line_pump(
            err,
            p.issue_id.clone(),
            "stderr",
            Arc::clone(&p.events),
            Arc::clone(&p.last_event_at),
        );
    }

    let (kill_tx, kill_rx) = oneshot::channel::<KillReason>();
    let timeout = Duration::from_millis(p.max_run_timeout_ms);
    let issue_id = p.issue_id.clone();

    // Supervising task: race child exit against timeout and kill requests.
    let done = tokio::spawn(async move {
        supervise(child, pid, issue_id, timeout, kill_rx).await
    });

    Ok(RunnerHandle {
        pid,
        started_at,
        kill_tx,
        done,
    })
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
                    logging::ev(&issue_id, "exit", &format!("status={s} (abnormal)"));
                    return ExitKind::Abnormal;
                }
                Err(e) => {
                    logging::ev(&issue_id, "exit", &format!("wait error: {e} (abnormal)"));
                    return ExitKind::Abnormal;
                }
            }
        }
        // Per-attempt timeout.
        _ = tokio::time::sleep(timeout) => {
            logging::ev(&issue_id, "timeout", "max_run_timeout_ms exceeded; killing");
            ExitKind::Abnormal
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
            ExitKind::Abnormal
        }
    };

    // We reach here only for timeout/kill paths: terminate the group, then
    // reap so the child does not become a zombie.
    term_then_kill(pid, KILL_GRACE);
    let _ = child.wait().await;
    kind
}

/// Stream one byte source line-by-line into the event ring + log, updating
/// `last_event_at` per line. Lines are prefixed `child[ID]:` per PRD.
fn spawn_line_pump<R>(
    reader: R,
    issue_id: String,
    stream: &'static str,
    events: Arc<EventRing>,
    last_event_at: Arc<Mutex<DateTime<Utc>>>,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let formatted = format!("child[{issue_id}]: {line}");
                    events.push(formatted.clone());
                    if let Ok(mut t) = last_event_at.lock() {
                        *t = Utc::now();
                    }
                    logging::ev(&issue_id, stream, &line);
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
    use chrono::Utc;
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
            prompt: String::new(),
            issue_id: "ISSUE-1".to_string(),
            max_run_timeout_ms: 1000,
            events: Arc::new(EventRing::new()),
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
}
