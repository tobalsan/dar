//! OpenCode runner extension: one `opencode serve` child, one live session.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use cap_runner::{
    ExitKind, KillReason, RunnerEventSink, RunnerEventStore, RunnerHandle, SpawnParams,
    TurnDecision, TurnEnded,
};
use chrono::{DateTime, Utc};
use host_api::{Extension, RegisterCtx};
use opencode_client::{OpenCodeEvent, OpenCodeServer};
use runner_core::{classify_protocol_line, effective_command, log_ev};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

const EVENT_KIND: &str = "runner.opencode";
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

pub struct RunnerOpenCodeExtension;

impl Extension for RunnerOpenCodeExtension {
    fn id(&self) -> &'static str {
        "runner-opencode"
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            ctx.services
                .service::<dyn cap_runner::Runner>("opencode", Arc::new(OpenCodeRunner))?;
            Ok(())
        })
    }
}

pub struct OpenCodeRunner;

impl cap_runner::Runner for OpenCodeRunner {
    fn spawn<'a>(
        &self,
        params: SpawnParams<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<RunnerHandle>> + Send + 'a>,
    > {
        Box::pin(async move { spawn_opencode(params).await })
    }
}

async fn spawn_opencode(p: SpawnParams<'_>) -> Result<RunnerHandle> {
    host_api::assert_contained(p.workspace_root, p.workspace)
        .context("workspace containment check failed; refusing to spawn opencode")?;

    let command = effective_command(p.command, "opencode");
    let server = OpenCodeServer::spawn(command.clone(), p.workspace).await?;
    let pid = server.pid().context("opencode serve has no pid")?;

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
            "args": ["serve", "--hostname", "127.0.0.1", "--port", "<ephemeral>"],
            "base_url": server.base_url(),
        }),
    );

    let (kill_tx, kill_rx) = oneshot::channel::<KillReason>();
    let (ended_tx, ended_rx) = tokio::sync::mpsc::unbounded_channel::<TurnEnded>();
    let (decision_tx, decision_rx) = tokio::sync::mpsc::unbounded_channel::<TurnDecision>();
    let ctx = TurnLoopCtx {
        issue_id: p.issue_id.clone(),
        run_id: p.run_id.clone(),
        prompt: p.prompt.clone(),
        model: p.model.clone(),
        events: Arc::clone(&p.events),
        store: Arc::clone(&p.store),
        last_event_at: Arc::clone(&p.last_event_at),
    };
    let timeout = Duration::from_millis(p.max_run_timeout_ms);
    let io = TurnIo {
        server,
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

struct TurnLoopCtx {
    issue_id: String,
    run_id: String,
    prompt: String,
    model: Option<String>,
    events: Arc<dyn RunnerEventSink>,
    store: Arc<dyn RunnerEventStore>,
    last_event_at: Arc<Mutex<DateTime<Utc>>>,
}

struct TurnIo {
    server: OpenCodeServer,
    kill_rx: oneshot::Receiver<KillReason>,
    ended_tx: UnboundedSender<TurnEnded>,
    decision_rx: UnboundedReceiver<TurnDecision>,
}

async fn run_turn_loop(ctx: &TurnLoopCtx, io: TurnIo, timeout: Duration) -> ExitKind {
    let TurnIo {
        mut server,
        mut kill_rx,
        ended_tx,
        mut decision_rx,
    } = io;
    let client = server.client();
    let mut events = match client.events().await {
        Ok(events) => events,
        Err(e) => {
            log_ev(
                &ctx.issue_id,
                "error",
                &format!("opening event stream failed: {e:#}"),
            );
            server.kill_and_wait(SHUTDOWN_GRACE).await;
            return ExitKind::Abnormal(None);
        }
    };
    let session_id = match client.create_session(&ctx.issue_id).await {
        Ok(id) => id,
        Err(e) => {
            log_ev(
                &ctx.issue_id,
                "error",
                &format!("creating session failed: {e:#}"),
            );
            server.kill_and_wait(SHUTDOWN_GRACE).await;
            return ExitKind::Abnormal(None);
        }
    };
    persist_event(
        ctx.store.as_ref(),
        Some(&ctx.run_id),
        &ctx.issue_id,
        serde_json::json!({ "type": "session", "session_id": session_id }),
    );

    let deadline = tokio::time::Instant::now() + timeout;
    if let Err(e) = client
        .send_prompt(&session_id, &ctx.prompt, ctx.model.as_deref())
        .await
    {
        log_ev(
            &ctx.issue_id,
            "error",
            &format!("sending prompt failed: {e:#}"),
        );
        server.kill_and_wait(SHUTDOWN_GRACE).await;
        return ExitKind::Abnormal(None);
    }

    loop {
        let boundary = tokio::select! {
            event = events.next_event() => match event {
                Ok(Some(event)) => {
                    emit_event(ctx, &event);
                    is_turn_boundary(&event, &session_id)
                }
                Ok(None) => {
                    log_ev(&ctx.issue_id, "error", "event stream closed unexpectedly");
                    server.kill_and_wait(SHUTDOWN_GRACE).await;
                    return ExitKind::Abnormal(None);
                }
                Err(e) => {
                    log_ev(&ctx.issue_id, "error", &format!("event stream failed: {e:#}"));
                    server.kill_and_wait(SHUTDOWN_GRACE).await;
                    return ExitKind::Abnormal(None);
                }
            },
            status = server.wait() => {
                return classify_child_exit(&ctx.issue_id, status);
            }
            _ = tokio::time::sleep_until(deadline) => {
                log_ev(&ctx.issue_id, "timeout", "max_run_timeout_ms exceeded; killing");
                server.kill_and_wait(SHUTDOWN_GRACE).await;
                return ExitKind::Interrupted { reason: "turn_timeout" };
            }
            reason = &mut kill_rx => {
                return kill_exit(&ctx.issue_id, &mut server, reason).await;
            }
        };

        if !boundary {
            continue;
        }
        if ended_tx.send(TurnEnded).is_err() {
            return finish(&ctx.issue_id, &mut server).await;
        }
        let decision = tokio::select! {
            decision = decision_rx.recv() => decision,
            status = server.wait() => {
                return classify_child_exit(&ctx.issue_id, status);
            }
            _ = tokio::time::sleep_until(deadline) => {
                log_ev(&ctx.issue_id, "timeout", "max_run_timeout_ms exceeded awaiting decision; killing");
                server.kill_and_wait(SHUTDOWN_GRACE).await;
                return ExitKind::Interrupted { reason: "turn_timeout" };
            }
            reason = &mut kill_rx => {
                return kill_exit(&ctx.issue_id, &mut server, reason).await;
            }
        };
        match decision {
            Some(TurnDecision::Continue { prompt }) => {
                if let Err(e) = client
                    .send_prompt(&session_id, &prompt, ctx.model.as_deref())
                    .await
                {
                    log_ev(
                        &ctx.issue_id,
                        "error",
                        &format!("sending continuation failed: {e:#}"),
                    );
                    server.kill_and_wait(SHUTDOWN_GRACE).await;
                    return ExitKind::Abnormal(None);
                }
            }
            Some(TurnDecision::Finish) | None => return finish(&ctx.issue_id, &mut server).await,
        }
    }
}

fn classify_child_exit(issue_id: &str, status: Result<std::process::ExitStatus>) -> ExitKind {
    match status {
        Ok(status) if status.success() => {
            log_ev(issue_id, "exit", "code=0 (normal)");
            ExitKind::Normal
        }
        Ok(status) => {
            log_ev(issue_id, "exit", &format!("status={status} (abnormal)"));
            ExitKind::Abnormal(status.code())
        }
        Err(e) => {
            log_ev(issue_id, "exit", &format!("wait error: {e:#} (abnormal)"));
            ExitKind::Abnormal(None)
        }
    }
}

async fn finish(issue_id: &str, server: &mut OpenCodeServer) -> ExitKind {
    log_ev(issue_id, "finish", "disposing opencode server");
    classify_child_exit(issue_id, server.dispose_then_wait(SHUTDOWN_GRACE).await)
}

async fn kill_exit(
    issue_id: &str,
    server: &mut OpenCodeServer,
    reason: Result<KillReason, oneshot::error::RecvError>,
) -> ExitKind {
    match reason {
        Ok(KillReason::Timeout) => log_ev(issue_id, "kill", "reason=timeout"),
        Ok(KillReason::OperatorStop) => log_ev(issue_id, "kill", "reason=operator_stop"),
        Ok(KillReason::Reconcile) => log_ev(issue_id, "kill", "reason=reconcile"),
        Err(_) => log_ev(issue_id, "kill", "handle dropped"),
    }
    server.kill_and_wait(SHUTDOWN_GRACE).await;
    ExitKind::Interrupted { reason: "killed" }
}

fn is_turn_boundary(event: &OpenCodeEvent, session_id: &str) -> bool {
    let Some(value) = event_payload(event) else {
        return false;
    };
    value.get("type").and_then(|v| v.as_str()) == Some("session.idle")
        && value
            .get("properties")
            .and_then(|p| p.get("sessionID"))
            .and_then(|id| id.as_str())
            == Some(session_id)
}

fn event_payload(event: &OpenCodeEvent) -> Option<serde_json::Value> {
    let value = serde_json::from_str::<serde_json::Value>(&event.data).ok()?;
    if let Some(payload) = value.get("payload") {
        return Some(payload.clone());
    }
    if event.event.as_deref() == value.get("type").and_then(|v| v.as_str()) {
        return Some(value);
    }
    Some(value)
}

fn emit_event(ctx: &TurnLoopCtx, event: &OpenCodeEvent) {
    let ts = Utc::now();
    let clean = if let Some(name) = &event.event {
        format!("{name}: {}", event.data)
    } else {
        event.data.clone()
    };
    let pl = classify_protocol_line("stdout", &event.data);
    ctx.events.push(format!("child[{}]: {clean}", ctx.issue_id));
    if let Ok(mut t) = ctx.last_event_at.lock() {
        *t = ts;
    }
    log_ev(&ctx.issue_id, "sse", &clean);
    let payload = serde_json::json!({
        "type": "protocol_event",
        "stream": "sse",
        "event": event.event,
        "log_row": pl.row_type,
        "text": pl.text,
        "detail": pl.detail,
        "raw": event.data,
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
    use std::path::{Path, PathBuf};

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

    #[test]
    fn session_idle_for_current_session_is_turn_boundary() {
        let event = OpenCodeEvent {
            event: Some("session.idle".to_string()),
            data: r#"{"type":"session.idle","properties":{"sessionID":"s1"}}"#.to_string(),
        };
        assert!(is_turn_boundary(&event, "s1"));
        assert!(!is_turn_boundary(&event, "other"));
    }

    #[test]
    fn payload_wrapped_session_idle_is_turn_boundary() {
        let event = OpenCodeEvent {
            event: None,
            data:
                r#"{"id":"e1","payload":{"type":"session.idle","properties":{"sessionID":"s1"}}}"#
                    .to_string(),
        };
        assert!(is_turn_boundary(&event, "s1"));
    }

    fn write_script(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("fake-opencode.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    async fn spawn_against(dir: &Path, script: &Path) -> RunnerHandle {
        let workspaces = dir.join("workspaces");
        let workspace = workspaces.join("ISSUE-1");
        std::fs::create_dir_all(&workspace).unwrap();
        let params = SpawnParams::builder(
            script.to_str().unwrap(),
            "opencode",
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
        spawn_opencode(params).await.expect("spawn")
    }

    const FAKE_SERVER: &str = r#"PORT=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --port) shift; PORT="$1" ;;
  esac
  shift
done
python3 - "$PORT" <<'PY'
import json, sys, time, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

port = int(sys.argv[1])
prompt_count = 0
dispose = False

class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *args): pass
    def _json(self, obj):
        body = json.dumps(obj).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def do_GET(self):
        global dispose
        if self.path == "/global/health":
            return self._json({"healthy": True, "version": "fake"})
        if self.path == "/global/event":
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("cache-control", "no-cache")
            self.end_headers()
            sent = 0
            while not dispose:
                if prompt_count > sent:
                    sent = prompt_count
                    self.wfile.write(b'event: message.part.updated\n')
                    self.wfile.write(b'data: {"type":"message.part.updated","properties":{"part":{"type":"text","text":"pong"}}}\n\n')
                    self.wfile.write(b'data: {"id":"e1","payload":{"type":"session.idle","properties":{"sessionID":"sess-1"}}}\n\n')
                    self.wfile.flush()
                time.sleep(0.02)
            return
        self.send_error(404)
    def do_POST(self):
        global prompt_count, dispose
        length = int(self.headers.get("content-length", "0") or "0")
        body = self.rfile.read(length).decode()
        if self.path == "/session":
            return self._json({"id": "sess-1", "directory": "."})
        if self.path == "/session/sess-1/prompt_async":
            prompt_count += 1
            with open("received.log", "a") as f:
                f.write(body + "\n")
            self.send_response(204)
            self.send_header("content-length", "0")
            self.end_headers()
            return
        if self.path == "/instance/dispose":
            dispose = True
            self._json(True)
            threading.Thread(target=self.server.shutdown, daemon=True).start()
            return
        self.send_error(404)

ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()
PY"#;

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
        let script = write_script(dir.path(), FAKE_SERVER);
        let mut handle = spawn_against(dir.path(), &script).await;
        wait_for_turn_ended(&mut handle).await;
        handle.send_turn_decision(TurnDecision::Finish);
        let exit = handle.wait().await;
        assert_eq!(exit, ExitKind::Normal, "got {exit:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn continue_feeds_second_prompt_to_same_session() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), FAKE_SERVER);
        let received = dir.path().join("workspaces/ISSUE-1/received.log");
        let mut handle = spawn_against(dir.path(), &script).await;
        wait_for_turn_ended(&mut handle).await;
        handle.send_turn_decision(TurnDecision::Continue {
            prompt: "second prompt".to_string(),
        });
        wait_for_turn_ended(&mut handle).await;
        handle.send_turn_decision(TurnDecision::Finish);
        assert_eq!(handle.wait().await, ExitKind::Normal);
        let log = std::fs::read_to_string(received).unwrap();
        assert!(log.contains("initial prompt"), "{log}");
        assert!(log.contains("second prompt"), "{log}");
    }
}
