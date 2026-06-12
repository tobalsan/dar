//! Pi runner extension — one long-lived `pi --mode rpc` child per run with an
//! in-process turn loop (ALG-234, "Tracker-driven run loop").
//!
//! The child speaks stock pi's JSONL RPC protocol (the same one `chat-pi`
//! drives): a turn is started with a `{"type":"prompt"}` line; the agent
//! streams `message_update` / `tool_execution_*` events and finishes a turn with
//! `agent_end` (or an assistant `error`). At each turn boundary the runner sends
//! a [`TurnEnded`] and parks for a [`TurnDecision`]: `Continue { prompt }` feeds
//! the next prompt into the SAME live session, `Finish` shuts the child down
//! cleanly. Run completion is decided by the orchestrator (the tracker is the
//! state machine) — never by the runner.
//!
//! Session persistence: pi stores/resumes sessions under `--session-dir`, so a
//! per-issue session dir means an abnormal-exit cold respawn resumes context.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use cap_runner::{
    ExitKind, KillReason, RunnerEventSink, RunnerEventStore, RunnerHandle, SpawnParams, TurnDecision,
    TurnEnded,
};
use chrono::{DateTime, Utc};
use host_api::{Extension, RegisterCtx};
use runner_core::{
    classify_protocol_line, common_env, effective_command, log_ev, scrub_loaded_env,
    setup_process_group, strip_ansi, term_then_kill,
};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

const EVENT_KIND: &str = "runner.pi";
/// SIGTERM-to-SIGKILL grace passed to `term_then_kill` on shutdown/kill.
const KILL_GRACE: Duration = Duration::from_secs(5);

pub struct RunnerPiExtension;

impl Extension for RunnerPiExtension {
    fn id(&self) -> &'static str {
        "runner-pi"
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            ctx.services
                .service::<dyn cap_runner::Runner>("pi", Arc::new(PiRunner))?;
            Ok(())
        })
    }
}

pub struct PiRunner;

impl cap_runner::Runner for PiRunner {
    fn spawn<'a>(
        &self,
        params: SpawnParams<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<RunnerHandle>> + Send + 'a>,
    > {
        Box::pin(async move { spawn_pi(params).await })
    }
}

fn session_dir(p: &SpawnParams<'_>) -> PathBuf {
    p.agent_root.join("pi-sessions").join(&p.issue_id)
}

/// Build args for `pi --mode rpc`. Sessions are stored/resumed under
/// `--session-dir`; `--model` forwards the configured model when set.
fn pi_args(session_dir: &Path, model: Option<&str>) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("--mode"),
        OsString::from("rpc"),
        OsString::from("--session-dir"),
        session_dir.as_os_str().to_os_string(),
    ];
    if let Some(model) = model {
        args.push(OsString::from("--model"));
        args.push(OsString::from(model));
    }
    args
}

/// One `{"type":"prompt"}` JSONL line carrying the next turn's prompt.
fn prompt_command(id: &str, message: &str) -> String {
    serde_json::json!({ "id": id, "type": "prompt", "message": message }).to_string() + "\n"
}

// ---------------------------------------------------------------------------
// Spawn
// ---------------------------------------------------------------------------

async fn spawn_pi(p: SpawnParams<'_>) -> Result<RunnerHandle> {
    host_api::assert_contained(p.workspace_root, p.workspace)
        .context("workspace containment check failed; refusing to spawn child")?;

    let session_dir = session_dir(&p);
    std::fs::create_dir_all(&session_dir)
        .with_context(|| format!("creating pi session dir {}", session_dir.display()))?;

    let command = effective_command(p.command, "pi");
    let args = pi_args(&session_dir, p.model.as_deref());

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
    persist_event(
        p.store.as_ref(),
        Some(&p.run_id),
        &p.issue_id,
        serde_json::json!({
            "type": "spawn",
            "runner": p.runner_kind,
            "pid": pid,
            "command": command.to_string_lossy(),
            "args": args.iter().map(|a| a.to_string_lossy().to_string()).collect::<Vec<_>>(),
            "session_dir": session_dir.display().to_string(),
        }),
    );

    let stdin = child.stdin.take().context("no stdin handle after spawn")?;
    let stdout = child.stdout.take().context("no stdout handle after spawn")?;
    let stderr = child.stderr.take().context("no stderr handle after spawn")?;

    // stderr streams straight through to events/store (no protocol meaning).
    runner_core::spawn_line_pump(
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

    let ctx = TurnLoopCtx {
        pid,
        issue_id: p.issue_id.clone(),
        run_id: p.run_id.clone(),
        prompt: p.prompt.clone(),
        events: Arc::clone(&p.events),
        store: Arc::clone(&p.store),
        last_event_at: Arc::clone(&p.last_event_at),
    };

    let io = TurnIo {
        child,
        stdin,
        stdout,
        kill_rx,
        ended_tx,
        decision_rx,
    };

    let done = tokio::spawn(async move { run_turn_loop(&ctx, io, timeout).await });

    Ok(RunnerHandle::with_turns(
        pid,
        kill_tx,
        done,
        ended_rx,
        decision_tx,
    ))
}

// ---------------------------------------------------------------------------
// Turn loop
// ---------------------------------------------------------------------------

/// Everything the supervising task needs that is not stdio/channel state.
struct TurnLoopCtx {
    pid: u32,
    issue_id: String,
    run_id: String,
    prompt: String,
    events: Arc<dyn RunnerEventSink>,
    store: Arc<dyn RunnerEventStore>,
    last_event_at: Arc<Mutex<DateTime<Utc>>>,
}

/// The owned child + stdio + turn channels the supervising loop drives.
struct TurnIo {
    child: tokio::process::Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    kill_rx: oneshot::Receiver<KillReason>,
    ended_tx: UnboundedSender<TurnEnded>,
    decision_rx: UnboundedReceiver<TurnDecision>,
}

/// How one turn's event pump stopped.
enum TurnOutcome {
    /// `agent_end` (or assistant error) — child is idle awaiting the next turn.
    Ended,
    /// Child stdout closed — the child is exiting on its own.
    Exited,
}

/// Drive the long-lived child: feed the initial prompt, pump one turn, park at
/// the boundary for a [`TurnDecision`], and either feed the next prompt or shut
/// down. The whole loop is bounded by `timeout` and interruptible by `kill_rx`
/// in every state (including while awaiting a decision).
async fn run_turn_loop(ctx: &TurnLoopCtx, io: TurnIo, timeout: Duration) -> ExitKind {
    let TurnIo {
        mut child,
        mut stdin,
        stdout,
        mut kill_rx,
        ended_tx,
        mut decision_rx,
    } = io;
    let mut lines = BufReader::new(stdout).lines();
    let deadline = tokio::time::Instant::now() + timeout;
    let mut turn: u64 = 0;

    // Feed the initial prompt as the first user message.
    if let Err(e) = write_prompt(&mut stdin, &mut turn, &ctx.prompt).await {
        log_ev(&ctx.issue_id, "error", &format!("stdin write failed: {e}"));
        term_then_kill(ctx.pid, KILL_GRACE);
        return ExitKind::Abnormal(None);
    }

    loop {
        // --- pump one turn, racing the kill channel and the hard ceiling ---
        let outcome = tokio::select! {
            outcome = pump_one_turn(ctx, &mut lines, &mut stdin) => outcome,
            _ = tokio::time::sleep_until(deadline) => {
                log_ev(&ctx.issue_id, "timeout", "max_run_timeout_ms exceeded; killing");
                term_then_kill(ctx.pid, KILL_GRACE);
                let _ = child.wait().await;
                return ExitKind::Interrupted { reason: "turn_timeout" };
            }
            reason = &mut kill_rx => {
                return kill_exit(&ctx.issue_id, ctx.pid, &mut child, reason).await;
            }
        };

        match outcome {
            // stdout closed: the child is exiting. `child.wait()` gives the
            // authoritative exit code (the pump only saw EOF).
            TurnOutcome::Exited => return reap_exit(ctx, &mut child).await,
            TurnOutcome::Ended => {}
        }

        // --- turn boundary: report idle and park for a decision ---
        if ended_tx.send(TurnEnded).is_err() {
            // Orchestrator dropped the handle: shut down gracefully.
            return finish(&ctx.issue_id, ctx.pid, &mut child, &mut stdin).await;
        }

        let decision = tokio::select! {
            decision = decision_rx.recv() => decision,
            _ = tokio::time::sleep_until(deadline) => {
                log_ev(&ctx.issue_id, "timeout", "max_run_timeout_ms exceeded awaiting decision; killing");
                term_then_kill(ctx.pid, KILL_GRACE);
                let _ = child.wait().await;
                return ExitKind::Interrupted { reason: "turn_timeout" };
            }
            reason = &mut kill_rx => {
                return kill_exit(&ctx.issue_id, ctx.pid, &mut child, reason).await;
            }
        };

        match decision {
            Some(TurnDecision::Continue { prompt }) => {
                if let Err(e) = write_prompt(&mut stdin, &mut turn, &prompt).await {
                    log_ev(&ctx.issue_id, "error", &format!("stdin write failed: {e}"));
                    term_then_kill(ctx.pid, KILL_GRACE);
                    return ExitKind::Abnormal(None);
                }
                // loop: pump the continuation turn.
            }
            // Finish, or the decision channel closed (handle dropped).
            Some(TurnDecision::Finish) | None => {
                return finish(&ctx.issue_id, ctx.pid, &mut child, &mut stdin).await;
            }
        }
    }
}

/// Pump child stdout until the turn ends (`agent_end` / assistant error) or the
/// child exits. Streams every line to events/store and answers blocking UI
/// dialogs so the turn keeps moving.
async fn pump_one_turn(
    ctx: &TurnLoopCtx,
    lines: &mut tokio::io::Lines<BufReader<ChildStdout>>,
    stdin: &mut ChildStdin,
) -> TurnOutcome {
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            // stdout EOF: child has closed its output, it is exiting.
            Ok(None) => return TurnOutcome::Exited,
            Err(e) => {
                log_ev(&ctx.issue_id, "error", &format!("stdout read error: {e}"));
                return TurnOutcome::Exited;
            }
        };

        let clean = strip_ansi(line.trim_end_matches('\r'));
        emit_line(ctx, &clean);

        match map_line(&clean) {
            Mapped::TurnEnded => return TurnOutcome::Ended,
            Mapped::AutoRespond(reply) => {
                let _ = stdin.write_all(reply.as_bytes()).await;
                let _ = stdin.flush().await;
            }
            Mapped::Ignore => {}
        }
    }
}

/// Await the child's own exit and classify it: code 0 → `Normal`, anything
/// else (non-zero, signal, wait error) → `Abnormal`.
async fn reap_exit(ctx: &TurnLoopCtx, child: &mut tokio::process::Child) -> ExitKind {
    match child.wait().await {
        Ok(status) if status.success() => {
            log_ev(&ctx.issue_id, "exit", "code=0 (normal)");
            ExitKind::Normal
        }
        Ok(status) => {
            log_ev(&ctx.issue_id, "exit", &format!("status={status} (abnormal)"));
            ExitKind::Abnormal(status.code())
        }
        Err(e) => {
            log_ev(&ctx.issue_id, "exit", &format!("wait error: {e} (abnormal)"));
            ExitKind::Abnormal(None)
        }
    }
}

/// Graceful shutdown: drop stdin (pi's clean-quit signal) then escalate to a
/// process-group term/kill after the grace. Always resolves `Normal`.
async fn finish(
    issue_id: &str,
    pid: u32,
    child: &mut tokio::process::Child,
    stdin: &mut ChildStdin,
) -> ExitKind {
    log_ev(issue_id, "finish", "graceful shutdown");
    let _ = stdin.shutdown().await; // EOF → clean pi quit
    term_then_kill(pid, KILL_GRACE);
    let _ = child.wait().await;
    ExitKind::Normal
}

/// Map a kill-channel result to the interrupted exit, killing then reaping.
async fn kill_exit(
    issue_id: &str,
    pid: u32,
    child: &mut tokio::process::Child,
    reason: Result<KillReason, oneshot::error::RecvError>,
) -> ExitKind {
    match reason {
        Ok(KillReason::Timeout) => log_ev(issue_id, "kill", "reason=timeout"),
        Ok(KillReason::OperatorStop) => log_ev(issue_id, "kill", "reason=operator_stop"),
        Ok(KillReason::Reconcile) => log_ev(issue_id, "kill", "reason=reconcile"),
        Err(_) => log_ev(issue_id, "kill", "handle dropped"),
    }
    term_then_kill(pid, KILL_GRACE);
    let _ = child.wait().await;
    ExitKind::Interrupted { reason: "killed" }
}

async fn write_prompt(stdin: &mut ChildStdin, turn: &mut u64, prompt: &str) -> std::io::Result<()> {
    *turn += 1;
    let line = prompt_command(&format!("t{turn}"), prompt);
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await
}

// ---------------------------------------------------------------------------
// Line mapping
// ---------------------------------------------------------------------------

/// What one stdout line asks of the pump.
enum Mapped {
    /// `agent_end` or an assistant error: the turn is over.
    TurnEnded,
    /// A blocking `extension_ui_request`: answer on stdin so the turn proceeds.
    AutoRespond(String),
    Ignore,
}

fn map_line(line: &str) -> Mapped {
    if line.trim().is_empty() {
        return Mapped::Ignore;
    }
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Mapped::Ignore;
    };
    match value.get("type").and_then(Value::as_str) {
        // Turn boundary: pi finished the agent loop and is awaiting the next prompt.
        Some("agent_end") => Mapped::TurnEnded,
        // An assistant error (including `aborted`) also ends the turn.
        Some("message_update") if is_assistant_error(&value) => Mapped::TurnEnded,
        Some("extension_ui_request") => map_ui_request(&value),
        _ => Mapped::Ignore,
    }
}

fn is_assistant_error(value: &Value) -> bool {
    value
        .get("assistantMessageEvent")
        .and_then(|e| e.get("type"))
        .and_then(Value::as_str)
        == Some("error")
}

fn map_ui_request(value: &Value) -> Mapped {
    let method = value.get("method").and_then(Value::as_str).unwrap_or("");
    let reply_value = match method {
        "confirm" => Value::Bool(true),
        "select" => first_option(value).unwrap_or_else(|| Value::String(String::new())),
        "input" | "editor" => Value::String(String::new()),
        // notify / setStatus / setWidget / setTitle and unknowns: no reply.
        _ => return Mapped::Ignore,
    };
    let reply = serde_json::json!({
        "type": "extension_ui_response",
        "id": value.get("id").cloned().unwrap_or(Value::Null),
        "value": reply_value,
    })
    .to_string()
        + "\n";
    Mapped::AutoRespond(reply)
}

fn first_option(value: &Value) -> Option<Value> {
    value
        .get("options")
        .or_else(|| value.get("params").and_then(|p| p.get("options")))
        .and_then(Value::as_array)
        .and_then(|options| options.first())
        .cloned()
}

// ---------------------------------------------------------------------------
// Event sink / store
// ---------------------------------------------------------------------------

/// Stream one stdout line to the event sink + store and bump `last_event_at`.
fn emit_line(ctx: &TurnLoopCtx, clean: &str) {
    let ts = Utc::now();
    let pl = classify_protocol_line("stdout", clean);
    ctx.events.push(format!("child[{}]: {clean}", ctx.issue_id));
    if let Ok(mut t) = ctx.last_event_at.lock() {
        *t = ts;
    }
    log_ev(&ctx.issue_id, "stdout", clean);
    let payload = serde_json::json!({
        "type": "protocol_event",
        "stream": "stdout",
        "log_row": pl.row_type,
        "text": pl.text,
        "detail": pl.detail,
    })
    .to_string();
    ctx.store
        .insert_event(Some(&ctx.run_id), &ctx.issue_id, EVENT_KIND, &payload, ts);
}

fn persist_event(
    store: &dyn RunnerEventStore,
    run_id: Option<&str>,
    issue_id: &str,
    value: serde_json::Value,
) {
    store.insert_event(run_id, issue_id, EVENT_KIND, &value.to_string(), Utc::now());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
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

    // -- arg / command shape --------------------------------------------------

    #[test]
    fn pi_args_carry_rpc_mode_session_dir_and_optional_model() {
        let dir = Path::new("/agent/pi-sessions/ISSUE-1");
        assert_eq!(
            pi_args(dir, None),
            vec![
                OsString::from("--mode"),
                OsString::from("rpc"),
                OsString::from("--session-dir"),
                OsString::from("/agent/pi-sessions/ISSUE-1"),
            ]
        );
        let args = pi_args(dir, Some("gpt-5"));
        assert_eq!(args[4], OsString::from("--model"));
        assert_eq!(args[5], OsString::from("gpt-5"));
    }

    #[test]
    fn session_dir_is_per_issue_under_agent_root() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let p = params(None, workspace, workspace_root);
        assert_eq!(
            session_dir(&p),
            PathBuf::from("/tmp/agent/pi-sessions/ISSUE-1")
        );
    }

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

    // -- line mapping ---------------------------------------------------------

    #[test]
    fn agent_end_maps_to_turn_ended() {
        assert!(matches!(map_line(r#"{"type":"agent_end"}"#), Mapped::TurnEnded));
    }

    #[test]
    fn assistant_error_maps_to_turn_ended() {
        let line = r#"{"type":"message_update","assistantMessageEvent":{"type":"error","reason":"aborted"}}"#;
        assert!(matches!(map_line(line), Mapped::TurnEnded));
    }

    #[test]
    fn deltas_and_unknowns_are_ignored() {
        let delta = r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"hi"}}"#;
        assert!(matches!(map_line(delta), Mapped::Ignore));
        assert!(matches!(map_line(r#"{"type":"queue_update"}"#), Mapped::Ignore));
        assert!(matches!(map_line("not json"), Mapped::Ignore));
        assert!(matches!(map_line("   "), Mapped::Ignore));
    }

    #[test]
    fn confirm_dialog_is_auto_answered_true() {
        let line = r#"{"type":"extension_ui_request","id":"d1","method":"confirm"}"#;
        match map_line(line) {
            Mapped::AutoRespond(reply) => {
                let v: Value = serde_json::from_str(reply.trim_end()).unwrap();
                assert_eq!(v["type"], "extension_ui_response");
                assert_eq!(v["id"], "d1");
                assert_eq!(v["value"], Value::Bool(true));
            }
            _ => panic!("expected AutoRespond"),
        }
    }

    #[test]
    fn fire_and_forget_dialogs_are_ignored() {
        assert!(matches!(
            map_line(r#"{"type":"extension_ui_request","id":"n1","method":"notify"}"#),
            Mapped::Ignore
        ));
    }

    // -- spawn-level integration (fake `pi --mode rpc` shell script) ----------

    fn params<'a>(
        model: Option<String>,
        workspace: &'a Path,
        workspace_root: &'a Path,
    ) -> SpawnParams<'a> {
        SpawnParams::builder(
            "runner",
            "pi",
            workspace,
            workspace_root,
            workspace_root.parent().unwrap_or(workspace_root),
            "initial prompt".to_string(),
            "ISSUE-1".to_string(),
            "run-1".to_string(),
            10_000,
            Arc::new(NullSink),
            Arc::new(NullStore),
            Arc::new(Mutex::new(Utc::now())),
        )
        .model(model)
        .build()
    }

    fn write_script(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("fake-pi.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Build a real spawn against `script`, with a fresh workspace under `dir`.
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
                "pi",
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
            match spawn_pi(params).await {
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
                .map_or(false, |io| io.raw_os_error() == Some(26))
        })
    }

    /// Echo stub: on each prompt streams a text delta then `agent_end`, and
    /// appends every received line to `$AGENT_WORKSPACE/received.log`.
    const ECHO_STUB: &str = r#"while IFS= read -r line; do
  printf '%s\n' "$line" >> "$AGENT_WORKSPACE/received.log"
  case "$line" in
    *'"type":"prompt"'*)
      printf '%s\n' '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"pong"}}'
      printf '%s\n' '{"type":"agent_end"}'
      ;;
  esac
done"#;

    async fn wait_for_turn_ended(handle: &mut RunnerHandle) {
        for _ in 0..200 {
            if handle.try_recv_turn_ended() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("no TurnEnded within deadline");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn happy_path_turn_then_finish_returns_normal() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), ECHO_STUB);

        let mut handle = spawn_against(dir.path(), &script).await;
        wait_for_turn_ended(&mut handle).await;
        handle.send_turn_decision(TurnDecision::Finish);

        let exit = handle.wait().await;
        assert_eq!(exit, ExitKind::Normal, "got {exit:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn continue_feeds_a_second_prompt_into_the_same_process() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), ECHO_STUB);
        let received = dir.path().join("workspaces/ISSUE-1/received.log");

        let mut handle = spawn_against(dir.path(), &script).await;
        wait_for_turn_ended(&mut handle).await;
        handle.send_turn_decision(TurnDecision::Continue {
            prompt: "second prompt".to_string(),
        });
        wait_for_turn_ended(&mut handle).await;
        handle.send_turn_decision(TurnDecision::Finish);
        assert_eq!(handle.wait().await, ExitKind::Normal);

        // The SAME process saw two distinct prompts.
        let log = std::fs::read_to_string(&received).unwrap();
        let prompts: Vec<&str> = log.lines().filter(|l| l.contains("\"type\":\"prompt\"")).collect();
        assert_eq!(prompts.len(), 2, "expected two prompts, got: {log}");
        assert!(prompts[0].contains("initial prompt"), "first: {}", prompts[0]);
        assert!(prompts[1].contains("second prompt"), "second: {}", prompts[1]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn child_exit_nonzero_mid_turn_is_abnormal() {
        // Never answers the prompt; exits 7 as soon as it has read one line.
        let stub = r#"while IFS= read -r line; do
  exit 7
done"#;
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), stub);

        let handle = spawn_against(dir.path(), &script).await;
        let exit = handle.wait().await;
        assert_eq!(exit, ExitKind::Abnormal(Some(7)), "got {exit:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn kill_while_awaiting_decision_is_interrupted_and_reaps_child() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), ECHO_STUB);

        let mut handle = spawn_against(dir.path(), &script).await;
        let pid = handle.pid();
        wait_for_turn_ended(&mut handle).await;

        // Kill instead of deciding.
        let exit = handle.request_kill_and_wait(KillReason::OperatorStop).await;
        assert!(
            matches!(exit, ExitKind::Interrupted { .. }),
            "expected Interrupted, got {exit:?}"
        );

        // Child (and its group) is reaped.
        let pgid = nix::unistd::Pid::from_raw(-(pid as i32));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if nix::sys::signal::kill(pgid, None).is_err() {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "child {pid} still alive");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}
