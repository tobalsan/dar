//! OpenCode chat backend — one `opencode serve` child per TUI chat session.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use cap_chat::{ArtifactReady, ChatBackend, ChatEvent, ChatRole, ChatSession, ChatSessionParams};
use host_api::{Extension, RegisterCtx};
use opencode_client::{OpenCodeEvent, OpenCodeServer};
use runner_core::{effective_command, opencode_config, write_opencode_config};
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;

const CLOSE_GRACE: Duration = Duration::from_secs(5);

pub struct ChatOpenCodeExtension;

impl Extension for ChatOpenCodeExtension {
    fn id(&self) -> &'static str {
        "chat-opencode"
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            ctx.services
                .service::<dyn ChatBackend>("opencode", Arc::new(OpenCodeChatBackend))?;
            Ok(())
        })
    }
}

pub struct OpenCodeChatBackend;

impl ChatBackend for OpenCodeChatBackend {
    fn open<'a>(
        &'a self,
        params: ChatSessionParams,
        tx: Sender<ChatEvent>,
    ) -> cap_chat::BoxFuture<'a, Result<Box<dyn ChatSession>>> {
        Box::pin(async move {
            let session = OpenCodeChatSession::spawn(&params, tx).await?;
            Ok(Box::new(session) as Box<dyn ChatSession>)
        })
    }
}

pub struct OpenCodeChatSession {
    client: opencode_client::OpenCodeClient,
    session_id: String,
    model: Option<String>,
    server: Option<OpenCodeServer>,
    pump: JoinHandle<()>,
}

impl OpenCodeChatSession {
    async fn spawn(params: &ChatSessionParams, tx: Sender<ChatEvent>) -> Result<Self> {
        let dir = session_dir(params);
        let model = effective_model(params.model.as_deref(), params.provider.as_deref());
        std::fs::create_dir_all(dir.join("config"))
            .with_context(|| format!("creating opencode chat dir {}", dir.display()))?;
        seed_opencode_auth(&dir)?;
        let bridge = params.host_tool_bridge.as_ref();
        write_opencode_config(&dir, model.as_deref(), bridge)?;

        let mut server = OpenCodeServer::spawn(
            effective_command(&params.command, "opencode"),
            opencode_args(0),
            opencode_env(&dir, model.as_deref(), bridge),
            &params.agent_root,
        )
        .await?;
        let client = server.client();
        let session_id = match client.create_session("tui-chat").await {
            Ok(id) => id,
            Err(e) => {
                server.kill_and_wait(CLOSE_GRACE).await;
                return Err(e);
            }
        };
        let mut events = match client.events().await {
            Ok(events) => events,
            Err(e) => {
                server.kill_and_wait(CLOSE_GRACE).await;
                return Err(e);
            }
        };
        let pump_client = client.clone();
        let pump_session = session_id.clone();
        let artifact_ready = params.artifact_ready.clone();
        let pump = tokio::spawn(async move {
            loop {
                match events.next_event().await {
                    Ok(Some(event)) => {
                        if let Some(permission_id) = permission_request_id(&event, &pump_session) {
                            let _ = pump_client
                                .respond_permission(&pump_session, &permission_id, "once", false)
                                .await;
                            continue;
                        }
                        if let (Some(sink), Some(ready)) =
                            (&artifact_ready, artifact_ready_from_opencode(&event))
                        {
                            let _ = sink.send(ready).await;
                        }
                        if let Some(chat_event) = map_event(&event) {
                            if tx.send(chat_event).await.is_err() {
                                return;
                            }
                        }
                    }
                    Ok(None) => return,
                    Err(e) => {
                        let _ = tx
                            .send(ChatEvent::SessionClosed {
                                error: Some(e.to_string()),
                            })
                            .await;
                        return;
                    }
                }
            }
        });

        Ok(Self {
            client,
            session_id,
            model: model.clone(),
            server: Some(server),
            pump,
        })
    }
}

impl ChatSession for OpenCodeChatSession {
    fn send_turn(&mut self, prompt: String) -> cap_chat::BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.client
                .send_prompt(&self.session_id, &prompt, self.model.as_deref())
                .await
        })
    }

    fn abort(&mut self) -> cap_chat::BoxFuture<'_, Result<()>> {
        Box::pin(async move { self.client.abort_session(&self.session_id).await })
    }

    fn close(self: Box<Self>) -> cap_chat::BoxFuture<'static, Result<()>> {
        let mut this = *self;
        Box::pin(async move {
            this.pump.abort();
            if let Some(mut server) = this.server.take() {
                if let Err(e) = server.dispose_then_wait(CLOSE_GRACE).await {
                    server.kill_and_wait(CLOSE_GRACE).await;
                    return Err(e);
                }
            }
            Ok(())
        })
    }
}

fn session_dir(params: &ChatSessionParams) -> PathBuf {
    params.session_dir.join("opencode")
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

/// opencode reads credentials from `$XDG_DATA_HOME/opencode`, defaulting to
/// `~/.local/share/opencode`. The chat backend isolates XDG_DATA_HOME per session,
/// so we resolve the *host* location here to copy global auth into the sandbox.
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
    session_dir: &Path,
    model: Option<&str>,
    bridge: Option<&cap_chat::HostToolBridge>,
) -> Vec<(OsString, OsString)> {
    let config_dir = session_dir.join("config");
    vec![
        (
            OsString::from("AGENT_SESSION_DIR"),
            session_dir.as_os_str().to_os_string(),
        ),
        (
            OsString::from("OPENCODE_CONFIG"),
            config_dir.join("opencode.json").as_os_str().to_os_string(),
        ),
        (
            OsString::from("OPENCODE_CONFIG_DIR"),
            config_dir.as_os_str().to_os_string(),
        ),
        (
            OsString::from("OPENCODE_CONFIG_CONTENT"),
            OsString::from(opencode_config(model, bridge).to_string()),
        ),
        (
            OsString::from("XDG_DATA_HOME"),
            session_dir.join("data").as_os_str().to_os_string(),
        ),
        (
            OsString::from("XDG_STATE_HOME"),
            session_dir.join("state").as_os_str().to_os_string(),
        ),
        (
            OsString::from("XDG_CACHE_HOME"),
            session_dir.join("cache").as_os_str().to_os_string(),
        ),
    ]
}

fn artifact_ready_from_opencode(event: &OpenCodeEvent) -> Option<ArtifactReady> {
    let value = event_payload(event)?;
    if value.get("type")?.as_str()? != "message.part.updated" {
        return None;
    }
    let part = value.get("properties")?.get("part")?;
    let name = part.get("tool").or_else(|| part.get("name"))?.as_str()?;
    let state = part.get("state").unwrap_or(part);
    if name != "artifact_publish" || state.get("error").is_some() {
        return None;
    }
    let output = state.get("output")?;
    let content = output.get("content")?.as_array()?;
    content
        .iter()
        .find_map(|resource| ArtifactReady::from_publish_resource("artifact_publish", resource))
}

fn map_event(event: &OpenCodeEvent) -> Option<ChatEvent> {
    let value = event_payload(event)?;
    match value.get("type").and_then(|v| v.as_str()) {
        Some("message.part.updated") => map_part(value.get("properties")?.get("part")?),
        Some("session.idle") => Some(ChatEvent::TurnFinished {
            ok: true,
            error: None,
        }),
        Some("message.error") | Some("session.error") => Some(ChatEvent::TurnFinished {
            ok: false,
            error: Some(
                value
                    .get("properties")
                    .and_then(|p| p.get("error"))
                    .and_then(|e| e.as_str())
                    .unwrap_or("opencode error")
                    .to_string(),
            ),
        }),
        _ => None,
    }
}

fn map_part(part: &serde_json::Value) -> Option<ChatEvent> {
    match part.get("type").and_then(|v| v.as_str()) {
        Some("text") => Some(ChatEvent::Delta {
            role: ChatRole::Assistant,
            text: part_text(part),
        }),
        Some("reasoning") => Some(ChatEvent::Delta {
            role: ChatRole::Thinking,
            text: part_text(part),
        }),
        Some("tool") => map_tool(part),
        _ => None,
    }
}

fn map_tool(part: &serde_json::Value) -> Option<ChatEvent> {
    let id = part
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let name = part
        .get("tool")
        .or_else(|| part.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let state = part.get("state").unwrap_or(part);
    if let Some(input) = state.get("input") {
        return Some(ChatEvent::ToolCall {
            id,
            name,
            args: render_value(input),
        });
    }
    if state.get("output").is_some() || state.get("error").is_some() {
        let is_error = state.get("error").is_some()
            || matches!(
                state.get("status").and_then(|v| v.as_str()),
                Some("error" | "failed")
            );
        let text = state
            .get("output")
            .or_else(|| state.get("error"))
            .map(result_text)
            .unwrap_or_default();
        return Some(ChatEvent::ToolOutput {
            id,
            text,
            is_error,
            done: matches!(
                state.get("status").and_then(|v| v.as_str()),
                Some("completed" | "error" | "failed")
            ),
        });
    }
    None
}

fn part_text(part: &serde_json::Value) -> String {
    part.get("text")
        .or_else(|| part.get("delta"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

fn result_text(value: &serde_json::Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    if let Some(items) = value.get("content").and_then(|v| v.as_array()) {
        return items
            .iter()
            .filter_map(|item| item.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
    }
    render_value(value)
}

fn render_value(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
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
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cap_chat::{ChatRole, ChatSessionParams};
    use opencode_client::OpenCodeEvent;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use tokio::sync::mpsc::Receiver;

    fn mapped_event(data: &str) -> Option<ChatEvent> {
        map_event(&OpenCodeEvent {
            event: None,
            data: data.to_string(),
        })
    }

    #[test]
    fn artifact_ready_requires_exact_publish_resource() {
        let event = OpenCodeEvent {
            event: None,
            data: r#"{"type":"message.part.updated","properties":{"part":{"tool":"artifact_publish","state":{"output":{"content":[{"type":"resource_link","uri":"dar-artifact://550e8400-e29b-41d4-a716-446655440000","name":"report.txt","bytes":5,"sha256":"abc"}]}}}}}"#.to_string(),
        };
        assert_eq!(
            artifact_ready_from_opencode(&event).unwrap().name,
            "report.txt"
        );
        assert!(artifact_ready_from_opencode(&OpenCodeEvent {
            event: None,
            data: "{}".to_string()
        })
        .is_none());
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
    fn effective_model_no_model_returns_none() {
        assert_eq!(effective_model(None, Some("x")), None);
    }

    #[test]
    fn seed_opencode_auth_from_copies_auth_and_skips_missing_account() {
        let host_dir = tempfile::tempdir().unwrap();
        let session_dir = tempfile::tempdir().unwrap();

        let src = host_dir.path().join("opencode");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("auth.json"), r#"{"token":"fake"}"#).unwrap();

        seed_opencode_auth_from(&src, session_dir.path()).unwrap();

        let dst_auth = session_dir.path().join("data/opencode/auth.json");
        assert!(dst_auth.exists(), "auth.json should be copied");
        let contents = std::fs::read_to_string(&dst_auth).unwrap();
        assert!(contents.contains("fake"), "auth.json contents should match");

        let dst_account = session_dir.path().join("data/opencode/account.json");
        assert!(
            !dst_account.exists(),
            "account.json should not be created when source is absent"
        );
    }

    #[test]
    fn maps_text_and_thinking_deltas() {
        match mapped_event(
            r#"{"payload":{"type":"message.part.updated","properties":{"part":{"type":"text","text":"hi"}}}}"#,
        )
        .unwrap()
        {
            ChatEvent::Delta { role, text } => {
                assert_eq!(role, ChatRole::Assistant);
                assert_eq!(text, "hi");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        match mapped_event(
            r#"{"payload":{"type":"message.part.updated","properties":{"part":{"type":"reasoning","text":"hmm"}}}}"#,
        )
        .unwrap()
        {
            ChatEvent::Delta { role, text } => {
                assert_eq!(role, ChatRole::Thinking);
                assert_eq!(text, "hmm");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn maps_tool_lifecycle() {
        match mapped_event(
            r#"{"payload":{"type":"message.part.updated","properties":{"part":{"id":"tool-1","type":"tool","tool":"bash","state":{"input":{"cmd":"pwd"}}}}}}"#,
        )
        .unwrap()
        {
            ChatEvent::ToolCall { id, name, args } => {
                assert_eq!(id, "tool-1");
                assert_eq!(name, "bash");
                assert_eq!(args, r#"{"cmd":"pwd"}"#);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        match mapped_event(
            r#"{"payload":{"type":"message.part.updated","properties":{"part":{"id":"tool-1","type":"tool","tool":"bash","state":{"output":"ok","status":"completed"}}}}}"#,
        )
        .unwrap()
        {
            ChatEvent::ToolOutput {
                id,
                text,
                is_error,
                done,
            } => {
                assert_eq!(id, "tool-1");
                assert_eq!(text, "ok");
                assert!(!is_error);
                assert!(done);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn maps_turn_finished_and_errors() {
        match mapped_event(r#"{"payload":{"type":"session.idle","properties":{"sessionID":"s1"}}}"#)
            .unwrap()
        {
            ChatEvent::TurnFinished { ok, error } => {
                assert!(ok);
                assert_eq!(error, None);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        match mapped_event(r#"{"payload":{"type":"message.error","properties":{"error":"boom"}}}"#)
            .unwrap()
        {
            ChatEvent::TurnFinished { ok, error } => {
                assert!(!ok);
                assert_eq!(error.as_deref(), Some("boom"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn registers_chat_backend_under_opencode() {
        let temp = tempfile::tempdir().unwrap();
        let paths = host_api::HostPaths::new(temp.path()).unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let mut ctx = host_api::RegisterCtx {
            bus: host_api::EventBus::new(),
            http: host_api::HttpRegistry::disabled(),
            foreground: host_api::ForegroundRegistry::default(),
            services: host_api::ServiceRegistry::default(),
            paths,
            config: host_api::ConfigStore::default(),
            shutdown: host_api::ShutdownToken::new(rx),
        };

        ChatOpenCodeExtension.register(&mut ctx).await.unwrap();
        assert!(ctx
            .services
            .get_named::<dyn ChatBackend>("opencode")
            .is_ok());
        assert!(ctx.services.get_named::<dyn ChatBackend>("pi").is_err());
    }

    #[test]
    fn opencode_session_params_use_tui_session_dir_as_root() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("data").join("tui").join("sessions");
        let params = ChatSessionParams::builder("", temp.path(), &sessions).build();
        assert_eq!(session_dir(&params), sessions.join("opencode"));
    }

    fn write_script(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("fake-opencode.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    async fn next_event(rx: &mut Receiver<ChatEvent>) -> ChatEvent {
        tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("timed out waiting for ChatEvent")
            .expect("chat event channel closed")
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
    def _empty(self):
        self.send_response(204)
        self.send_header("content-length", "0")
        self.end_headers()
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
            permission_sent = False
            while not dispose:
                if prompt_count and not permission_sent:
                    permission_sent = True
                    self.wfile.write(b'data: {"payload":{"type":"permission.updated","properties":{"sessionID":"sess-1","permissionID":"perm-1"}}}\n\n')
                    self.wfile.flush()
                while prompt_count > sent:
                    sent += 1
                    self.wfile.write(b'data: {"payload":{"type":"message.part.updated","properties":{"part":{"type":"reasoning","text":"hmm"}}}}\n\n')
                    self.wfile.write(b'data: {"payload":{"type":"message.part.updated","properties":{"part":{"type":"text","text":"pong"}}}}\n\n')
                    self.wfile.write(b'data: {"payload":{"type":"message.part.updated","properties":{"part":{"id":"tool-1","type":"tool","tool":"bash","state":{"input":{"cmd":"pwd"}}}}}}\n\n')
                    self.wfile.write(b'data: {"payload":{"type":"message.part.updated","properties":{"part":{"id":"tool-1","type":"tool","tool":"bash","state":{"output":"ok","status":"completed"}}}}}\n\n')
                    self.wfile.write(b'data: {"payload":{"type":"session.idle","properties":{"sessionID":"sess-1"}}}\n\n')
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
            return self._empty()
        if self.path == "/session/sess-1/abort":
            with open("abort.log", "a") as f:
                f.write("abort\n")
            return self._empty()
        if self.path == "/session/sess-1/permissions/perm-1":
            data = json.loads(body)
            assert data["response"] == "once"
            assert data["remember"] is False
            with open("permission.log", "a") as f:
                f.write(body + "\n")
            return self._empty()
        if self.path == "/instance/dispose":
            dispose = True
            self._json(True)
            threading.Thread(target=self.server.shutdown, daemon=True).start()
            return
        self.send_error(404)

ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()
PY"#;

    const DISPOSE_FAIL_SERVER: &str = r#"PORT=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --port) shift; PORT="$1" ;;
  esac
  shift
done
python3 - "$PORT" <<'PY'
import json, sys, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

port = int(sys.argv[1])

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
        if self.path == "/global/health":
            return self._json({"healthy": True, "version": "fake"})
        if self.path == "/global/event":
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("cache-control", "no-cache")
            self.end_headers()
            while True:
                time.sleep(0.05)
        self.send_error(404)
    def do_POST(self):
        length = int(self.headers.get("content-length", "0") or "0")
        self.rfile.read(length)
        if self.path == "/session":
            return self._json({"id": "sess-1", "directory": "."})
        if self.path == "/instance/dispose":
            body = b"dispose failed"
            self.send_response(500)
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        self.send_response(204)
        self.send_header("content-length", "0")
        self.end_headers()

ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()
PY"#;

    #[tokio::test(flavor = "multi_thread")]
    async fn session_sends_steering_aborts_permissions_and_closes_server() {
        let temp = tempfile::tempdir().unwrap();
        let script = write_script(temp.path(), FAKE_SERVER);
        let sessions = temp.path().join("data").join("tui").join("sessions");
        let params = ChatSessionParams::builder(script.to_str().unwrap(), temp.path(), &sessions)
            .model(Some("anthropic/claude-sonnet".to_string()))
            .build();
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let mut session = OpenCodeChatBackend.open(params, tx).await.unwrap();

        session.send_turn("first".to_string()).await.unwrap();
        session.send_turn("second".to_string()).await.unwrap();
        session.abort().await.unwrap();

        let mut finished = 0;
        while finished < 2 {
            match next_event(&mut rx).await {
                ChatEvent::Delta {
                    role: ChatRole::Thinking,
                    text,
                } => assert_eq!(text, "hmm"),
                ChatEvent::Delta {
                    role: ChatRole::Assistant,
                    text,
                } => assert_eq!(text, "pong"),
                ChatEvent::ToolCall { name, .. } => assert_eq!(name, "bash"),
                ChatEvent::ToolOutput { text, done, .. } => {
                    assert_eq!(text, "ok");
                    assert!(done);
                }
                ChatEvent::TurnFinished { ok, error } => {
                    assert!(ok, "{error:?}");
                    finished += 1;
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }

        ChatSession::close(session).await.unwrap();
        let received = std::fs::read_to_string(temp.path().join("received.log")).unwrap();
        assert!(received.contains("first"), "{received}");
        assert!(received.contains("second"), "{received}");
        assert!(received.contains("providerID"), "{received}");
        assert_eq!(
            std::fs::read_to_string(temp.path().join("abort.log")).unwrap(),
            "abort\n"
        );
        assert!(temp.path().join("permission.log").exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn close_kills_server_when_dispose_fails() {
        let temp = tempfile::tempdir().unwrap();
        let script = write_script(temp.path(), DISPOSE_FAIL_SERVER);
        let sessions = temp.path().join("data").join("tui").join("sessions");
        let params =
            ChatSessionParams::builder(script.to_str().unwrap(), temp.path(), &sessions).build();
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let session = OpenCodeChatSession::spawn(&params, tx).await.unwrap();
        let pid = session.server.as_ref().unwrap().pid().unwrap();

        assert!(ChatSession::close(Box::new(session)).await.is_err());
        assert_process_dead(pid).await;
    }

    async fn assert_process_dead(pid: u32) {
        let pgid = nix::unistd::Pid::from_raw(pid as i32);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if nix::sys::signal::kill(pgid, None).is_err() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "child {pid} still alive"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}
