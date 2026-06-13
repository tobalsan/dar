//! OpenCode runner extension: one `opencode serve` child, one live session.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
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
use runner_core::{
    classify_opencode_event, common_env, effective_command, env_with_session_dir, log_ev,
};
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

/// opencode wants `provider/model`. Fold the separate provider field in when the
/// model isn't already qualified with a `/`.
fn effective_model(model: Option<&str>, provider: Option<&str>) -> Option<String> {
    let model = model?;
    match provider {
        Some(p) if !p.is_empty() && !model.contains('/') => Some(format!("{p}/{model}")),
        _ => Some(model.to_string()),
    }
}

async fn spawn_opencode(p: SpawnParams<'_>) -> Result<RunnerHandle> {
    host_api::assert_contained(p.workspace_root, p.workspace)
        .context("workspace containment check failed; refusing to spawn opencode")?;

    let model = effective_model(p.model.as_deref(), p.provider.as_deref());
    let session_dir = session_dir(&p);
    std::fs::create_dir_all(session_dir.join("config"))
        .with_context(|| format!("creating opencode config dir {}", session_dir.display()))?;
    seed_opencode_auth(&session_dir)?;
    write_opencode_config(&session_dir, model.as_deref())?;

    let command = effective_command(p.command, "opencode");
    let args = opencode_args(0);
    let env = opencode_env(&p, &session_dir, model.as_deref());
    let server = OpenCodeServer::spawn(command.clone(), args.clone(), env, p.workspace).await?;
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
            "args": args.iter().map(|a| {
                if a == "0" { "<ephemeral>".to_string() } else { a.to_string_lossy().to_string() }
            }).collect::<Vec<_>>(),
            "base_url": server.base_url(),
            "session_dir": session_dir.display().to_string(),
        }),
    );

    let (kill_tx, kill_rx) = oneshot::channel::<KillReason>();
    let (ended_tx, ended_rx) = tokio::sync::mpsc::unbounded_channel::<TurnEnded>();
    let (decision_tx, decision_rx) = tokio::sync::mpsc::unbounded_channel::<TurnDecision>();
    let ctx = TurnLoopCtx {
        issue_id: p.issue_id.clone(),
        run_id: p.run_id.clone(),
        prompt: p.prompt.clone(),
        model: model.clone(),
        events: Arc::clone(&p.events),
        store: Arc::clone(&p.store),
        last_event_at: Arc::clone(&p.last_event_at),
        seen_parts: Mutex::new(HashSet::new()),
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

fn session_dir(p: &SpawnParams<'_>) -> PathBuf {
    p.agent_root.join("opencode-sessions").join(&p.issue_id)
}

fn opencode_args(port: u16) -> Vec<OsString> {
    vec![
        OsString::from("serve"),
        OsString::from("--hostname"),
        OsString::from("127.0.0.1"),
        OsString::from("--port"),
        OsString::from(port.to_string()),
    ]
}

fn opencode_env(
    p: &SpawnParams<'_>,
    session_dir: &Path,
    model: Option<&str>,
) -> Vec<(OsString, OsString)> {
    let config_dir = session_dir.join("config");
    let mut env = env_with_session_dir(common_env(p), session_dir);
    env.push((
        OsString::from("OPENCODE_CONFIG"),
        config_dir.join("opencode.json").as_os_str().to_os_string(),
    ));
    env.push((
        OsString::from("OPENCODE_CONFIG_DIR"),
        config_dir.as_os_str().to_os_string(),
    ));
    env.push((
        OsString::from("OPENCODE_CONFIG_CONTENT"),
        OsString::from(opencode_config(model).to_string()),
    ));
    env.push((
        OsString::from("XDG_DATA_HOME"),
        session_dir.join("data").as_os_str().to_os_string(),
    ));
    env.push((
        OsString::from("XDG_STATE_HOME"),
        session_dir.join("state").as_os_str().to_os_string(),
    ));
    env.push((
        OsString::from("XDG_CACHE_HOME"),
        session_dir.join("cache").as_os_str().to_os_string(),
    ));
    env
}

fn opencode_config(model: Option<&str>) -> serde_json::Value {
    let mut config = serde_json::json!({
        "$schema": "https://opencode.ai/config.json",
        "permission": {
            "*": "allow",
            "bash": "allow",
            "doom_loop": "allow",
            "edit": "allow",
            "external_directory": "allow",
            "glob": "allow",
            "grep": "allow",
            "list": "allow",
            "lsp": "allow",
            "question": "allow",
            "read": "allow",
            "skill": "allow",
            "task": "allow",
            "todowrite": "allow",
            "todoread": "allow",
            "webfetch": "allow",
            "websearch": "allow",
            "write": "allow",
        },
    });
    if let Some(model) = model {
        config["model"] = serde_json::Value::String(model.to_string());
    }
    config
}

fn write_opencode_config(session_dir: &Path, model: Option<&str>) -> Result<()> {
    let path = session_dir.join("config").join("opencode.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&opencode_config(model))?)
        .with_context(|| format!("writing opencode config {}", path.display()))
}

/// opencode reads credentials from `$XDG_DATA_HOME/opencode`, defaulting to
/// `~/.local/share/opencode`. The runner isolates XDG_DATA_HOME per issue, so
/// we resolve the *host* location here to copy global auth into the sandbox.
fn host_opencode_data_dir() -> Option<PathBuf> {
    if let Some(x) = std::env::var_os("XDG_DATA_HOME") {
        let p = PathBuf::from(x);
        if !p.as_os_str().is_empty() {
            return Some(p.join("opencode"));
        }
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/opencode"))
}

/// Copy the host's global opencode credentials into the isolated XDG_DATA_HOME
/// so custom providers (e.g. `opencode-go`) authenticate. Best-effort: missing
/// source files are skipped, not an error.
fn seed_opencode_auth(session_dir: &Path) -> Result<()> {
    let Some(src) = host_opencode_data_dir() else {
        return Ok(());
    };
    seed_opencode_auth_from(&src, session_dir)
}

fn seed_opencode_auth_from(src: &Path, session_dir: &Path) -> Result<()> {
    let dst = session_dir.join("data").join("opencode");
    for file in ["auth.json", "account.json"] {
        let from = src.join(file);
        if from.exists() {
            std::fs::create_dir_all(&dst)
                .with_context(|| format!("creating opencode data dir {}", dst.display()))?;
            std::fs::copy(&from, dst.join(file))
                .with_context(|| format!("seeding opencode {file}"))?;
        }
    }
    Ok(())
}

struct TurnLoopCtx {
    issue_id: String,
    run_id: String,
    prompt: String,
    model: Option<String>,
    events: Arc<dyn RunnerEventSink>,
    store: Arc<dyn RunnerEventStore>,
    last_event_at: Arc<Mutex<DateTime<Utc>>>,
    /// Part ids whose finalized card has already been stored, keyed
    /// `"{part_id}:call"` / `"{part_id}:out"` so a re-sent terminal snapshot
    /// for the same opencode part never duplicates its dashboard rows.
    seen_parts: Mutex<HashSet<String>>,
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
                    if let Some(permission_id) = permission_request_id(&event, &session_id) {
                        if let Err(e) = client
                            .respond_permission(&session_id, &permission_id, "once", false)
                            .await
                        {
                            log_ev(
                                &ctx.issue_id,
                                "error",
                                &format!("permission auto-response failed: {e:#}"),
                            );
                            server.kill_and_wait(SHUTDOWN_GRACE).await;
                            return ExitKind::Abnormal(None);
                        }
                    }
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

fn permission_request_id(event: &OpenCodeEvent, session_id: &str) -> Option<String> {
    let value = event_payload(event)?;
    let type_name = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if !type_name.contains("permission") {
        return None;
    }
    let properties = value.get("properties").unwrap_or(&value);
    let event_session = properties
        .get("sessionID")
        .or_else(|| properties.get("sessionId"))
        .and_then(|id| id.as_str());
    if event_session.is_some() && event_session != Some(session_id) {
        return None;
    }
    properties
        .get("permissionID")
        .or_else(|| properties.get("permissionId"))
        .or_else(|| properties.get("id"))
        .and_then(|id| id.as_str())
        .map(str::to_string)
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
    // Liveness + live log always fire, even for hidden/streaming events, so the
    // stall timer and the frontend-log feed see every SSE frame.
    ctx.events.push(format!("child[{}]: {clean}", ctx.issue_id));
    if let Ok(mut t) = ctx.last_event_at.lock() {
        *t = ts;
    }
    log_ev(&ctx.issue_id, "sse", &clean);

    // Only finalized parts become dashboard cards; streaming snapshots and
    // lifecycle noise classify to an empty row_type and are skipped here.
    let Some(payload) = event_payload(event) else {
        return;
    };
    let Some(pl) = classify_opencode_event(&payload) else {
        return;
    };
    if pl.row_type.is_empty() {
        return;
    }

    // opencode re-sends a part's terminal snapshot multiple times; store each
    // finalized row once, keyed by the part id.
    let part_id = payload
        .get("properties")
        .and_then(|p| p.get("part"))
        .and_then(|part| part.get("id"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();

    store_card(ctx, event, &format!("{part_id}:call"), pl.row_type, &pl.text, &pl.detail, ts);

    // A completed tool needs a second tool_output row carrying its output.
    if pl.row_type == "tool_call" {
        let output = payload
            .get("properties")
            .and_then(|p| p.get("part"))
            .and_then(|part| part.get("state"))
            .and_then(|s| s.get("output"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        store_card(ctx, event, &format!("{part_id}:out"), "tool_output", output, "", ts);
    }
}

/// Store one protocol-event card, deduped by `dedup_key`. A key already present
/// in `seen_parts` is a re-sent snapshot and is dropped.
fn store_card(
    ctx: &TurnLoopCtx,
    event: &OpenCodeEvent,
    dedup_key: &str,
    log_row: &str,
    text: &str,
    detail: &str,
    ts: DateTime<Utc>,
) {
    if let Ok(mut seen) = ctx.seen_parts.lock() {
        if !seen.insert(dedup_key.to_string()) {
            return;
        }
    }
    let payload = serde_json::json!({
        "type": "protocol_event",
        "stream": "sse",
        "event": event.event,
        "log_row": log_row,
        "text": text,
        "detail": detail,
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

    #[test]
    fn permission_event_for_current_session_returns_permission_id() {
        let event = OpenCodeEvent {
            event: Some("permission.updated".to_string()),
            data: r#"{"type":"permission.updated","properties":{"sessionID":"s1","permissionID":"perm-1"}}"#.to_string(),
        };
        assert_eq!(
            permission_request_id(&event, "s1").as_deref(),
            Some("perm-1")
        );
        assert_eq!(permission_request_id(&event, "other"), None);
    }

    #[derive(Default)]
    struct RecordingStore {
        events: Mutex<Vec<String>>,
    }
    impl cap_runner::RunnerEventStore for RecordingStore {
        fn insert_event(
            &self,
            _run_id: Option<&str>,
            _issue_identifier: &str,
            _kind: &'static str,
            payload: &str,
            _ts: DateTime<Utc>,
        ) {
            self.events.lock().unwrap().push(payload.to_string());
        }
    }

    #[derive(Default)]
    struct RecordingSink {
        lines: Mutex<Vec<String>>,
    }
    impl cap_runner::RunnerEventSink for RecordingSink {
        fn push(&self, line: String) {
            self.lines.lock().unwrap().push(line);
        }
    }

    fn recording_ctx() -> (TurnLoopCtx, Arc<RecordingSink>, Arc<RecordingStore>) {
        let sink = Arc::new(RecordingSink::default());
        let store = Arc::new(RecordingStore::default());
        let ctx = TurnLoopCtx {
            issue_id: "ISSUE-1".to_string(),
            run_id: "run-1".to_string(),
            prompt: String::new(),
            model: None,
            events: Arc::clone(&sink) as Arc<dyn cap_runner::RunnerEventSink>,
            store: Arc::clone(&store) as Arc<dyn cap_runner::RunnerEventStore>,
            last_event_at: Arc::new(Mutex::new(Utc::now())),
            seen_parts: Mutex::new(HashSet::new()),
        };
        (ctx, sink, store)
    }

    fn sse(data: &str) -> OpenCodeEvent {
        OpenCodeEvent { event: Some("message.part.updated".to_string()), data: data.to_string() }
    }

    fn stored_rows(store: &RecordingStore) -> Vec<serde_json::Value> {
        store
            .events
            .lock()
            .unwrap()
            .iter()
            .map(|p| serde_json::from_str(p).unwrap())
            .collect()
    }

    #[test]
    fn emit_streaming_text_part_pushes_live_log_but_stores_nothing() {
        let (ctx, sink, store) = recording_ctx();
        let ev = sse(r#"{"type":"message.part.updated","properties":{"sessionID":"s1","part":{"id":"p1","type":"text","text":"par","time":{"start":1}}}}"#);
        emit_event(&ctx, &ev);
        assert_eq!(sink.lines.lock().unwrap().len(), 1);
        assert_eq!(store.events.lock().unwrap().len(), 0);
    }

    #[test]
    fn emit_final_text_part_stores_assistant_once() {
        let (ctx, _sink, store) = recording_ctx();
        let ev = sse(r#"{"type":"message.part.updated","properties":{"sessionID":"s1","part":{"id":"p1","type":"text","text":"all done","time":{"start":1,"end":2}}}}"#);
        emit_event(&ctx, &ev);
        emit_event(&ctx, &ev);
        let rows = stored_rows(&store);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["log_row"], "assistant");
        assert_eq!(rows[0]["text"], "all done");
    }

    #[test]
    fn emit_completed_tool_stores_call_and_output() {
        let (ctx, _sink, store) = recording_ctx();
        let ev = sse(r#"{"type":"message.part.updated","properties":{"sessionID":"s1","part":{"id":"p2","type":"tool","tool":"bash","state":{"status":"completed","input":{"command":"ls"},"output":"file1"}}}}"#);
        emit_event(&ctx, &ev);
        let rows = stored_rows(&store);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["log_row"], "tool_call");
        assert_eq!(rows[0]["text"], "bash");
        assert!(rows[0]["detail"].as_str().unwrap_or("").contains("ls"));
        assert_eq!(rows[1]["log_row"], "tool_output");
        assert_eq!(rows[1]["text"], "file1");
        // emit again — still 2 rows (dedup)
        emit_event(&ctx, &ev);
        assert_eq!(stored_rows(&store).len(), 2);
    }

    #[test]
    fn emit_tool_running_stores_nothing() {
        let (ctx, sink, store) = recording_ctx();
        let ev = sse(r#"{"type":"message.part.updated","properties":{"sessionID":"s1","part":{"id":"p3","type":"tool","tool":"bash","state":{"status":"running","input":{"command":"ls"}}}}}"#);
        emit_event(&ctx, &ev);
        assert_eq!(store.events.lock().unwrap().len(), 0);
        assert_eq!(sink.lines.lock().unwrap().len(), 1);
    }

    #[test]
    fn emit_reasoning_final_stores_thinking() {
        let (ctx, _sink, store) = recording_ctx();
        let ev = sse(r#"{"type":"message.part.updated","properties":{"sessionID":"s1","part":{"id":"p4","type":"reasoning","text":"ponder","time":{"start":1,"end":2}}}}"#);
        emit_event(&ctx, &ev);
        let rows = stored_rows(&store);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["log_row"], "thinking");
        assert_eq!(rows[0]["text"], "ponder");
    }

    #[test]
    fn emit_noise_event_pushes_live_log_but_stores_nothing() {
        let (ctx, sink, store) = recording_ctx();
        let ev = OpenCodeEvent {
            event: Some("session.idle".to_string()),
            data: r#"{"type":"session.idle","properties":{"sessionID":"s1"}}"#.to_string(),
        };
        emit_event(&ctx, &ev);
        assert_eq!(store.events.lock().unwrap().len(), 0);
        assert_eq!(sink.lines.lock().unwrap().len(), 1);
    }

    #[test]
    fn session_dir_is_per_issue_under_agent_root() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let p = params(None, workspace, workspace_root);
        assert_eq!(
            session_dir(&p),
            PathBuf::from("/tmp/agent/opencode-sessions/ISSUE-1")
        );
    }

    #[test]
    fn opencode_args_use_headless_server_only() {
        assert_eq!(
            opencode_args(4179),
            vec![
                OsString::from("serve"),
                OsString::from("--hostname"),
                OsString::from("127.0.0.1"),
                OsString::from("--port"),
                OsString::from("4179"),
            ]
        );
    }

    #[test]
    fn opencode_env_points_storage_and_config_at_issue_dir() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let p = params(
            Some("anthropic/claude-sonnet".into()),
            workspace,
            workspace_root,
        );
        let session_dir = session_dir(&p);
        let model = effective_model(p.model.as_deref(), p.provider.as_deref());
        let env = opencode_env(&p, &session_dir, model.as_deref());
        assert!(env.contains(&(
            OsString::from("OPENCODE_CONFIG"),
            OsString::from("/tmp/agent/opencode-sessions/ISSUE-1/config/opencode.json")
        )));
        assert!(env.contains(&(
            OsString::from("OPENCODE_CONFIG_DIR"),
            OsString::from("/tmp/agent/opencode-sessions/ISSUE-1/config")
        )));
        let content = env
            .iter()
            .find(|(key, _)| key == "OPENCODE_CONFIG_CONTENT")
            .map(|(_, value)| value.to_string_lossy().to_string())
            .expect("OPENCODE_CONFIG_CONTENT missing");
        let config: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(config["permission"]["*"], "allow");
        assert_eq!(config["permission"]["bash"], "allow");
        assert_eq!(config["permission"]["external_directory"], "allow");
        assert_eq!(config["permission"]["edit"], "allow");
        assert_eq!(config["permission"]["question"], "allow");
        assert_eq!(config["permission"]["webfetch"], "allow");
        assert_eq!(config["model"], "anthropic/claude-sonnet");
        assert!(env.contains(&(
            OsString::from("XDG_DATA_HOME"),
            OsString::from("/tmp/agent/opencode-sessions/ISSUE-1/data")
        )));
        assert!(env.contains(&(
            OsString::from("AGENT_SESSION_DIR"),
            OsString::from("/tmp/agent/opencode-sessions/ISSUE-1")
        )));
        assert!(env.contains(&(
            OsString::from("AGENT_MODEL"),
            OsString::from("anthropic/claude-sonnet")
        )));
    }

    #[test]
    fn opencode_config_allows_permissions_and_uses_model_when_set() {
        let config = opencode_config(Some("anthropic/claude-sonnet"));
        assert_eq!(config["permission"]["*"], "allow");
        assert_eq!(config["permission"]["bash"], "allow");
        assert_eq!(config["permission"]["external_directory"], "allow");
        assert_eq!(config["permission"]["edit"], "allow");
        assert_eq!(config["permission"]["question"], "allow");
        assert_eq!(config["permission"]["webfetch"], "allow");
        assert_eq!(config["model"], "anthropic/claude-sonnet");
        assert!(opencode_config(None).get("model").is_none());
    }

    #[test]
    fn effective_model_folds_provider_in() {
        assert_eq!(
            effective_model(Some("minimax-m3"), Some("opencode-go")),
            Some("opencode-go/minimax-m3".to_string())
        );
    }

    #[test]
    fn effective_model_leaves_already_qualified_model_alone() {
        assert_eq!(
            effective_model(Some("anthropic/claude-sonnet"), Some("opencode-go")),
            Some("anthropic/claude-sonnet".to_string())
        );
    }

    #[test]
    fn effective_model_no_provider_returns_model_unchanged() {
        assert_eq!(
            effective_model(Some("minimax-m3"), None),
            Some("minimax-m3".to_string())
        );
    }

    #[test]
    fn effective_model_no_model_returns_none() {
        assert_eq!(effective_model(None, Some("x")), None);
    }

    fn write_script(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("fake-opencode.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn params<'a>(
        model: Option<String>,
        workspace: &'a Path,
        workspace_root: &'a Path,
    ) -> SpawnParams<'a> {
        SpawnParams::builder(
            "",
            "opencode",
            workspace,
            workspace_root,
            Path::new("/tmp/agent"),
            "prompt".to_string(),
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

    /// Retries up to 3 times on ETXTBSY (os error 26): under parallel `cargo
    /// test` a sibling fork may briefly hold the script's fd open.
    async fn spawn_against(dir: &Path, script: &Path) -> RunnerHandle {
        let workspaces = dir.join("workspaces");
        let workspace = workspaces.join("ISSUE-1");
        std::fs::create_dir_all(&workspace).unwrap();
        for attempt in 0u8..3 {
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
            match spawn_opencode(params).await {
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
                .is_some_and(|io| io.raw_os_error() == Some(26))
        })
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

    #[test]
    fn seed_opencode_auth_from_copies_auth_and_skips_missing_account() {
        let host_dir = tempfile::tempdir().unwrap();
        let session_dir = tempfile::tempdir().unwrap();

        // Write only auth.json into fake host opencode dir.
        let src = host_dir.path().join("opencode");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("auth.json"), r#"{"token":"fake"}"#).unwrap();
        // account.json intentionally absent.

        seed_opencode_auth_from(&src, session_dir.path()).unwrap();

        let dst_auth = session_dir.path().join("data/opencode/auth.json");
        assert!(dst_auth.exists(), "auth.json should be copied");
        let contents = std::fs::read_to_string(&dst_auth).unwrap();
        assert!(contents.contains("fake"), "auth.json contents should match");

        let dst_account = session_dir.path().join("data/opencode/account.json");
        assert!(!dst_account.exists(), "account.json should not be created when source is absent");
    }

    #[test]
    fn seed_opencode_auth_from_copies_both_when_present() {
        let host_dir = tempfile::tempdir().unwrap();
        let session_dir = tempfile::tempdir().unwrap();

        let src = host_dir.path().join("opencode");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("auth.json"), r#"{"token":"a"}"#).unwrap();
        std::fs::write(src.join("account.json"), r#"{"id":"u1"}"#).unwrap();

        seed_opencode_auth_from(&src, session_dir.path()).unwrap();

        assert!(session_dir.path().join("data/opencode/auth.json").exists());
        assert!(session_dir.path().join("data/opencode/account.json").exists());
    }

    #[test]
    fn seed_opencode_auth_from_is_noop_when_source_empty() {
        let host_dir = tempfile::tempdir().unwrap();
        let session_dir = tempfile::tempdir().unwrap();

        // src dir exists but has no auth files.
        let src = host_dir.path().join("opencode");
        std::fs::create_dir_all(&src).unwrap();

        seed_opencode_auth_from(&src, session_dir.path()).unwrap();

        assert!(!session_dir.path().join("data/opencode").exists());
    }

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
