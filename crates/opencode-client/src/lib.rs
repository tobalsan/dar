use std::ffi::OsString;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::Client;
use serde::Deserialize;
use tokio::process::{Child, Command};

const HEALTH_PATH: &str = "/global/health";
const EVENT_PATH: &str = "/global/event";
const DISPOSE_PATH: &str = "/instance/dispose";

#[derive(Debug)]
pub struct OpenCodeServer {
    base_url: String,
    child: Child,
}

#[derive(Debug, Clone)]
pub struct OpenCodeClient {
    base_url: String,
    client: Client,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenCodeEvent {
    pub event: Option<String>,
    pub data: String,
}

pub struct EventStream {
    stream: futures_util::stream::BoxStream<'static, reqwest::Result<Bytes>>,
    buffer: String,
}

#[derive(Debug, Deserialize)]
struct Session {
    id: String,
}

impl OpenCodeServer {
    pub async fn spawn(
        command: impl Into<OsString>,
        args: impl IntoIterator<Item = OsString>,
        env: impl IntoIterator<Item = (OsString, OsString)>,
        cwd: &Path,
    ) -> Result<Self> {
        let port = reserve_local_port()?;
        let base_url = format!("http://127.0.0.1:{port}");
        let port_arg = OsString::from(port.to_string());
        let args = args
            .into_iter()
            .map(|arg| if arg == "0" { port_arg.clone() } else { arg })
            .collect::<Vec<_>>();
        let mut cmd = Command::new(command.into());
        runner_core::scrub_loaded_env(&mut cmd);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd.envs(env);
        runner_core::setup_process_group(&mut cmd);
        let child = cmd.spawn().context("spawning opencode serve")?;
        let mut server = Self { base_url, child };
        let client = server.client();
        let health = client.wait_for_health(Duration::from_secs(20));
        tokio::pin!(health);
        tokio::select! {
            result = &mut health => {
                if let Err(e) = result {
                    server.kill_and_wait(Duration::from_secs(1)).await;
                    return Err(e);
                }
            }
            status = server.child.wait() => {
                let status = status.context("waiting for opencode serve during health check")?;
                return Err(anyhow!("opencode server exited before becoming healthy: {status}"));
            }
        }
        Ok(server)
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    pub fn client(&self) -> OpenCodeClient {
        OpenCodeClient::new(self.base_url.clone())
    }

    pub async fn wait(&mut self) -> Result<std::process::ExitStatus> {
        self.child
            .wait()
            .await
            .context("waiting for opencode serve")
    }

    pub async fn dispose_then_wait(&mut self, grace: Duration) -> Result<std::process::ExitStatus> {
        self.client().dispose().await?;
        let Some(pid) = self.pid() else {
            return Err(anyhow!("opencode serve has no pid"));
        };
        let wait = self.child.wait();
        tokio::pin!(wait);
        tokio::select! {
            status = &mut wait => status.context("waiting for opencode serve after dispose"),
            _ = tokio::time::sleep(grace) => {
                runner_core::term_then_kill(pid, Duration::from_secs(2));
                (&mut wait)
                    .await
                    .context("waiting for opencode serve after forced shutdown")
            }
        }
    }

    pub async fn kill_and_wait(&mut self, grace: Duration) {
        if let Some(pid) = self.pid() {
            runner_core::term_then_kill(pid, grace);
        }
        let _ = self.child.wait().await;
    }
}

impl OpenCodeClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: Client::new(),
        }
    }

    pub async fn wait_for_health(&self, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match self.client.get(self.url(HEALTH_PATH)).send().await {
                Ok(resp) if resp.status().is_success() => return Ok(()),
                _ if tokio::time::Instant::now() >= deadline => {
                    return Err(anyhow!("opencode server did not become healthy"));
                }
                _ => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    }

    pub async fn create_session(&self, title: &str) -> Result<String> {
        let session = self
            .client
            .post(self.url("/session"))
            .json(&serde_json::json!({ "title": title }))
            .send()
            .await
            .context("creating opencode session")?
            .error_for_status()
            .context("creating opencode session")?
            .json::<Session>()
            .await
            .context("decoding opencode session")?;
        Ok(session.id)
    }

    pub async fn send_prompt(
        &self,
        session_id: &str,
        prompt: &str,
        model: Option<&str>,
    ) -> Result<()> {
        self.send_prompt_with_system(session_id, prompt, model, None)
            .await
    }

    pub async fn send_prompt_with_system(
        &self,
        session_id: &str,
        prompt: &str,
        model: Option<&str>,
        system_prompt: Option<&str>,
    ) -> Result<()> {
        let mut body = serde_json::json!({
            "parts": [{ "type": "text", "text": prompt }]
        });
        if let Some(model) = model.and_then(parse_model) {
            body["model"] = model;
        }
        if let Some(system_prompt) = system_prompt.filter(|prompt| !prompt.is_empty()) {
            body["system"] = serde_json::Value::String(system_prompt.to_string());
        }
        self.client
            .post(self.url(&format!("/session/{session_id}/prompt_async")))
            .json(&body)
            .send()
            .await
            .context("sending opencode prompt")?
            .error_for_status()
            .context("sending opencode prompt")?;
        Ok(())
    }

    pub async fn events(&self) -> Result<EventStream> {
        let response = self
            .client
            .get(self.url(EVENT_PATH))
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .send()
            .await
            .context("opening opencode event stream")?
            .error_for_status()
            .context("opening opencode event stream")?;
        Ok(EventStream {
            stream: response.bytes_stream().boxed(),
            buffer: String::new(),
        })
    }

    pub async fn dispose(&self) -> Result<()> {
        self.client
            .post(self.url(DISPOSE_PATH))
            .send()
            .await
            .context("disposing opencode server")?
            .error_for_status()
            .context("disposing opencode server")?;
        Ok(())
    }

    pub async fn abort_session(&self, session_id: &str) -> Result<()> {
        self.client
            .post(self.url(&format!("/session/{session_id}/abort")))
            .send()
            .await
            .context("aborting opencode session")?
            .error_for_status()
            .context("aborting opencode session")?;
        Ok(())
    }

    pub async fn respond_permission(
        &self,
        session_id: &str,
        permission_id: &str,
        response: &str,
        remember: bool,
    ) -> Result<()> {
        self.client
            .post(self.url(&format!(
                "/session/{session_id}/permissions/{permission_id}"
            )))
            .json(&serde_json::json!({ "response": response, "remember": remember }))
            .send()
            .await
            .context("responding to opencode permission request")?
            .error_for_status()
            .context("responding to opencode permission request")?;
        Ok(())
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }
}

fn parse_model(model: &str) -> Option<serde_json::Value> {
    let (provider, model_id) = model.split_once('/')?;
    Some(serde_json::json!({
        "providerID": provider,
        "modelID": model_id,
    }))
}

impl EventStream {
    pub async fn next_event(&mut self) -> Result<Option<OpenCodeEvent>> {
        loop {
            if let Some((raw, rest)) = split_sse_frame(&self.buffer) {
                self.buffer = rest;
                if let Some(event) = parse_sse_frame(&raw) {
                    return Ok(Some(event));
                }
            }
            let Some(chunk) = self.stream.next().await else {
                if self.buffer.trim().is_empty() {
                    return Ok(None);
                }
                let raw = std::mem::take(&mut self.buffer);
                return Ok(parse_sse_frame(&raw));
            };
            let chunk = chunk.context("reading opencode event stream")?;
            self.buffer.push_str(&String::from_utf8_lossy(&chunk));
        }
    }
}

fn reserve_local_port() -> Result<u16> {
    let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .context("reserving local opencode port")?;
    Ok(listener.local_addr()?.port())
}

fn split_sse_frame(buffer: &str) -> Option<(String, String)> {
    if let Some(pos) = buffer.find("\n\n") {
        return Some((buffer[..pos].to_string(), buffer[pos + 2..].to_string()));
    }
    if let Some(pos) = buffer.find("\r\n\r\n") {
        return Some((buffer[..pos].to_string(), buffer[pos + 4..].to_string()));
    }
    None
}

fn parse_sse_frame(raw: &str) -> Option<OpenCodeEvent> {
    let mut event = None;
    let mut data = Vec::new();
    for line in raw.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(value) = line.strip_prefix("event:") {
            event = Some(value.trim_start().to_string());
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.trim_start().to_string());
        }
    }
    if data.is_empty() {
        return None;
    }
    Some(OpenCodeEvent {
        event,
        data: data.join("\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::Path;
    use axum::response::sse::{Event, Sse};
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use futures_util::stream;
    use serde_json::Value;
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::TcpListener as TokioTcpListener;

    #[test]
    fn parses_sse_event_and_data() {
        let event = parse_sse_frame("event: session.idle\ndata: {\"ok\":true}\n").unwrap();
        assert_eq!(event.event.as_deref(), Some("session.idle"));
        assert_eq!(event.data, r#"{"ok":true}"#);
    }

    #[test]
    fn split_sse_frame_keeps_remainder() {
        let (frame, rest) = split_sse_frame("data: one\n\ndata: two").unwrap();
        assert_eq!(frame, "data: one");
        assert_eq!(rest, "data: two");
    }

    #[test]
    fn parses_provider_model_string_for_prompt_body() {
        assert_eq!(
            parse_model("anthropic/claude-sonnet").unwrap(),
            serde_json::json!({
                "providerID": "anthropic",
                "modelID": "claude-sonnet",
            })
        );
        assert!(parse_model("claude-sonnet").is_none());
    }

    #[test]
    fn malformed_sse_lines_are_ignored_without_dropping_data() {
        let event = parse_sse_frame("junk\nevent: session.idle\ndata: {\"ok\":true}\n").unwrap();
        assert_eq!(event.event.as_deref(), Some("session.idle"));
        assert_eq!(event.data, r#"{"ok":true}"#);
    }

    #[tokio::test]
    async fn client_creates_session_sends_prompt_and_reads_events() {
        async fn health() -> Json<Value> {
            Json(serde_json::json!({ "healthy": true, "version": "test" }))
        }
        async fn session() -> Json<Value> {
            Json(serde_json::json!({ "id": "sess-1" }))
        }
        async fn prompt(Json(body): Json<Value>) -> Json<Value> {
            assert_eq!(body["parts"][0]["text"], "hello");
            assert!(body.get("system").is_none());
            Json(serde_json::json!({ "ok": true }))
        }
        async fn events(
        ) -> Sse<impl futures_util::Stream<Item = Result<Event, std::convert::Infallible>>>
        {
            Sse::new(stream::iter([Ok(Event::default()
                .event("session.idle")
                .data(
                    r#"{"type":"session.idle","properties":{"sessionID":"sess-1"}}"#,
                ))]))
        }

        let app = Router::new()
            .route("/global/health", get(health))
            .route("/session", post(session))
            .route("/session/sess-1/prompt_async", post(prompt))
            .route("/global/event", get(events));
        let listener = TokioTcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = OpenCodeClient::new(format!("http://{addr}"));
        client
            .wait_for_health(Duration::from_secs(1))
            .await
            .unwrap();
        let session = client.create_session("ISSUE-1").await.unwrap();
        assert_eq!(session, "sess-1");
        client.send_prompt(&session, "hello", None).await.unwrap();
        client
            .send_prompt_with_system(&session, "hello", None, Some(""))
            .await
            .unwrap();
        let mut events = client.events().await.unwrap();
        let event = events.next_event().await.unwrap().unwrap();
        assert_eq!(event.event.as_deref(), Some("session.idle"));
    }

    #[tokio::test]
    async fn client_aborts_and_responds_to_permission_requests() {
        async fn abort(Path(session): Path<String>) -> Json<Value> {
            assert_eq!(session, "sess-1");
            Json(serde_json::json!(true))
        }
        async fn permission(
            Path((session, permission)): Path<(String, String)>,
            Json(body): Json<Value>,
        ) -> impl IntoResponse {
            assert_eq!(session, "sess-1");
            assert_eq!(permission, "perm-1");
            assert_eq!(body["response"], "once");
            assert_eq!(body["remember"], false);
            Json(serde_json::json!(true))
        }

        let app = Router::new()
            .route("/session/:session/abort", post(abort))
            .route(
                "/session/:session/permissions/:permission",
                post(permission),
            );
        let listener = TokioTcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = OpenCodeClient::new(format!("http://{addr}"));
        client.abort_session("sess-1").await.unwrap();
        client
            .respond_permission("sess-1", "perm-1", "once", false)
            .await
            .unwrap();
    }

    fn write_script(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        let path = dir.join("fake-opencode.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    const CLEAN_SERVER: &str = r#"PORT=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --port) shift; PORT="$1" ;;
  esac
  shift
done
python3 - "$PORT" <<'PY'
import json, sys, threading
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
        self.send_error(404)
    def do_POST(self):
        if self.path == "/instance/dispose":
            self._json(True)
            threading.Thread(target=self.server.shutdown, daemon=True).start()
            return
        self.send_error(404)

ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()
PY"#;

    #[tokio::test]
    async fn server_spawn_health_and_clean_shutdown_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), CLEAN_SERVER);
        let mut server = OpenCodeServer::spawn(
            script.as_os_str().to_os_string(),
            [
                OsString::from("serve"),
                OsString::from("--hostname"),
                OsString::from("127.0.0.1"),
                OsString::from("--port"),
                OsString::from("0"),
            ],
            [],
            dir.path(),
        )
        .await
        .unwrap();
        assert!(server.pid().is_some());
        assert!(server
            .dispose_then_wait(Duration::from_secs(1))
            .await
            .unwrap()
            .success());
    }

    #[tokio::test]
    async fn server_spawn_reports_abnormal_health_failure() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), "exit 42");
        let err = OpenCodeServer::spawn(
            script.as_os_str().to_os_string(),
            [
                OsString::from("serve"),
                OsString::from("--hostname"),
                OsString::from("127.0.0.1"),
                OsString::from("--port"),
                OsString::from("0"),
            ],
            [],
            dir.path(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("exited"), "{err:#}");
    }
}
