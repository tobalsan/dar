//! OpenCode chat backend — one `opencode serve` child per TUI chat session.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use cap_chat::{
    ArtifactReady, ChatBackend, ChatEvent, ChatRole, ChatSession, ChatSessionParams, QuestionInfo,
};
use host_api::{Extension, RegisterCtx};
use opencode_client::{OpenCodeEvent, OpenCodeServer};
use runner_core::{effective_command, opencode_config, write_opencode_config};
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;

const CLOSE_GRACE: Duration = Duration::from_secs(5);
/// How long to wait for `opencode serve` to exit on its own after a dispose
/// before signalling it. `POST /instance/dispose` answers `true` but does not
/// terminate the server (opencode 1.18.x), so this window is always spent in
/// full — keep it short: SIGTERM stops the server promptly, and host shutdown
/// gives each extension only a few seconds to stop.
const DISPOSE_GRACE: Duration = Duration::from_secs(1);

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
    system_prompt: Option<String>,
    server: Option<OpenCodeServer>,
    pump: JoinHandle<()>,
    marker: Option<PathBuf>,
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
        let session_id = match resume_session(&client, params.resume_session_id.as_deref()).await {
            Some(id) => id,
            None => match client.create_session("tui-chat").await {
                Ok(id) => id,
                Err(e) => {
                    server.kill_and_wait(CLOSE_GRACE).await;
                    return Err(e);
                }
            },
        };
        // Marker in the SHARED session dir (params.session_dir, sibling to
        // pi's files) so the generic newest-wins resume resolution finds this
        // opencode session after a restart. Best-effort: a failed write only
        // costs resume, never the session.
        let marker = ensure_marker(&params.session_dir, &session_id);
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
            let mut user_messages: HashSet<String> = HashSet::new();
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
                        if let Some(chat_event) = question_event(&event, &pump_session) {
                            if tx.send(chat_event).await.is_err() {
                                return;
                            }
                            continue;
                        }
                        if let Some(chat_event) = map_event(&event, &mut user_messages) {
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
            system_prompt: params.system_prompt.clone(),
            server: Some(server),
            pump,
            marker,
        })
    }
}

/// Reuse `resume` when the server still knows it; any miss or error falls
/// back to fresh (None) — resume is always optional.
async fn resume_session(
    client: &opencode_client::OpenCodeClient,
    resume: Option<&str>,
) -> Option<String> {
    let id = resume?;
    matches!(client.session_exists(id).await, Ok(true)).then(|| id.to_string())
}

/// Find the existing marker whose header id matches, else write a new one:
/// `{millis:013}_{id}.jsonl`, first line
/// {"type":"session","id":"<id>","backend":"opencode"}. Returns the path.
fn ensure_marker(shared_dir: &Path, session_id: &str) -> Option<PathBuf> {
    std::fs::create_dir_all(shared_dir).ok()?;
    if let Ok(entries) = std::fs::read_dir(shared_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(contents) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some(first_line) = contents.lines().next() else {
                continue;
            };
            let Ok(header) = serde_json::from_str::<serde_json::Value>(first_line) else {
                continue;
            };
            if header.get("id").and_then(|v| v.as_str()) == Some(session_id)
                && header.get("backend").and_then(|v| v.as_str()) == Some("opencode")
            {
                return Some(path);
            }
        }
    }
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis();
    let path = shared_dir.join(format!("{millis:013}_{session_id}.jsonl"));
    let header = serde_json::json!({ "type": "session", "id": session_id, "backend": "opencode" });
    std::fs::write(&path, format!("{header}\n")).ok()?;
    Some(path)
}

/// Freshen the marker mtime so chat-web's mtime-based idle expiry counts
/// this session as active. (`File::set_modified`, MSRV 1.83-ok.)
fn touch_marker(marker: Option<&Path>) {
    if let Some(p) = marker {
        if let Ok(f) = std::fs::File::options().append(true).open(p) {
            let _ = f.set_modified(std::time::SystemTime::now());
        }
    }
}

impl ChatSession for OpenCodeChatSession {
    fn send_turn(&mut self, prompt: String) -> cap_chat::BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            touch_marker(self.marker.as_deref());
            self.client
                .send_prompt_with_system(
                    &self.session_id,
                    &prompt,
                    self.model.as_deref(),
                    self.system_prompt.as_deref(),
                )
                .await
        })
    }

    fn answer_question(
        &mut self,
        request_id: String,
        answers: Vec<Vec<String>>,
    ) -> cap_chat::BoxFuture<'_, Result<()>> {
        Box::pin(async move { self.client.reply_question(&request_id, &answers).await })
    }

    fn abort(&mut self) -> cap_chat::BoxFuture<'_, Result<()>> {
        Box::pin(async move { self.client.abort_session(&self.session_id).await })
    }

    fn close(self: Box<Self>) -> cap_chat::BoxFuture<'static, Result<()>> {
        let mut this = *self;
        Box::pin(async move {
            this.pump.abort();
            if let Some(mut server) = this.server.take() {
                if let Err(e) = server.dispose_then_wait(DISPOSE_GRACE).await {
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

fn map_event(event: &OpenCodeEvent, user_messages: &mut HashSet<String>) -> Option<ChatEvent> {
    let value = event_payload(event)?;
    match value.get("type").and_then(|v| v.as_str()) {
        Some("message.updated") => {
            let info = value.get("properties")?.get("info")?;
            if info.get("role").and_then(|v| v.as_str()) == Some("user") {
                if let Some(id) = info.get("id").and_then(|v| v.as_str()) {
                    user_messages.insert(id.to_string());
                }
            }
            None
        }
        Some("message.part.updated") => {
            let part = value.get("properties")?.get("part")?;
            if let Some(message_id) = part.get("messageID").and_then(|v| v.as_str()) {
                if user_messages.contains(message_id) {
                    return None;
                }
            }
            map_part(part)
        }
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
    if name == "question" {
        return None;
    }
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

fn question_event(event: &OpenCodeEvent, session_id: &str) -> Option<ChatEvent> {
    let value = event_payload(event)?;
    let type_name = value.get("type").and_then(|v| v.as_str())?;
    if !type_name.starts_with("question.") {
        return None;
    }
    let properties = value.get("properties").unwrap_or(&value);
    let event_session = properties
        .get("sessionID")
        .or_else(|| properties.get("sessionId"))
        .and_then(|v| v.as_str());
    if event_session.is_some() && event_session != Some(session_id) {
        return None;
    }
    let request_id = properties
        .get("requestID")
        .or_else(|| properties.get("id"))
        .and_then(|v| v.as_str())?
        .to_string();
    match type_name {
        "question.asked" | "question.v2.asked" => {
            match serde_json::from_value::<Vec<QuestionInfo>>(properties.get("questions")?.clone())
            {
                Ok(questions) if !questions.is_empty() => Some(ChatEvent::QuestionAsked {
                    request_id,
                    questions,
                }),
                _ => Some(ChatEvent::Error(format!(
                    "question {request_id} could not be parsed; answer it in opencode's own UI"
                ))),
            }
        }
        "question.replied" | "question.v2.replied" => Some(ChatEvent::QuestionResolved {
            request_id,
            answers: serde_json::from_value(
                properties
                    .get("answers")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            )
            .unwrap_or_default(),
            rejected: false,
        }),
        "question.rejected" | "question.v2.rejected" => Some(ChatEvent::QuestionResolved {
            request_id,
            answers: vec![],
            rejected: true,
        }),
        _ => None,
    }
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
        let mut user_messages = std::collections::HashSet::new();
        map_event(
            &OpenCodeEvent {
                event: None,
                data: data.to_string(),
            },
            &mut user_messages,
        )
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
    fn question_asked_maps_to_chat_event() {
        let event = OpenCodeEvent {
            event: None,
            data: r#"{"payload":{"type":"question.asked","properties":{"id":"req-1","sessionID":"sess-1","questions":[{"header":"Pick","question":"Which one?","options":[{"label":"A","description":"first"},{"label":"B","description":"second"}]}]}}}"#.to_string(),
        };
        match question_event(&event, "sess-1").unwrap() {
            ChatEvent::QuestionAsked {
                request_id,
                questions,
            } => {
                assert_eq!(request_id, "req-1");
                assert_eq!(questions.len(), 1);
                let q = &questions[0];
                assert_eq!(q.header, "Pick");
                assert_eq!(q.question, "Which one?");
                assert!(!q.multiple);
                assert!(!q.custom);
                assert_eq!(q.options[0].label, "A");
                assert_eq!(q.options[0].description, "first");
                assert_eq!(q.options[1].label, "B");
                assert_eq!(q.options[1].description, "second");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn question_asked_for_other_session_is_none() {
        let event = OpenCodeEvent {
            event: None,
            data: r#"{"payload":{"type":"question.asked","properties":{"id":"req-1","sessionID":"other","questions":[{"header":"Pick","question":"Which?","options":[]}]}}}"#.to_string(),
        };
        assert!(question_event(&event, "sess-1").is_none());
    }

    #[test]
    fn question_asked_malformed_questions_maps_to_error() {
        let event = OpenCodeEvent {
            event: None,
            data: r#"{"payload":{"type":"question.asked","properties":{"id":"req-1","sessionID":"sess-1","questions":"nope"}}}"#.to_string(),
        };
        match question_event(&event, "sess-1").unwrap() {
            ChatEvent::Error(text) => assert!(text.contains("req-1"), "{text}"),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn question_replied_and_rejected_map_to_resolved() {
        let replied = OpenCodeEvent {
            event: None,
            data: r#"{"payload":{"type":"question.replied","properties":{"sessionID":"sess-1","requestID":"req-1","answers":[["A"]]}}}"#.to_string(),
        };
        match question_event(&replied, "sess-1").unwrap() {
            ChatEvent::QuestionResolved {
                request_id,
                answers,
                rejected,
            } => {
                assert_eq!(request_id, "req-1");
                assert_eq!(answers, vec![vec!["A".to_string()]]);
                assert!(!rejected);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let rejected = OpenCodeEvent {
            event: None,
            data: r#"{"payload":{"type":"question.rejected","properties":{"sessionID":"sess-1","requestID":"req-1"}}}"#.to_string(),
        };
        match question_event(&rejected, "sess-1").unwrap() {
            ChatEvent::QuestionResolved {
                request_id,
                answers,
                rejected,
            } => {
                assert_eq!(request_id, "req-1");
                assert!(answers.is_empty());
                assert!(rejected);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn question_v2_alias_still_maps() {
        let event = OpenCodeEvent {
            event: None,
            data: r#"{"payload":{"type":"question.v2.asked","properties":{"id":"req-1","sessionID":"sess-1","questions":[{"header":"Pick","question":"Which one?","options":[{"label":"A","description":"first"}]}]}}}"#.to_string(),
        };
        match question_event(&event, "sess-1").unwrap() {
            ChatEvent::QuestionAsked { request_id, .. } => assert_eq!(request_id, "req-1"),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn question_tool_part_is_suppressed() {
        assert!(mapped_event(
            r#"{"payload":{"type":"message.part.updated","properties":{"part":{"id":"q-1","type":"tool","tool":"question","state":{"input":{}}}}}}"#,
        )
        .is_none());
        assert!(mapped_event(
            r#"{"payload":{"type":"message.part.updated","properties":{"part":{"id":"q-1","type":"tool","tool":"question","state":{"output":"ignored","status":"completed"}}}}}"#,
        )
        .is_none());
    }

    #[test]
    fn ensure_marker_writes_header_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = ensure_marker(dir.path(), "sess-1").unwrap();
        let filename = path.file_name().unwrap().to_str().unwrap();
        assert!(filename.ends_with("_sess-1.jsonl"), "{filename}");
        let (millis, _) = filename.split_once('_').unwrap();
        assert_eq!(millis.len(), 13);

        let contents = std::fs::read_to_string(&path).unwrap();
        let header: serde_json::Value =
            serde_json::from_str(contents.lines().next().unwrap()).unwrap();
        assert_eq!(
            header,
            serde_json::json!({ "type": "session", "id": "sess-1", "backend": "opencode" })
        );

        let second = ensure_marker(dir.path(), "sess-1").unwrap();
        assert_eq!(second, path);
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

    #[test]
    fn suppresses_user_echo_but_still_maps_assistant_text() {
        let mut user_messages = std::collections::HashSet::new();

        let update = map_event(
            &OpenCodeEvent {
                event: None,
                data: r#"{"payload":{"type":"message.updated","properties":{"info":{"id":"msg_u","role":"user"}}}}"#.to_string(),
            },
            &mut user_messages,
        );
        assert!(update.is_none());

        let echo = map_event(
            &OpenCodeEvent {
                event: None,
                data: r#"{"payload":{"type":"message.part.updated","properties":{"part":{"type":"text","messageID":"msg_u","text":"echo"}}}}"#.to_string(),
            },
            &mut user_messages,
        );
        assert!(echo.is_none());

        match map_event(
            &OpenCodeEvent {
                event: None,
                data: r#"{"payload":{"type":"message.part.updated","properties":{"part":{"type":"text","messageID":"msg_a","text":"hi"}}}}"#.to_string(),
            },
            &mut user_messages,
        )
        .unwrap()
        {
            ChatEvent::Delta { role, text } => {
                assert_eq!(role, ChatRole::Assistant);
                assert_eq!(text, "hi");
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
question_replied = False

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
        if self.path == "/session/sess-1":
            return self._json({"id": "sess-1"})
        if self.path.startswith("/session/") and self.path.count("/") == 2:
            self.send_error(404)
            return
        if self.path == "/global/event":
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("cache-control", "no-cache")
            self.end_headers()
            sent = 0
            permission_sent = False
            replied_sent = False
            while not dispose:
                if prompt_count and not permission_sent:
                    permission_sent = True
                    self.wfile.write(b'data: {"payload":{"type":"permission.updated","properties":{"sessionID":"sess-1","permissionID":"perm-1"}}}\n\n')
                    self.wfile.flush()
                if question_replied and not replied_sent:
                    replied_sent = True
                    self.wfile.write(b'data: {"payload":{"type":"question.replied","properties":{"sessionID":"sess-1","requestID":"req-1","answers":[["A"]]}}}\n\n')
                    self.wfile.flush()
                while prompt_count > sent:
                    sent += 1
                    self.wfile.write(b'data: {"payload":{"type":"message.updated","properties":{"info":{"id":"msg-user-1","role":"user"}}}}\n\n')
                    self.wfile.write(b'data: {"payload":{"type":"message.part.updated","properties":{"part":{"type":"text","messageID":"msg-user-1","text":"ECHO"}}}}\n\n')
                    self.wfile.write(b'data: {"payload":{"type":"message.part.updated","properties":{"part":{"type":"reasoning","text":"hmm"}}}}\n\n')
                    self.wfile.write(b'data: {"payload":{"type":"message.part.updated","properties":{"part":{"type":"text","text":"pong"}}}}\n\n')
                    self.wfile.write(b'data: {"payload":{"type":"message.part.updated","properties":{"part":{"id":"tool-1","type":"tool","tool":"bash","state":{"input":{"cmd":"pwd"}}}}}}\n\n')
                    self.wfile.write(b'data: {"payload":{"type":"message.part.updated","properties":{"part":{"id":"tool-1","type":"tool","tool":"bash","state":{"output":"ok","status":"completed"}}}}}\n\n')
                    self.wfile.write(b'data: {"payload":{"type":"question.asked","properties":{"id":"req-1","sessionID":"sess-1","questions":[{"header":"Pick","question":"Which one?","options":[{"label":"A","description":"first"},{"label":"B","description":"second"}]}]}}}\n\n')
                    self.wfile.write(b'data: {"payload":{"type":"session.idle","properties":{"sessionID":"sess-1"}}}\n\n')
                    self.wfile.flush()
                time.sleep(0.02)
            return
        self.send_error(404)
    def do_POST(self):
        global prompt_count, dispose, question_replied
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
        if self.path == "/question/req-1/reply":
            data = json.loads(body)
            assert data["answers"] == [["A"]]
            with open("question.log", "a") as f:
                f.write(body + "\n")
            question_replied = True
            return self._json(True)
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
            .system_prompt(Some("exact identity context\n".to_string()))
            .build();
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let mut session = OpenCodeChatBackend.open(params, tx).await.unwrap();

        session.send_turn("first".to_string()).await.unwrap();
        session.send_turn("second".to_string()).await.unwrap();
        session.abort().await.unwrap();

        let mut finished = 0;
        let mut answered = false;
        let mut question_resolved = false;
        while finished < 2 || !question_resolved {
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
                ChatEvent::QuestionAsked {
                    request_id,
                    questions,
                } => {
                    assert_eq!(request_id, "req-1");
                    assert_eq!(questions.len(), 1);
                    assert_eq!(questions[0].options.len(), 2);
                    if !answered {
                        answered = true;
                        session
                            .answer_question(request_id, vec![vec!["A".to_string()]])
                            .await
                            .unwrap();
                    }
                }
                ChatEvent::QuestionResolved {
                    request_id,
                    answers,
                    rejected,
                } => {
                    assert_eq!(request_id, "req-1");
                    assert_eq!(answers, vec![vec!["A".to_string()]]);
                    assert!(!rejected);
                    question_resolved = true;
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
        for line in received.lines() {
            let body: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(body["system"], "exact identity context\n");
            assert!(!body["parts"][0]["text"]
                .as_str()
                .unwrap()
                .contains("identity context"));
        }
        assert_eq!(
            std::fs::read_to_string(temp.path().join("abort.log")).unwrap(),
            "abort\n"
        );
        assert!(temp.path().join("permission.log").exists());
        assert!(temp.path().join("question.log").exists());
        let marker_count = std::fs::read_dir(&sessions)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .and_then(|e| e.to_str())
                    == Some("jsonl")
            })
            .count();
        assert_eq!(marker_count, 1, "expected exactly one opencode marker file");
        let marker_path = std::fs::read_dir(&sessions)
            .unwrap()
            .find_map(|e| {
                let path = e.unwrap().path();
                (path.extension().and_then(|e| e.to_str()) == Some("jsonl")).then_some(path)
            })
            .unwrap();
        let header: serde_json::Value = serde_json::from_str(
            std::fs::read_to_string(&marker_path)
                .unwrap()
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(header["backend"], "opencode");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resume_reuses_existing_session_and_touches_marker() {
        let temp = tempfile::tempdir().unwrap();
        let script = write_script(temp.path(), FAKE_SERVER);
        let sessions = temp.path().join("data").join("tui").join("sessions");

        let params =
            ChatSessionParams::builder(script.to_str().unwrap(), temp.path(), &sessions).build();
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let session = OpenCodeChatSession::spawn(&params, tx).await.unwrap();
        let marker = session.marker.clone().expect("marker written on spawn");
        ChatSession::close(Box::new(session)).await.unwrap();
        let mtime_before = std::fs::metadata(&marker).unwrap().modified().unwrap();

        let params = ChatSessionParams::builder(script.to_str().unwrap(), temp.path(), &sessions)
            .resume_session_id(Some("sess-1".to_string()))
            .build();
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let mut session = OpenCodeChatSession::spawn(&params, tx).await.unwrap();
        assert_eq!(session.session_id, "sess-1");

        let marker_count = std::fs::read_dir(&sessions)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .and_then(|e| e.to_str())
                    == Some("jsonl")
            })
            .count();
        assert_eq!(marker_count, 1, "resume must not create a second marker");

        tokio::time::sleep(Duration::from_millis(50)).await;
        session.send_turn("hi".to_string()).await.unwrap();
        let mtime_after = std::fs::metadata(&marker).unwrap().modified().unwrap();
        assert!(
            mtime_after > mtime_before,
            "marker mtime should advance on send_turn"
        );

        ChatSession::close(Box::new(session)).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resume_unknown_id_falls_back_to_create() {
        let temp = tempfile::tempdir().unwrap();
        let script = write_script(temp.path(), FAKE_SERVER);
        let sessions = temp.path().join("data").join("tui").join("sessions");
        let params = ChatSessionParams::builder(script.to_str().unwrap(), temp.path(), &sessions)
            .resume_session_id(Some("missing".to_string()))
            .build();
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let mut session = OpenCodeChatSession::spawn(&params, tx).await.unwrap();

        session.send_turn("hello".to_string()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let received = std::fs::read_to_string(temp.path().join("received.log")).unwrap();
        assert!(received.contains("hello"), "{received}");

        ChatSession::close(Box::new(session)).await.unwrap();
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
