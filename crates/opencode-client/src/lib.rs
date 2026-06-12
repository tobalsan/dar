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
    pub async fn spawn(command: impl Into<OsString>, cwd: &Path) -> Result<Self> {
        let port = reserve_local_port()?;
        let base_url = format!("http://127.0.0.1:{port}");
        let mut cmd = Command::new(command.into());
        cmd.arg("serve")
            .arg("--hostname")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(port.to_string())
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        runner_core::setup_process_group(&mut cmd);
        let child = cmd.spawn().context("spawning opencode serve")?;
        let mut server = Self { base_url, child };
        if let Err(e) = server
            .client()
            .wait_for_health(Duration::from_secs(20))
            .await
        {
            server.kill_and_wait(Duration::from_secs(1)).await;
            return Err(e);
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
        let mut body = serde_json::json!({
            "parts": [{ "type": "text", "text": prompt }]
        });
        if let Some(model) = model.and_then(parse_model) {
            body["model"] = model;
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
    use axum::response::sse::{Event, Sse};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use futures_util::stream;
    use serde_json::Value;
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
        let mut events = client.events().await.unwrap();
        let event = events.next_event().await.unwrap().unwrap();
        assert_eq!(event.event.as_deref(), Some("session.idle"));
    }
}
