//! Browser chat surface. It is deliberately an opt-in extension: without an
//! `extensions.chat-web` section it mounts neither routes nor dashboard tab.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    sync::Arc,
};

use anyhow::{Context, Result};
use axum::{
    extract::{DefaultBodyLimit, Multipart, Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware,
    response::{
        sse::{Event, KeepAlive, Sse},
        Html, IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use cap_chat::QuestionInfo;
use cap_dashboard_tab::{DashboardTab, DashboardTabs};
use dar_extension_sdk::{
    chat::{self, ChatBackend, ChatEvent, ChatRole, ChatSession, TurnOrigin},
    Extension, RegisterCtx, StartCtx,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, watch, Mutex};

const TAB_ID: &str = "chat";
const MAX_UPLOAD_BYTES: usize = 8 * 1024 * 1024;
const MAX_ATTACHMENTS: usize = 8;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    backend: Option<String>,
    command: Option<String>,
    idle_minutes: Option<u64>,
    /// Runtime kill switch: `false` skips mounting routes, the dashboard tab,
    /// and the chat coordinator service, even though the extension still
    /// links (build-time selection is by section presence). Mirrors the
    /// scheduler extension's `enabled` flag.
    enabled: Option<bool>,
}

#[derive(Default)]
pub struct ChatWebExtension {
    state: std::sync::OnceLock<Arc<AppState>>,
}

impl Extension for ChatWebExtension {
    fn id(&self) -> &'static str {
        "chat-web"
    }
    fn register<'a>(
        &'a self,
        ctx: &'a mut RegisterCtx,
    ) -> dar_extension_sdk::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let Some(value) = ctx.config.get(self.id()) else {
                return Ok(());
            };
            let config: Config = serde_json::from_value(value.clone())
                .context("invalid extensions.chat-web config")?;
            if config.enabled == Some(false) {
                return Ok(());
            }
            let state = Arc::new(AppState {
                config,
                root: ctx.paths.root().to_path_buf(),
                start: std::sync::OnceLock::new(),
                sessions: Mutex::new(HashMap::new()),
                live_id: Mutex::new(None),
                transition: Mutex::new(()),
                meta: Mutex::new(load_meta(
                    &ctx.paths.root().join("data/chat/sessions-meta.json"),
                )),
            });
            self.state
                .set(Arc::clone(&state))
                .map_err(|_| anyhow::anyhow!("chat-web registered twice"))?;
            migrate_tui_sessions(ctx.paths.root())?;
            let transcript = ctx.paths.root().join("data/chat/sessions/main.jsonl");
            if let Some(parent) = transcript.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(transcript, "")?;
            state.session("main").await?;
            let coordinator: Arc<dyn chat::ChatCoordinator> = state.clone();
            ctx.services.service::<dyn chat::ChatCoordinator>(
                chat::CHAT_COORDINATOR_SERVICE,
                coordinator,
            )?;
            DashboardTabs::shared(&mut ctx.services)?.add(Arc::new(ChatTab {
                agent_name: agent_display_name(ctx.paths.root()),
                agent_description: agent_description(ctx.paths.root()),
                agent_avatar: agent_avatar(ctx.paths.root()),
            }))?;
            ctx.http.mount(host_api::HttpMount {
                namespace: "/chat".into(),
                router: router(state),
                routes: vec![
                    "/".into(),
                    "/sessions".into(),
                    "/sessions/{id}".into(),
                    "/avatar".into(),
                    "/{session}/resume".into(),
                    "/{session}/stream".into(),
                    "/{session}/history".into(),
                    "/{session}/send".into(),
                    "/{session}/upload".into(),
                    "/{session}/attachment/{command}/{name}".into(),
                    "/{session}/abort".into(),
                    "/{session}/compact".into(),
                    "/{session}/new".into(),
                    "/{session}/answer".into(),
                ],
                claim_root: false,
            })?;
            Ok(())
        })
    }
    fn start<'a>(&'a self, ctx: StartCtx) -> dar_extension_sdk::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let Some(state) = self.state.get() else {
                return Ok(());
            };
            state
                .start
                .set(ctx)
                .map_err(|_| anyhow::anyhow!("chat-web started twice"))?;
            Ok(())
        })
    }

    fn stop<'a>(&'a self) -> dar_extension_sdk::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let Some(state) = self.state.get() else {
                return Ok(());
            };
            let sessions: Vec<Arc<Session>> =
                state.sessions.lock().await.values().cloned().collect();
            for session in sessions {
                let backend = session.inner.lock().await.take();
                if let Some(backend) = backend {
                    // Best-effort: one session's close failing must not skip
                    // closing the rest.
                    let _ = backend.close().await;
                }
            }
            Ok(())
        })
    }
}

struct ChatTab {
    agent_name: String,
    agent_description: Option<String>,
    agent_avatar: Option<Avatar>,
}
impl DashboardTab for ChatTab {
    fn id(&self) -> &str {
        TAB_ID
    }
    fn title(&self) -> &str {
        "Chat"
    }
    fn self_refreshing(&self) -> bool {
        true
    }
    fn passive_default(&self) -> bool {
        true
    }
    fn render(&self) -> Result<String> {
        Ok(format!(
            r#"<style>{}</style><section class="chat-web" id="chat-root" data-agent-name="{}"><aside class="chat-sidebar" aria-label="Conversations"><div class="chat-sidebar-head"><strong>Chats</strong><button type="button" id="chat-sidebar-toggle" data-sidebar-toggle class="chat-icon" aria-label="Toggle sidebar"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M4 5h16M4 12h16M4 19h16"/></svg></button></div><input id="chat-search" class="chat-search" type="search" placeholder="Search conversations" aria-label="Search conversations"><div id="chat-history-list" class="chat-history-list" aria-live="polite"></div></aside><div class="chat-main"><header class="chat-header"><button type="button" data-sidebar-toggle class="chat-icon chat-mobile-sidebar-toggle" aria-label="Open conversations"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M4 5h16M4 12h16M4 19h16"/></svg></button><div>{}<span class="chat-title"><strong>{}</strong>{}</span><span id="chat-token-meter" class="chat-meter"></span></div><button type="button" class="chat-new" onclick="fetch('/chat/main/new',{{method:'POST'}})"><svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" aria-hidden="true"><path d="M12 5v14M5 12h14"/></svg>New chat</button></header><div class="chat-dropzone" id="chat-dropzone" hidden>Drop files to attach</div><div class="chat-transcript" id="chat-transcript" role="log" aria-live="polite"></div><div class="chat-hero" id="chat-hero">{}<div class="chat-hero-line" id="chat-hero-line"></div></div><form class="chat-dock" id="chat-composer" autocomplete="off" onsubmit="event.preventDefault()"><div class="chat-chips" id="chat-chips"></div><div class="chat-cap-hint" id="chat-cap-hint"></div><div class="chat-row"><button type="button" id="chat-attach" class="chat-icon" aria-label="Attach files"><svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M21.4 11.1l-9.2 9.1a6 6 0 01-8.4-8.4L13 2.6a4 4 0 015.6 5.6l-9.2 9.2a2 2 0 01-2.8-2.8l8.5-8.5"/></svg></button><input type="file" id="chat-attachments" multiple hidden><textarea class="chat-input" id="chat-input" rows="1" placeholder="Message {}" aria-label="Message"></textarea><button type="button" id="chat-abort" class="chat-icon" aria-label="Stop" hidden><svg width="16" height="16" viewBox="0 0 24 24" fill="currentColor"><rect x="6" y="6" width="12" height="12" rx="2"/></svg></button><button type="submit" id="chat-send" class="chat-icon" aria-label="Send" disabled><svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2"><path d="M12 19V5M5 12l7-7 7 7"/></svg></button></div><div id="chat-context-warning" class="chat-context-warning" hidden>Context is filling up — send /compact to summarize.</div></form></div></section><script>{}</script><script>{}</script><script>{}</script>"#,
            include_str!("chat.css"),
            escape_html_attr(&self.agent_name),
            avatar_html(self.agent_avatar.as_ref(), "chat-avatar", &self.agent_name),
            escape_html_attr(&self.agent_name),
            self.agent_description
                .as_deref()
                .map(|d| format!("<small class=\"chat-desc\">{}</small>", escape_html_attr(d)))
                .unwrap_or_default(),
            avatar_html(
                self.agent_avatar.as_ref(),
                "chat-hero-avatar",
                &self.agent_name
            ),
            escape_html_attr(&self.agent_name),
            include_str!("vendor/marked.min.js"),
            include_str!("vendor/purify.min.js"),
            include_str!("renderer.js"),
        ))
    }
}

/// Reads `agent.yaml`'s `name` (falling back to `id`, then `"Agent"`) so the
/// Chat tab can show which agent it's talking to. Any read/parse error also
/// falls back to `"Agent"` — this is a display label, not a config gate.
fn agent_display_name(root: &std::path::Path) -> String {
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct AgentYaml {
        name: Option<String>,
        id: Option<String>,
    }
    fs::read_to_string(root.join("agent.yaml"))
        .ok()
        .and_then(|contents| serde_yaml::from_str::<AgentYaml>(&contents).ok())
        .and_then(|parsed| {
            parsed
                .name
                .map(|name| name.trim().to_owned())
                .filter(|name| !name.is_empty())
                .or_else(|| {
                    parsed
                        .id
                        .map(|id| id.trim().to_owned())
                        .filter(|id| !id.is_empty())
                })
        })
        .unwrap_or_else(|| "Agent".to_owned())
}

/// `agent.yaml`'s optional `description`, shown under the name in the header.
fn agent_description(root: &std::path::Path) -> Option<String> {
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct AgentYaml {
        description: Option<String>,
    }
    fs::read_to_string(root.join("agent.yaml"))
        .ok()
        .and_then(|contents| serde_yaml::from_str::<AgentYaml>(&contents).ok())
        .and_then(|parsed| parsed.description)
        .map(|d| d.trim().to_owned())
        .filter(|d| !d.is_empty())
}

/// `agent.yaml`'s optional top-level `avatar` (same shape as yoplai): an
/// emoji/short text, an `http(s)` image URL, or an image path relative to the
/// agent folder (served by `GET /chat/avatar`).
#[derive(Debug, PartialEq)]
enum Avatar {
    Text(String),
    Url(String),
    File(std::path::PathBuf),
}

fn agent_avatar(root: &std::path::Path) -> Option<Avatar> {
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct AgentYaml {
        avatar: Option<String>,
    }
    let value = fs::read_to_string(root.join("agent.yaml"))
        .ok()
        .and_then(|contents| serde_yaml::from_str::<AgentYaml>(&contents).ok())?
        .avatar?
        .trim()
        .to_owned();
    if value.is_empty() {
        return None;
    }
    if value.starts_with("https://") || value.starts_with("http://") {
        return Some(Avatar::Url(value));
    }
    // Image paths must resolve to a file inside the agent folder.
    let file = root.join(&value).canonicalize().ok().filter(|file| {
        file.is_file() && root.canonicalize().is_ok_and(|root| file.starts_with(root))
    });
    if let Some(file) = file {
        return Some(Avatar::File(file));
    }
    if value.chars().count() <= 8 && !value.contains(['.', '/', '\\']) {
        return Some(Avatar::Text(value));
    }
    tracing::warn!(avatar = %value, "agent.yaml avatar is not an emoji, URL, or image inside the agent folder; ignoring");
    None
}

/// Avatar markup; image `src` is set by the renderer so the fleet `__dashPrefix` applies.
fn avatar_html(avatar: Option<&Avatar>, class: &str, name: &str) -> String {
    match avatar {
        None => String::new(),
        Some(Avatar::Text(text)) => format!(
            r#"<span class="{class}" aria-hidden="true">{}</span>"#,
            escape_html_attr(text)
        ),
        Some(Avatar::Url(url)) => format!(
            r#"<span class="{class}"><img data-avatar-src="{}" alt="{}"></span>"#,
            escape_html_attr(url),
            escape_html_attr(name)
        ),
        Some(Avatar::File(_)) => format!(
            r#"<span class="{class}"><img data-avatar-src="/chat/avatar" alt="{}"></span>"#,
            escape_html_attr(name)
        ),
    }
}

async fn avatar(State(state): State<Arc<AppState>>) -> axum::response::Response {
    let Some(Avatar::File(file)) = agent_avatar(&state.root) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let name = file.to_string_lossy().to_string();
    let content_type = match attachment_content_type(&name) {
        "application/octet-stream" if name.to_ascii_lowercase().ends_with(".svg") => {
            "image/svg+xml"
        }
        other => other,
    };
    match fs::read(&file) {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, content_type),
                (header::CACHE_CONTROL, "no-cache"),
                // SVG avatars may carry scripts; never let them run on the dashboard origin.
                (
                    header::CONTENT_SECURITY_POLICY,
                    "default-src 'none'; style-src 'unsafe-inline'; sandbox",
                ),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

fn escape_html_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

struct AppState {
    config: Config,
    root: std::path::PathBuf,
    start: std::sync::OnceLock<StartCtx>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    live_id: Mutex<Option<String>>,
    meta: Mutex<HashMap<String, SessionMeta>>,
    transition: Mutex<()>,
}
#[cfg(test)]
struct PublishPause {
    sent: std::sync::mpsc::Sender<()>,
    proceed: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}
#[cfg(test)]
struct StreamPause {
    subscribed: std::sync::mpsc::Sender<()>,
    snapshot_done: std::sync::mpsc::Sender<()>,
    proceed: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}
struct Session {
    inner: Mutex<Option<Box<dyn ChatSession>>>,
    /// Serializes backend event publication with turn acceptance so an eager
    /// backend cannot publish output before its accepted user event.
    acceptance_lock: Mutex<()>,
    tx: broadcast::Sender<WireEvent>,
    events: broadcast::Sender<ChatEvent>,
    generation: std::sync::atomic::AtomicU64,
    next_seq: std::sync::atomic::AtomicU64,
    active_turns: std::sync::atomic::AtomicUsize,
    abort_requested: std::sync::atomic::AtomicBool,
    transcript_failed: std::sync::atomic::AtomicBool,
    suppress_resume: std::sync::atomic::AtomicBool,
    title_started: std::sync::atomic::AtomicBool,
    opened_after_ms: std::sync::atomic::AtomicU64,
    resolved_live_id: std::sync::Mutex<Option<String>>,
    abort_signal: watch::Sender<bool>,
    publish_lock: std::sync::Mutex<()>,
    command_ids: Mutex<HashSet<String>>,
    history: std::sync::Mutex<VecDeque<WireEvent>>,
    transcript: PathBuf,
    #[cfg(test)]
    pause_after_send: std::sync::Mutex<Option<Arc<PublishPause>>>,
    #[cfg(test)]
    pause_after_subscribe: std::sync::Mutex<Option<Arc<StreamPause>>>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct WireEvent {
    seq: u64,
    #[serde(default = "now_ms")]
    ts: u64,
    #[serde(rename = "type")]
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    args: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_error: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    done: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tokens_used: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_window: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    attachments: Vec<Attachment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    questions: Option<Vec<QuestionInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    origin: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    historical: bool,
}
#[derive(Clone, Serialize, Deserialize)]
struct Attachment {
    name: String,
    url: String,
    image: bool,
}
#[derive(Deserialize)]
struct Send {
    command_id: String,
    message: String,
}

fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/sessions", get(session_index))
        .route("/avatar", get(avatar))
        .route(
            "/sessions/{id}",
            get(session_transcript)
                .patch(patch_session)
                .delete(delete_session),
        )
        .route("/{session}/resume", post(resume_session))
        .route("/{session}/stream", get(stream))
        .route("/{session}/send", post(send))
        .route("/{session}/upload", post(upload))
        .route("/{session}/attachment/{command}/{name}", get(attachment))
        .route("/{session}/abort", post(abort))
        .route("/{session}/compact", post(compact))
        .route("/{session}/new", post(new_session_route))
        .route("/{session}/answer", post(answer))
        .layer(middleware::map_response(mark_prefix_aware))
        // `/{session}/history` is a standalone page, never spliced into the
        // dashboard shell, so no `window.__dashPrefix`/patched
        // `fetch`/`EventSource` exist on it. Registered after the layer so it
        // stays un-marked and the fleet proxy's compat rewriter prefixes its
        // URLs (attachments, EventSource) as before.
        .route("/{session}/history", get(history))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .with_state(state)
}
async fn index() -> Html<&'static str> {
    Html("chat web is available from the Chat dashboard tab")
}

#[derive(Deserialize)]
struct SessionPage {
    offset: Option<usize>,
    count: Option<usize>,
}

#[derive(serde::Serialize)]
struct SessionListEntry {
    id: String,
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    start_time: Option<String>,
    modified_ms: u64,
    archived: bool,
    is_live: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct SessionMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    archived: Option<bool>,
}

fn valid_backend_id(id: &str) -> bool {
    !id.is_empty() && !id.contains('/') && !id.contains('\\') && !id.contains("..")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn archive_timestamp_ms(value: &serde_json::Value) -> Option<u64> {
    if let Some(value) = value.as_u64() {
        return Some(value);
    }
    let text = value.as_str()?;
    let (date, time) = text.split_once('T')?;
    let mut date = date.split('-').map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (date.next()??, date.next()??, date.next()??);
    let time = time.trim_end_matches('Z');
    let mut parts = time.split(':');
    let hour = parts.next()?.parse::<i64>().ok()?;
    let minute = parts.next()?.parse::<i64>().ok()?;
    let seconds = parts.next()?;
    let (second, fraction) = seconds.split_once('.').unwrap_or((seconds, ""));
    let second = second.parse::<i64>().ok()?;
    let millis = fraction.chars().take(3).collect::<String>();
    let millis = format!("{millis:0<3}").parse::<i64>().ok()?;
    let adjusted_year = year - i64::from(month <= 2);
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let yoe = adjusted_year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * shifted_month + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    (days >= 0)
        .then_some(((days * 86400 + hour * 3600 + minute * 60 + second) * 1000 + millis) as u64)
}

fn load_meta(path: &std::path::Path) -> HashMap<String, SessionMeta> {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn write_meta(path: &std::path::Path, meta: &HashMap<String, SessionMeta>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(meta)?)?;
    fs::rename(tmp, path)?;
    Ok(())
}

async fn session_index(State(state): State<Arc<AppState>>) -> Json<Vec<SessionListEntry>> {
    let dir = state.root.join("data/chat/sessions");
    if let Ok(session) = state.session("main").await {
        state.resolve_live_id(&session).await;
    }
    let live = state.live_id.lock().await.clone();
    let meta = state.meta.lock().await.clone();
    let mut sessions: Vec<_> = chat::archive::list(&dir)
        .into_iter()
        .filter_map(|session| {
            let path = chat::archive::session_file_by_id(&dir, &session.id)?;
            let modified_ms = path
                .metadata()
                .ok()?
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_millis() as u64;
            let item_meta = meta.get(&session.id).cloned().unwrap_or_default();
            let title = item_meta.title.filter(|title| !title.trim().is_empty());
            Some(SessionListEntry {
                is_live: live.as_deref() == Some(&session.id),
                label: title.clone().unwrap_or(session.label),
                id: session.id,
                title,
                start_time: session.start_time,
                modified_ms,
                archived: item_meta.archived.unwrap_or(false),
            })
        })
        .collect();
    sessions.sort_by_key(|session| std::cmp::Reverse(session.modified_ms));
    Json(sessions)
}

async fn session_transcript(
    Path(id): Path<String>,
    Query(page): Query<SessionPage>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    if id.is_empty() || id.contains('/') || id.contains('\\') {
        return StatusCode::NOT_FOUND.into_response();
    }
    match chat::archive::read_events(
        &state.root.join("data/chat/sessions"),
        &id,
        page.offset.unwrap_or(0),
        page.count.unwrap_or(100),
    ) {
        Some(page) => Json(page).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Deserialize)]
struct SessionPatch {
    title: Option<String>,
    archived: Option<bool>,
}

async fn patch_session(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<SessionPatch>,
) -> impl IntoResponse {
    let dir = state.root.join("data/chat/sessions");
    if !valid_backend_id(&id) || chat::archive::session_file_by_id(&dir, &id).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let mut meta = state.meta.lock().await;
    let entry = meta.entry(id).or_default();
    if let Some(title) = body.title {
        let title = title.trim().to_owned();
        entry.title = (!title.is_empty()).then_some(title);
    }
    if let Some(archived) = body.archived {
        entry.archived = Some(archived);
    }
    match write_meta(&state.root.join("data/chat/sessions-meta.json"), &meta) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    }
}

async fn delete_session(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let dir = state.root.join("data/chat/sessions");
    let Some(path) = valid_backend_id(&id)
        .then(|| chat::archive::session_file_by_id(&dir, &id))
        .flatten()
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let _transition = state.transition.lock().await;
    if let Ok(session) = state.session("main").await {
        state.resolve_live_id(&session).await;
    }
    if state.live_id.lock().await.as_deref() == Some(&id) {
        if let Err(error) = state.reset_session_unlocked("main").await {
            return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
        }
    }
    if let Err(error) = fs::remove_file(path) {
        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
    }
    let mut meta = state.meta.lock().await;
    meta.remove(&id);
    if let Err(error) = write_meta(&state.root.join("data/chat/sessions-meta.json"), &meta) {
        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize)]
struct ResumeRequest {
    id: String,
}

async fn resume_session(
    Path(session_id): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<ResumeRequest>,
) -> impl IntoResponse {
    let dir = state.root.join("data/chat/sessions");
    if !valid_backend_id(&body.id) || chat::archive::session_file_by_id(&dir, &body.id).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let _transition = state.transition.lock().await;
    if let Err(error) = state.reset_session_unlocked(&session_id).await {
        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
    }
    let session = match state.session(&session_id).await {
        Ok(session) => session,
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };
    let mut offset = 0;
    loop {
        let Some(page) =
            chat::archive::read_events(&dir, &body.id, offset, chat::archive::READ_MAX)
        else {
            return StatusCode::NOT_FOUND.into_response();
        };
        for value in page.events {
            let timestamp = value
                .get("timestamp")
                .and_then(archive_timestamp_ms)
                .unwrap_or_else(now_ms);
            let mut event: WireEvent = match serde_json::from_value(value) {
                Ok(event) => event,
                Err(_) => continue,
            };
            event.ts = timestamp;
            event.historical = true;
            let _publish = session
                .publish_lock
                .lock()
                .expect("chat-web publish mutex poisoned");
            event.seq = session
                .next_seq
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            if let Err(error) = append_transcript(&session.transcript, &event) {
                return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
            }
            session
                .history
                .lock()
                .expect("chat-web history mutex poisoned")
                .push_back(event.clone());
            let _ = session.tx.send(event);
        }
        match page.next_offset {
            Some(next) => offset = next,
            None => break,
        }
    }
    *session
        .resolved_live_id
        .lock()
        .expect("chat-web identity mutex poisoned") = Some(body.id.clone());
    *state.live_id.lock().await = Some(body.id);
    session
        .suppress_resume
        .store(false, std::sync::atomic::Ordering::SeqCst);
    StatusCode::ACCEPTED.into_response()
}

// Tells the fleet proxy (`dar dash`) this response's URLs are already
// prefix-correct at request time (shell JS shim), so it must skip its
// regex HTML rewriter for this response.
async fn mark_prefix_aware(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert("x-prefix-aware", HeaderValue::from_static("1"));
    response
}

impl AppState {
    async fn session(&self, id: &str) -> Result<Arc<Session>> {
        if id.is_empty() || id.contains('/') || id.contains('\\') || id == "." || id == ".." {
            anyhow::bail!("invalid session id");
        }
        let mut sessions = self.sessions.lock().await;
        if let Some(s) = sessions.get(id).cloned() {
            return Ok(s);
        }
        let (tx, _) = broadcast::channel(256);
        let (events, _) = broadcast::channel(256);
        let (abort_signal, _) = watch::channel(false);
        let transcript = self.transcript_path(id);
        let history = load_transcript(&transcript)?;
        let next_seq = history.back().map(|event| event.seq).unwrap_or(0);
        let session = Arc::new(Session {
            inner: Mutex::new(None),
            acceptance_lock: Mutex::new(()),
            tx,
            events,
            generation: std::sync::atomic::AtomicU64::new(0),
            next_seq: std::sync::atomic::AtomicU64::new(next_seq),
            active_turns: std::sync::atomic::AtomicUsize::new(0),
            abort_requested: std::sync::atomic::AtomicBool::new(false),
            transcript_failed: std::sync::atomic::AtomicBool::new(false),
            suppress_resume: std::sync::atomic::AtomicBool::new(false),
            title_started: std::sync::atomic::AtomicBool::new(false),
            opened_after_ms: std::sync::atomic::AtomicU64::new(0),
            resolved_live_id: std::sync::Mutex::new(None),
            abort_signal,
            publish_lock: std::sync::Mutex::new(()),
            command_ids: Mutex::new(HashSet::new()),
            history: std::sync::Mutex::new(history),
            transcript,
            #[cfg(test)]
            pause_after_send: std::sync::Mutex::new(None),
            #[cfg(test)]
            pause_after_subscribe: std::sync::Mutex::new(None),
        });
        sessions.insert(id.to_owned(), Arc::clone(&session));
        Ok(session)
    }

    fn transcript_path(&self, id: &str) -> PathBuf {
        self.root
            .join("data/chat/sessions")
            .join(format!("{id}.jsonl"))
    }

    async fn open_session(&self, session: Arc<Session>) -> Result<Box<dyn ChatSession>> {
        let start = self.start.get().context("chat-web has not started")?;
        let backend_id = chat::resolve_agent_backend(start, self.config.backend.as_deref());
        let backend = start
            .host
            .services
            .get_named::<dyn ChatBackend>(&backend_id)
            .with_context(|| format!("chat backend {backend_id:?} is not registered"))?;
        let session_dir = self.root.join("data/chat/sessions");
        std::fs::create_dir_all(&session_dir)?;
        self.resolve_live_id(&session).await;
        let resume_session_id = if session
            .suppress_resume
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            None
        } else {
            self.live_id.lock().await.clone()
        };
        session
            .opened_after_ms
            .store(now_ms(), std::sync::atomic::Ordering::SeqCst);
        let params = chat::agent_session_params(start, &session_dir)
            .command(self.config.command.as_deref().unwrap_or(""))
            .resume_session_id(resume_session_id)
            .build();
        let generation = session
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let (event_tx, mut event_rx) = mpsc::channel::<ChatEvent>(128);
        let sink = Arc::clone(&session);
        let identity_sink = Arc::clone(&session);
        let identity_dir = session_dir.clone();
        let identity_backend = backend_id.clone();
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                let terminal = matches!(
                    event,
                    ChatEvent::TurnFinished { .. } | ChatEvent::SessionClosed { .. }
                );
                let _acceptance = sink.acceptance_lock.lock().await;
                if sink.publish_if_current(generation, event.clone()) {
                    let _ = sink.events.send(event);
                    if terminal {
                        let opened_after = identity_sink
                            .opened_after_ms
                            .load(std::sync::atomic::Ordering::SeqCst);
                        if let Some(found) =
                            newest_session_since(&identity_dir, &identity_backend, opened_after)
                        {
                            // Reset bumps the generation before clearing the identity, so
                            // rechecking under the identity lock drops a stale terminal event.
                            let mut resolved = identity_sink
                                .resolved_live_id
                                .lock()
                                .expect("chat-web identity mutex poisoned");
                            if identity_sink
                                .generation
                                .load(std::sync::atomic::Ordering::SeqCst)
                                == generation
                            {
                                *resolved = Some(found.id);
                            }
                        }
                    }
                }
            }
        });
        backend.open(params, event_tx).await
    }

    /// Holds `live_id` throughout; reset clears the resolved identity and
    /// open time before `live_id`, so a concurrent reset can't be undone here.
    async fn resolve_live_id(&self, session: &Session) {
        let mut live = self.live_id.lock().await;
        if live.is_some() {
            return;
        }
        let resolved = session
            .resolved_live_id
            .lock()
            .expect("chat-web identity mutex poisoned")
            .clone();
        if let Some(id) = resolved {
            *live = Some(id);
            return;
        }
        let Some(start) = self.start.get() else {
            return;
        };
        let opened_after = session
            .opened_after_ms
            .load(std::sync::atomic::Ordering::SeqCst);
        if opened_after == 0 {
            return;
        }
        let backend_id = chat::resolve_agent_backend(start, self.config.backend.as_deref());
        if let Some(found) = newest_session_since(
            &self.root.join("data/chat/sessions"),
            &backend_id,
            opened_after,
        ) {
            *session
                .resolved_live_id
                .lock()
                .expect("chat-web identity mutex poisoned") = Some(found.id.clone());
            *live = Some(found.id);
        }
    }

    async fn finish_turn(
        &self,
        web_session: Arc<Session>,
        generation: u64,
        first_user: String,
        assistant: String,
    ) {
        if web_session
            .generation
            .load(std::sync::atomic::Ordering::SeqCst)
            != generation
        {
            return;
        }
        let Some(start) = self.start.get() else {
            return;
        };
        let backend_id = chat::resolve_agent_backend(start, self.config.backend.as_deref());
        let dir = self.root.join("data/chat/sessions");
        self.resolve_live_id(&web_session).await;
        let current_id = self.live_id.lock().await.clone();
        let id = match current_id {
            Some(id) => id,
            None => {
                let opened_after = web_session
                    .opened_after_ms
                    .load(std::sync::atomic::Ordering::SeqCst);
                let Some(session) = newest_session_since(&dir, &backend_id, opened_after) else {
                    return;
                };
                if web_session
                    .generation
                    .load(std::sync::atomic::Ordering::SeqCst)
                    != generation
                {
                    return;
                }
                *self.live_id.lock().await = Some(session.id.clone());
                session.id
            }
        };
        if self
            .meta
            .lock()
            .await
            .get(&id)
            .and_then(|meta| meta.title.as_ref())
            .is_some()
        {
            return;
        }
        let Ok(backend) = start
            .host
            .services
            .get_named::<dyn ChatBackend>(&backend_id)
        else {
            return;
        };
        let temp = self.root.join("data/chat/.titler").join(format!(
            "{}-{}",
            now_ms(),
            std::process::id()
        ));
        if fs::create_dir_all(&temp).is_err() {
            return;
        }
        let params = chat::agent_session_params(start, &temp)
            .command(self.config.command.as_deref().unwrap_or(""))
            .system_prompt(None)
            .host_tool_bridge(None)
            .resume_session_id(None)
            .build();
        let (tx, mut rx) = mpsc::channel(64);
        let mut title_session = match backend.open(params, tx).await {
            Ok(session) => session,
            Err(error) => {
                tracing::warn!("chat title session failed to open: {error:#}");
                let _ = fs::remove_dir_all(&temp);
                return;
            }
        };
        let prompt = format!("Return a concise 3-6 word title summarizing this chat in the conversation's language. No quotes. No punctuation at the end.\n\nUser: {}\n\nAssistant: {}", truncate_chars(&first_user, 2000), truncate_chars(&assistant, 2000));
        let collect = async {
            title_session.send_turn(prompt).await?;
            let mut title = String::new();
            loop {
                match rx.recv().await {
                    Some(ChatEvent::Delta {
                        role: ChatRole::Assistant,
                        text,
                    }) => title.push_str(&text),
                    Some(ChatEvent::TurnFinished { ok: true, .. }) => break,
                    Some(ChatEvent::TurnFinished { .. }) | Some(ChatEvent::Error(_)) => {
                        anyhow::bail!("title generation failed")
                    }
                    Some(_) => {}
                    None => anyhow::bail!("title session ended before successful finish"),
                }
            }
            Result::<String>::Ok(normalize_title(&title))
        };
        let result = tokio::time::timeout(title_timeout(), collect).await;
        if let Err(error) = title_session.close().await {
            tracing::warn!("chat title session close failed: {error:#}");
        }
        if let Err(error) = fs::remove_dir_all(&temp) {
            tracing::warn!("chat title temp cleanup failed: {error:#}");
        }
        let title = match result {
            Ok(Ok(title)) if !title.is_empty() => title,
            Ok(Ok(_)) => return,
            Ok(Err(error)) => {
                tracing::warn!("chat title generation failed: {error:#}");
                return;
            }
            Err(_) => {
                tracing::warn!("chat title generation timed out");
                return;
            }
        };
        if web_session
            .generation
            .load(std::sync::atomic::Ordering::SeqCst)
            != generation
        {
            return;
        };
        let mut meta = self.meta.lock().await;
        if meta
            .get(&id)
            .and_then(|entry| entry.title.as_ref())
            .is_none()
        {
            meta.entry(id.clone()).or_default().title = Some(title);
            if let Err(error) = write_meta(&self.root.join("data/chat/sessions-meta.json"), &meta) {
                tracing::warn!("chat title metadata write failed: {error:#}");
                meta.entry(id).or_default().title = None;
            }
        }
    }

    async fn reset_session(&self, id: &str) -> Result<()> {
        let _transition = self.transition.lock().await;
        self.reset_session_unlocked(id).await
    }

    async fn reset_session_unlocked(&self, id: &str) -> Result<()> {
        let session = self.session(id).await?;
        let backend = session.inner.lock().await.take();
        session
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        session
            .active_turns
            .store(0, std::sync::atomic::Ordering::SeqCst);
        session
            .abort_requested
            .store(false, std::sync::atomic::Ordering::SeqCst);
        session
            .transcript_failed
            .store(false, std::sync::atomic::Ordering::SeqCst);
        session
            .suppress_resume
            .store(true, std::sync::atomic::Ordering::SeqCst);
        *session
            .resolved_live_id
            .lock()
            .expect("chat-web identity mutex poisoned") = None;
        session
            .opened_after_ms
            .store(0, std::sync::atomic::Ordering::SeqCst);
        *self.live_id.lock().await = None;
        let _ = session.abort_signal.send(false);
        {
            let _publish = session
                .publish_lock
                .lock()
                .expect("chat-web publish mutex poisoned");
            session
                .history
                .lock()
                .expect("chat-web history mutex poisoned")
                .clear();
            if let Some(parent) = session.transcript.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&session.transcript, "")?;
            let seq = session
                .next_seq
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            let event = WireEvent {
                seq,
                ts: now_ms(),
                kind: "reset".into(),
                text: None,
                id: None,
                name: None,
                args: None,
                is_error: None,
                done: None,
                error: None,
                tokens_used: None,
                context_window: None,
                attachments: vec![],
                questions: None,
                origin: None,
                historical: false,
            };
            append_transcript(&session.transcript, &event)?;
            session
                .history
                .lock()
                .expect("chat-web history mutex poisoned")
                .push_back(event.clone());
            let _ = session.tx.send(event);
        }
        let _ = session.events.send(ChatEvent::SessionReset);
        session
            .title_started
            .store(false, std::sync::atomic::Ordering::SeqCst);
        if let Some(backend) = backend {
            backend.close().await?;
        }
        Ok(())
    }
}

fn title_timeout() -> std::time::Duration {
    #[cfg(test)]
    {
        std::time::Duration::from_millis(50)
    }
    #[cfg(not(test))]
    {
        std::time::Duration::from_secs(30)
    }
}

fn truncate_chars(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

fn normalize_title(value: &str) -> String {
    let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed
        .trim_matches(|c: char| matches!(c, '\'' | '"' | '`'))
        .trim_end_matches(|c: char| c.is_ascii_punctuation())
        .trim();
    if trimmed.chars().count() <= 60 {
        return trimmed.to_owned();
    }
    let prefix: String = trimmed.chars().take(60).collect();
    prefix
        .rsplit_once(char::is_whitespace)
        .map(|(head, _)| head)
        .unwrap_or(&prefix)
        .to_owned()
}

fn migrate_tui_sessions(root: &std::path::Path) -> Result<()> {
    let shared = root.join("data/chat/sessions");
    if !shared_is_empty(&shared)? || !root.join("data/tui/sessions").exists() {
        return Ok(());
    }
    if shared.exists() {
        fs::remove_dir(&shared)?;
    }
    std::fs::create_dir_all(shared.parent().expect("shared sessions has parent"))?;
    std::fs::rename(root.join("data/tui/sessions"), shared)?;
    Ok(())
}

fn shared_is_empty(path: &std::path::Path) -> Result<bool> {
    if !path.exists() {
        return Ok(true);
    }
    Ok(fs::read_dir(path)?.next().is_none())
}

struct PersistedSession {
    id: String,
    modified: std::time::SystemTime,
}

#[cfg(test)]
fn newest_session(dir: &std::path::Path, backend_id: &str) -> Option<PersistedSession> {
    newest_session_since(dir, backend_id, 0)
}

fn newest_session_since(
    dir: &std::path::Path,
    backend_id: &str,
    modified_after_ms: u64,
) -> Option<PersistedSession> {
    fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            (path.file_stem().is_some_and(|stem| stem != "main")
                && path.extension().is_some_and(|ext| ext == "jsonl"))
            .then(|| {
                let header = BufReader::new(fs::File::open(&path).ok()?)
                    .lines()
                    .find(|line| line.as_ref().is_ok_and(|line| !line.trim().is_empty()))
                    .and_then(Result::ok)
                    .and_then(|line| serde_json::from_str::<serde_json::Value>(&line).ok())?;
                (header.get("type")?.as_str()? == "session").then_some(())?;
                (header
                    .get("backend")
                    .and_then(|v| v.as_str())
                    .unwrap_or("pi")
                    == backend_id)
                    .then_some(())?;
                let id = header.get("id")?.as_str()?.to_owned();
                let modified = entry.metadata().ok()?.modified().ok()?;
                let modified_ms = modified
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()?
                    .as_millis() as u64;
                (modified_ms >= modified_after_ms).then_some(PersistedSession { id, modified })
            })?
        })
        .max_by_key(|session| session.modified)
}

impl chat::ChatCoordinator for AppState {
    fn send_turn<'a>(
        &'a self,
        prompt: String,
        display: String,
    ) -> dar_extension_sdk::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let _transition = self.transition.lock().await;
            let session = self.session("main").await?;
            let mut inner = session.inner.lock().await;
            if inner.is_none() {
                *inner = Some(self.open_session(Arc::clone(&session)).await?);
            }
            session
                .accept_turn(
                    inner.as_mut().expect("session opened above").as_mut(),
                    prompt,
                    display,
                    vec![],
                )
                .await
        })
    }

    fn abort<'a>(&'a self) -> dar_extension_sdk::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let session = self.session("main").await?;
            let mut inner = session.inner.lock().await;
            let backend = inner.as_mut().context("no active chat session")?;
            backend.abort().await
        })
    }

    fn new_session<'a>(&'a self) -> dar_extension_sdk::BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.reset_session("main").await })
    }

    fn subscribe(&self) -> broadcast::Receiver<ChatEvent> {
        // The main session is created during registration so subscriptions can
        // attach before the first browser or TUI turn.
        self.sessions
            .try_lock()
            .ok()
            .and_then(|sessions| {
                sessions
                    .get("main")
                    .map(|session| session.events.subscribe())
            })
            .expect("main chat session is initialized at registration")
    }

    fn answer_question<'a>(
        &'a self,
        request_id: String,
        answers: Vec<Vec<String>>,
    ) -> dar_extension_sdk::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let session = self.session("main").await?;
            let mut inner = session.inner.lock().await;
            let backend = inner.as_mut().context("no active chat session")?;
            backend.answer_question(request_id, answers).await
        })
    }
}
impl Session {
    async fn accept_turn(
        &self,
        backend: &mut dyn ChatSession,
        prompt: String,
        display: String,
        attachments: Vec<Attachment>,
    ) -> Result<()> {
        let _acceptance = self.acceptance_lock.lock().await;
        if self
            .transcript_failed
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            anyhow::bail!("chat transcript storage is unavailable; start a new session");
        }
        backend.send_turn(prompt).await?;
        if let Err(error) = self.publish_user(display.clone(), attachments) {
            let _ = backend.abort().await;
            return Err(error.context("failed to persist accepted chat turn"));
        }
        let _ = self.events.send(ChatEvent::User { text: display });
        self.active_turns
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    fn publish_user(&self, text: String, attachments: Vec<Attachment>) -> Result<()> {
        let _publish = self
            .publish_lock
            .lock()
            .expect("chat-web publish mutex poisoned");
        let seq = self
            .next_seq
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let event = WireEvent {
            seq,
            ts: now_ms(),
            kind: "user".into(),
            text: Some(text),
            id: None,
            name: None,
            args: None,
            is_error: None,
            done: None,
            error: None,
            tokens_used: None,
            context_window: None,
            attachments,
            questions: None,
            origin: None,
            historical: false,
        };
        append_transcript(&self.transcript, &event)?;
        self.history
            .lock()
            .expect("chat-web history mutex poisoned")
            .push_back(event.clone());
        let _ = self.tx.send(event);
        Ok(())
    }
    fn publish_if_current(&self, generation: u64, event: ChatEvent) -> bool {
        if self.generation.load(std::sync::atomic::Ordering::SeqCst) != generation {
            return false;
        }
        self.publish(event)
    }

    fn publish(&self, event: ChatEvent) -> bool {
        let (
            kind,
            text,
            error,
            id,
            name,
            args,
            is_error,
            done,
            tokens_used,
            context_window,
            questions,
            origin,
        ) = match event {
            ChatEvent::User { .. } | ChatEvent::SessionReset => return true,
            ChatEvent::TurnStarted { origin } => {
                if origin == TurnOrigin::Autonomous {
                    self.active_turns
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                (
                    "started",
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some(match origin {
                        TurnOrigin::Submitted => "submitted".to_owned(),
                        TurnOrigin::Autonomous => "autonomous".to_owned(),
                    }),
                )
            }
            ChatEvent::Delta {
                role: ChatRole::Assistant,
                text,
            } => (
                "delta",
                Some(text),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            ),
            ChatEvent::Delta {
                role: ChatRole::Thinking,
                text,
            } => (
                "thinking",
                Some(text),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            ),
            ChatEvent::ToolCall { id, name, args } => (
                "tool_call",
                None,
                None,
                Some(id),
                Some(name),
                Some(args),
                None,
                None,
                None,
                None,
                None,
                None,
            ),
            ChatEvent::ToolOutput {
                id,
                text,
                is_error,
                done,
            } => (
                "tool_output",
                Some(text),
                None,
                Some(id),
                None,
                None,
                Some(is_error),
                Some(done),
                None,
                None,
                None,
                None,
            ),
            ChatEvent::QuestionAsked {
                request_id,
                questions,
            } => (
                "question",
                None,
                None,
                Some(request_id),
                None,
                None,
                None,
                None,
                None,
                None,
                Some(questions),
                None,
            ),
            ChatEvent::QuestionResolved {
                request_id,
                answers,
                rejected,
            } => (
                "question_done",
                Some(if rejected {
                    "dismissed".to_owned()
                } else {
                    answers
                        .iter()
                        .map(|answer| answer.join(", "))
                        .collect::<Vec<_>>()
                        .join("; ")
                }),
                None,
                Some(request_id),
                None,
                None,
                Some(rejected),
                None,
                None,
                None,
                None,
                None,
            ),
            // Kept in the transcript (raw text) but rendered as nothing.
            ChatEvent::Silent { reason, text } => (
                "silent",
                Some(text),
                reason.map(|reason| reason.as_str().to_owned()),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            ),
            ChatEvent::Error(error) => (
                "error",
                None,
                Some(error),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            ),
            ChatEvent::TurnFinished { ok, error } => (
                if ok { "finished" } else { "aborted" },
                None,
                error,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            ),
            ChatEvent::ContextUsage {
                tokens_used,
                context_window,
            } => (
                "context_usage",
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(tokens_used),
                context_window,
                None,
                None,
            ),
            ChatEvent::SessionClosed { error } => (
                "closed", None, error, None, None, None, None, None, None, None, None, None,
            ),
        };
        let terminal = matches!(kind, "finished" | "aborted" | "closed");
        if terminal && self.active_turns.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            return true;
        }
        let _publish = self
            .publish_lock
            .lock()
            .expect("chat-web publish mutex poisoned");
        let seq = self
            .next_seq
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let event = WireEvent {
            seq,
            ts: now_ms(),
            kind: kind.to_owned(),
            text,
            id,
            name,
            args,
            is_error,
            done,
            error,
            tokens_used,
            context_window,
            attachments: vec![],
            questions,
            origin,
            historical: false,
        };
        let mut history = self
            .history
            .lock()
            .expect("chat-web history mutex poisoned");
        if let Err(error) = append_transcript(&self.transcript, &event) {
            drop(history);
            drop(_publish);
            self.fail_transcript(error);
            return false;
        }
        if terminal {
            let turns = self
                .active_turns
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            if turns == 1 {
                self.abort_requested
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                let _ = self.abort_signal.send(false);
            }
        }
        history.push_back(event.clone());
        let _ = self.tx.send(event);
        #[cfg(test)]
        {
            drop(history);
            drop(_publish);
        }
        #[cfg(test)]
        if let Some(pause) = self.pause_after_send.lock().unwrap().as_ref() {
            let _ = pause.sent.send(());
            let _ = pause.proceed.lock().unwrap().recv();
        }
        true
    }

    fn fail_transcript(&self, error: anyhow::Error) {
        self.transcript_failed
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let turns = self
            .active_turns
            .swap(0, std::sync::atomic::Ordering::SeqCst);
        self.abort_requested
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let _ = self.abort_signal.send(false);

        let message = format!("chat transcript write failed: {error:#}");
        let _ = self.events.send(ChatEvent::Error(message.clone()));
        let _publish = self
            .publish_lock
            .lock()
            .expect("chat-web publish mutex poisoned");
        let mut history = self
            .history
            .lock()
            .expect("chat-web history mutex poisoned");
        self.publish_volatile(&mut history, "error", Some(message.clone()));
        for _ in 0..turns {
            let _ = self.events.send(ChatEvent::TurnFinished {
                ok: false,
                error: Some(message.clone()),
            });
            self.publish_volatile(&mut history, "aborted", Some(message.clone()));
        }
    }

    fn publish_volatile(
        &self,
        history: &mut VecDeque<WireEvent>,
        kind: &str,
        error: Option<String>,
    ) {
        let seq = self
            .next_seq
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let event = WireEvent {
            seq,
            ts: now_ms(),
            kind: kind.to_owned(),
            text: None,
            id: None,
            name: None,
            args: None,
            is_error: None,
            done: None,
            error,
            tokens_used: None,
            context_window: None,
            attachments: vec![],
            questions: None,
            origin: None,
            historical: false,
        };
        history.push_back(event.clone());
        let _ = self.tx.send(event);
    }
}
fn load_transcript(path: &std::path::Path) -> Result<VecDeque<WireEvent>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(VecDeque::new()),
        Err(error) => return Err(error.into()),
    };
    BufReader::new(file)
        .lines()
        .map(|line| Ok(serde_json::from_str(&line?)?))
        .collect()
}

fn append_transcript(path: &std::path::Path, event: &WireEvent) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(&mut file, event)?;
    file.write_all(b"\n")?;
    file.sync_data()?;
    Ok(())
}

async fn history(Path(id): Path<String>, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.session(&id).await {
        Ok(session) => {
            let events: Vec<_> = {
                let _publish = session
                    .publish_lock
                    .lock()
                    .expect("chat-web publish mutex poisoned");
                match load_transcript(&session.transcript) {
                    Ok(events) => events.into(),
                    Err(error) => {
                        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response()
                    }
                }
            };
            let json = serde_json::to_string(&events)
                .expect("WireEvent serializes")
                .replace('&', "\\u0026")
                .replace('<', "\\u003c")
                .replace('>', "\\u003e")
                .replace('\u{2028}', "\\u2028")
                .replace('\u{2029}', "\\u2029");
            Html(format!(r#"<div id="chat-transcript"></div><script>{}for (const event of {}) window.renderChatEvent(event);</script>"#, include_str!("renderer.js"), json)).into_response()
        }
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    }
}
async fn stream(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    match state.session(&id).await {
        Ok(s) => {
            let last = headers
                .get("last-event-id")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            let (live, replay) = {
                let _publish = s
                    .publish_lock
                    .lock()
                    .expect("chat-web publish mutex poisoned");
                let live = s.tx.subscribe();
                #[cfg(test)]
                if let Some(pause) = s.pause_after_subscribe.lock().unwrap().as_ref() {
                    let _ = pause.subscribed.send(());
                    let _ = pause.proceed.lock().unwrap().recv();
                }
                let replay: Vec<_> = match load_transcript(&s.transcript) {
                    Ok(events) => events,
                    Err(error) => {
                        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response()
                    }
                }
                .into_iter()
                .filter(|event| event.seq > last)
                .collect();
                #[cfg(test)]
                if let Some(pause) = s.pause_after_subscribe.lock().unwrap().as_ref() {
                    let _ = pause.snapshot_done.send(());
                }
                (live, replay)
            };
            let cutoff = replay.last().map(|event| event.seq).unwrap_or(last);
            let replay = futures_util::stream::iter(
                replay.into_iter().map(Ok::<_, std::convert::Infallible>),
            );
            let live = futures_util::stream::unfold(
                (live, cutoff, Arc::clone(&s), VecDeque::<WireEvent>::new()),
                |(mut live, mut last, session, mut pending)| async move {
                    loop {
                        if let Some(event) = pending.pop_front() {
                            last = event.seq;
                            return Some((
                                Ok::<_, std::convert::Infallible>(event),
                                (live, last, session, pending),
                            ));
                        }
                        match live.recv().await {
                            Ok(event) if event.seq <= last => continue,
                            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {
                                let _publish = session
                                    .publish_lock
                                    .lock()
                                    .expect("chat-web publish mutex poisoned");
                                pending = match load_transcript(&session.transcript) {
                                    Ok(events) => events
                                        .into_iter()
                                        .filter(|event| event.seq > last)
                                        .collect(),
                                    Err(_) => return None,
                                };
                            }
                            Err(broadcast::error::RecvError::Closed) => return None,
                        }
                    }
                },
            );
            // Terminate the SSE response on host shutdown: the `live` unfold
            // above only ends when the session's broadcast sender closes,
            // which never happens on its own, so without this a browser tab
            // holding the stream open blocks `dar-host`'s graceful shutdown
            // until the tab disconnects. When `start` is unset (unit tests
            // that build `AppState` directly), fall back to a future that
            // never resolves so behavior is unchanged.
            let shutdown_signal: dar_extension_sdk::BoxFuture<'static, ()> =
                match state.start.get().map(|start| start.shutdown.clone()) {
                    Some(mut token) => Box::pin(async move { token.cancelled().await }),
                    None => Box::pin(std::future::pending()),
                };
            let stream = replay
                .chain(live)
                .map(sse_event)
                .take_until(shutdown_signal);
            Sse::new(stream)
                .keep_alive(KeepAlive::default())
                .into_response()
        }
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    }
}
fn sse_event(
    event: Result<WireEvent, std::convert::Infallible>,
) -> Result<Event, std::convert::Infallible> {
    let event = event?;
    Ok(Event::default()
        .id(event.seq.to_string())
        .data(serde_json::to_string(&event).expect("WireEvent serializes")))
}
async fn send(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<Send>,
) -> impl IntoResponse {
    submit(state, id, body.command_id, body.message, vec![], true).await
}

async fn submit(
    state: Arc<AppState>,
    id: String,
    command_id: String,
    message: String,
    attachments: Vec<Attachment>,
    reserve_command: bool,
) -> axum::response::Response {
    if command_id.trim().is_empty() || (message.trim().is_empty() && attachments.is_empty()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"accepted":false})),
        )
            .into_response();
    }
    let _transition = state.transition.lock().await;
    match state.session(&id).await {
        Ok(s) => {
            if reserve_command && !s.command_ids.lock().await.insert(command_id.clone()) {
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({"accepted":false,"error":"duplicate command_id"})),
                )
                    .into_response();
            }
            let mut guard = s.inner.lock().await;
            if guard.is_none() {
                match state.open_session(Arc::clone(&s)).await {
                    Ok(opened) => *guard = Some(opened),
                    Err(e) => {
                        s.command_ids.lock().await.remove(&command_id);
                        return (
                            StatusCode::SERVICE_UNAVAILABLE,
                            Json(serde_json::json!({"accepted":false,"error":e.to_string()})),
                        )
                            .into_response();
                    }
                }
            }
            let prompt = attachment_prompt(&message, &attachments, &state.root);
            let display = display_message(&message, &attachments);
            let mut aborted = s.abort_signal.subscribe();
            let mut completion = s.events.subscribe();
            let accepted = tokio::select! {
                result = s.accept_turn(
                    guard.as_mut().expect("session open").as_mut(),
                    prompt,
                    display.clone(),
                    attachments,
                ) => result,
                _ = aborted.changed() => Err(anyhow::anyhow!("turn aborted before acceptance")),
            };
            match accepted {
                Ok(()) => {
                    if !s
                        .title_started
                        .swap(true, std::sync::atomic::Ordering::SeqCst)
                    {
                        let watcher = Arc::clone(&state);
                        let watched_session = Arc::clone(&s);
                        let generation = s.generation.load(std::sync::atomic::Ordering::SeqCst);
                        let first_user = display.clone();
                        tokio::spawn(async move {
                            let mut assistant = String::new();
                            while let Ok(event) = completion.recv().await {
                                if watched_session
                                    .generation
                                    .load(std::sync::atomic::Ordering::SeqCst)
                                    != generation
                                {
                                    return;
                                }
                                match event {
                                    ChatEvent::Delta {
                                        role: ChatRole::Assistant,
                                        text,
                                    } => assistant.push_str(&text),
                                    ChatEvent::TurnFinished { ok: true, .. } => break,
                                    ChatEvent::TurnFinished { .. }
                                    | ChatEvent::SessionReset
                                    | ChatEvent::SessionClosed { .. } => return,
                                    _ => {}
                                }
                            }
                            watcher
                                .finish_turn(watched_session, generation, first_user, assistant)
                                .await;
                        });
                    }
                    (
                        StatusCode::ACCEPTED,
                        Json(serde_json::json!({"accepted":true,"command_id":command_id})),
                    )
                        .into_response()
                }
                Err(e) => {
                    s.command_ids.lock().await.remove(&command_id);
                    (
                        StatusCode::CONFLICT,
                        Json(serde_json::json!({"accepted":false,"error":e.to_string()})),
                    )
                        .into_response()
                }
            }
        }
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"accepted":false,"error":e.to_string()})),
        )
            .into_response(),
    }
}

async fn upload(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
    mut multipart: Multipart,
) -> axum::response::Response {
    if !safe_component(&id) {
        return upload_error("invalid session id");
    }
    let mut command_id = None;
    let mut message = None;
    let mut files = Vec::new();
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) if error.to_string().contains("length limit") => {
                return StatusCode::PAYLOAD_TOO_LARGE.into_response()
            }
            Err(_) => return upload_error("invalid multipart upload"),
        };
        match field.name() {
            Some("command_id") => command_id = field.text().await.ok(),
            Some("message") => message = field.text().await.ok(),
            Some("attachment") => {
                if files.len() == MAX_ATTACHMENTS {
                    return upload_error("too many attachments");
                }
                let Some(name) = field.file_name().map(str::to_owned) else {
                    return upload_error("attachment needs a filename");
                };
                let mime = field.content_type().map(str::to_owned).unwrap_or_default();
                if !allowed_attachment(&mime) {
                    return upload_error("unsupported attachment type");
                }
                let name = safe_filename(&name);
                if name.is_empty() {
                    return upload_error("invalid attachment filename");
                }
                match field.bytes().await {
                    Ok(bytes) if !bytes.is_empty() => files.push((name, mime, bytes)),
                    Ok(_) => return upload_error("empty attachment"),
                    Err(_) => return upload_error("invalid attachment"),
                }
            }
            _ => return upload_error("invalid upload field"),
        }
    }
    let Some(command_id) = command_id else {
        return upload_error("command_id is required");
    };
    if !safe_component(&command_id) {
        return upload_error("invalid command_id");
    }
    if files.is_empty() {
        return upload_error("attachment is required");
    }
    let session = match state.session(&id).await {
        Ok(session) => session,
        Err(error) => return upload_error(&error.to_string()),
    };
    if !session.command_ids.lock().await.insert(command_id.clone()) {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"accepted":false,"error":"duplicate command_id"})),
        )
            .into_response();
    }
    let dir = state
        .root
        .join("data/chat/uploads")
        .join(&id)
        .join(&command_id);
    if let Err(error) = fs::create_dir_all(&dir) {
        session.command_ids.lock().await.remove(&command_id);
        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
    }
    let mut attachments = Vec::new();
    for (index, (name, mime, bytes)) in files.into_iter().enumerate() {
        let stored = format!("{index}-{name}");
        if let Err(error) = fs::write(dir.join(&stored), bytes) {
            session.command_ids.lock().await.remove(&command_id);
            return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
        }
        attachments.push(Attachment {
            name,
            url: format!("/chat/{id}/attachment/{command_id}/{stored}"),
            image: mime.starts_with("image/"),
        });
    }
    submit(
        state,
        id,
        command_id,
        message.unwrap_or_default(),
        attachments,
        false,
    )
    .await
}

fn attachment_prompt(message: &str, attachments: &[Attachment], root: &std::path::Path) -> String {
    let paths = attachments
        .iter()
        .filter_map(|attachment| {
            attachment
                .url
                .split_once("/attachment/")
                .map(|(prefix, path)| {
                    let session = prefix.rsplit('/').next().unwrap_or_default();
                    root.join("data/chat/uploads")
                        .join(session)
                        .join(path)
                        .display()
                        .to_string()
                })
        })
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return message.to_owned();
    }
    format!(
        "{message}\n\nAttachments available at:\n{}",
        paths
            .into_iter()
            .map(|path| format!("- {path}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

fn display_message(message: &str, attachments: &[Attachment]) -> String {
    format!(
        "{message}{}",
        attachments
            .iter()
            .map(|attachment| format!("\n[attachment: {}]", attachment.name))
            .collect::<String>()
    )
}

fn allowed_attachment(mime: &str) -> bool {
    let mime = mime.split(';').next().unwrap_or_default().trim();
    (mime.starts_with("image/") && mime != "image/svg+xml")
        || matches!(
            mime,
            "application/pdf" | "text/plain" | "text/markdown" | "application/json"
        )
}

fn safe_filename(name: &str) -> String {
    name.rsplit(['/', '\\'])
        .next()
        .unwrap_or("attachment")
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_')
        })
        .take(100)
        .collect::<String>()
        .trim_matches('.')
        .to_owned()
}

fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

fn upload_error(error: &str) -> axum::response::Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"accepted":false,"error":error})),
    )
        .into_response()
}

async fn attachment(
    Path((id, command, name)): Path<(String, String, String)>,
    State(state): State<Arc<AppState>>,
) -> axum::response::Response {
    if !safe_component(&id)
        || !safe_component(&command)
        || name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\\'])
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match fs::read(
        state
            .root
            .join("data/chat/uploads")
            .join(id)
            .join(command)
            .join(&name),
    ) {
        Ok(bytes) => (
            [(header::CONTENT_TYPE, attachment_content_type(&name))],
            bytes,
        )
            .into_response(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            StatusCode::NOT_FOUND.into_response()
        }
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    }
}

fn attachment_content_type(name: &str) -> &'static str {
    match name
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "application/octet-stream",
        "pdf" => "application/pdf",
        "txt" | "md" => "text/plain; charset=utf-8",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
}
async fn abort(Path(id): Path<String>, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.session(&id).await {
        Ok(s) => {
            if s.active_turns.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                return (StatusCode::CONFLICT, "no active turn").into_response();
            }
            s.abort_requested
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = s.abort_signal.send(true);
            let mut inner = s.inner.lock().await;
            let backend = inner.take();
            s.generation
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let turns = s.active_turns.load(std::sync::atomic::Ordering::SeqCst);
            for _ in 0..turns {
                s.publish(ChatEvent::TurnFinished {
                    ok: false,
                    error: Some("aborted".into()),
                });
            }
            drop(inner);
            if let Some(mut backend) = backend {
                tokio::spawn(async move {
                    if backend.abort().await.is_ok() {
                        let _ = backend.close().await;
                    }
                });
            }
            StatusCode::ACCEPTED.into_response()
        }
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    }
}

async fn compact(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<Compact>,
) -> impl IntoResponse {
    send(
        Path(id),
        State(state),
        Json(Send {
            command_id: body.command_id,
            message: "/compact".into(),
        }),
    )
    .await
}

#[derive(Deserialize)]
struct Compact {
    command_id: String,
}

async fn new_session_route(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    match state.reset_session(&id).await {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct Answer {
    request_id: String,
    answers: Vec<Vec<String>>,
}

// Deliberately no lazy `open_session` here: after a restart the pending
// question died with the old backend process, so answering must fail
// cleanly (409), not spawn a fresh backend.
async fn answer(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<Answer>,
) -> impl IntoResponse {
    if body.request_id.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"accepted":false})),
        )
            .into_response();
    }
    match state.session(&id).await {
        Ok(s) => {
            let mut inner = s.inner.lock().await;
            let Some(backend) = inner.as_mut() else {
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({"accepted":false,"error":"no active chat session"})),
                )
                    .into_response();
            };
            match backend.answer_question(body.request_id, body.answers).await {
                Ok(()) => (
                    StatusCode::ACCEPTED,
                    Json(serde_json::json!({"accepted":true})),
                )
                    .into_response(),
                Err(e) => (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({"accepted":false,"error":e.to_string()})),
                )
                    .into_response(),
            }
        }
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"accepted":false,"error":e.to_string()})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tower::ServiceExt;

    struct FakeSession {
        aborted: Arc<AtomicBool>,
        sends: Arc<std::sync::atomic::AtomicUsize>,
        abort_fails: bool,
    }
    impl ChatSession for FakeSession {
        fn send_turn(&mut self, _prompt: String) -> cap_chat::BoxFuture<'_, Result<()>> {
            self.sends.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
        fn abort(&mut self) -> cap_chat::BoxFuture<'_, Result<()>> {
            let flag = Arc::clone(&self.aborted);
            let fail = self.abort_fails;
            Box::pin(async move {
                flag.store(true, Ordering::SeqCst);
                if fail {
                    anyhow::bail!("backend abort failed")
                }
                Ok(())
            })
        }
        fn close(self: Box<Self>) -> cap_chat::BoxFuture<'static, Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }
    struct RejectingSession;
    impl ChatSession for RejectingSession {
        fn send_turn(&mut self, _prompt: String) -> cap_chat::BoxFuture<'_, Result<()>> {
            Box::pin(async { anyhow::bail!("turn rejected") })
        }
        fn abort(&mut self) -> cap_chat::BoxFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn close(self: Box<Self>) -> cap_chat::BoxFuture<'static, Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }
    type AnswerCalls = Arc<std::sync::Mutex<Vec<(String, Vec<Vec<String>>)>>>;
    struct AnsweringSession {
        calls: AnswerCalls,
        fails: bool,
    }
    impl ChatSession for AnsweringSession {
        fn send_turn(&mut self, _prompt: String) -> cap_chat::BoxFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn abort(&mut self) -> cap_chat::BoxFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn close(self: Box<Self>) -> cap_chat::BoxFuture<'static, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn answer_question(
            &mut self,
            request_id: String,
            answers: Vec<Vec<String>>,
        ) -> cap_chat::BoxFuture<'_, Result<()>> {
            let calls = Arc::clone(&self.calls);
            let fails = self.fails;
            Box::pin(async move {
                if fails {
                    anyhow::bail!("backend rejected answer");
                }
                calls.lock().unwrap().push((request_id, answers));
                Ok(())
            })
        }
    }
    struct HangingAbortSession;
    impl ChatSession for HangingAbortSession {
        fn send_turn(&mut self, _prompt: String) -> cap_chat::BoxFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn abort(&mut self) -> cap_chat::BoxFuture<'_, Result<()>> {
            Box::pin(std::future::pending())
        }
        fn close(self: Box<Self>) -> cap_chat::BoxFuture<'static, Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }
    fn session(inner: Box<dyn ChatSession>) -> Arc<Session> {
        let (tx, _) = broadcast::channel(8);
        let (events, _) = broadcast::channel(8);
        Arc::new(Session {
            inner: Mutex::new(Some(inner)),
            acceptance_lock: Mutex::new(()),
            tx,
            events,
            generation: std::sync::atomic::AtomicU64::new(0),
            next_seq: std::sync::atomic::AtomicU64::new(0),
            active_turns: std::sync::atomic::AtomicUsize::new(0),
            abort_requested: std::sync::atomic::AtomicBool::new(false),
            transcript_failed: std::sync::atomic::AtomicBool::new(false),
            suppress_resume: std::sync::atomic::AtomicBool::new(false),
            title_started: std::sync::atomic::AtomicBool::new(false),
            opened_after_ms: std::sync::atomic::AtomicU64::new(0),
            resolved_live_id: std::sync::Mutex::new(None),
            abort_signal: watch::channel(false).0,
            publish_lock: std::sync::Mutex::new(()),
            command_ids: Mutex::new(HashSet::new()),
            history: std::sync::Mutex::new(VecDeque::new()),
            transcript: std::env::temp_dir()
                .join(format!("chat-web-test-{}.jsonl", uuid::Uuid::new_v4())),
            pause_after_send: std::sync::Mutex::new(None),
            pause_after_subscribe: std::sync::Mutex::new(None),
        })
    }

    #[test]
    fn silent_event_is_kept_raw_in_history() {
        let s = session(Box::new(RejectingSession));
        assert!(s.publish(ChatEvent::Silent {
            reason: None,
            text: "NO_REPLY".into(),
        }));
        let history = s.history.lock().unwrap();
        let event = history.back().unwrap();
        assert_eq!(event.kind, "silent");
        assert_eq!(event.text.as_deref(), Some("NO_REPLY"));
    }

    struct ClosingSession {
        closed: Arc<AtomicBool>,
    }
    impl ChatSession for ClosingSession {
        fn send_turn(&mut self, _prompt: String) -> cap_chat::BoxFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn abort(&mut self) -> cap_chat::BoxFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn close(self: Box<Self>) -> cap_chat::BoxFuture<'static, Result<()>> {
            let closed = self.closed;
            Box::pin(async move {
                closed.store(true, Ordering::SeqCst);
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn stop_closes_open_sessions() {
        let closed = Arc::new(AtomicBool::new(false));
        let session = session(Box::new(ClosingSession {
            closed: Arc::clone(&closed),
        }));
        let mut sessions = HashMap::new();
        sessions.insert("main".to_owned(), session);
        let state = Arc::new(AppState {
            config: Config::default(),
            root: test_root(),
            start: std::sync::OnceLock::new(),
            sessions: Mutex::new(sessions),
            live_id: Mutex::new(None),
            transition: Mutex::new(()),
            meta: Mutex::new(HashMap::new()),
        });
        let extension = ChatWebExtension::default();
        extension
            .state
            .set(state)
            .unwrap_or_else(|_| panic!("state set once"));

        extension.stop().await.unwrap();

        assert!(closed.load(Ordering::SeqCst));
    }

    struct FakeBackend {
        opens: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[derive(Clone, Copy)]
    enum TitlerMode {
        Success,
        Failure,
        Timeout,
    }
    struct TitlerBackend {
        mode: TitlerMode,
        closed: Arc<AtomicBool>,
    }
    struct TitlerSession {
        mode: TitlerMode,
        events: mpsc::Sender<ChatEvent>,
        closed: Arc<AtomicBool>,
    }
    impl ChatSession for TitlerSession {
        fn send_turn(&mut self, _prompt: String) -> cap_chat::BoxFuture<'_, Result<()>> {
            let mode = self.mode;
            let events = self.events.clone();
            Box::pin(async move {
                match mode {
                    TitlerMode::Success => {
                        events
                            .send(ChatEvent::Delta {
                                role: ChatRole::Assistant,
                                text: "Useful title.".into(),
                            })
                            .await?;
                        events
                            .send(ChatEvent::TurnFinished {
                                ok: true,
                                error: None,
                            })
                            .await?;
                    }
                    TitlerMode::Failure => anyhow::bail!("mock title failure"),
                    TitlerMode::Timeout => std::future::pending::<()>().await,
                }
                Ok(())
            })
        }
        fn abort(&mut self) -> cap_chat::BoxFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn close(self: Box<Self>) -> cap_chat::BoxFuture<'static, Result<()>> {
            let closed = self.closed;
            Box::pin(async move {
                closed.store(true, Ordering::SeqCst);
                Ok(())
            })
        }
    }
    impl ChatBackend for TitlerBackend {
        fn open<'a>(
            &'a self,
            _params: cap_chat::ChatSessionParams,
            events: mpsc::Sender<ChatEvent>,
        ) -> cap_chat::BoxFuture<'a, Result<Box<dyn ChatSession>>> {
            let mode = self.mode;
            let closed = Arc::clone(&self.closed);
            Box::pin(async move {
                Ok(Box::new(TitlerSession {
                    mode,
                    events,
                    closed,
                }) as Box<dyn ChatSession>)
            })
        }
    }

    struct StreamingSession {
        events: mpsc::Sender<ChatEvent>,
    }
    impl ChatSession for StreamingSession {
        fn send_turn(&mut self, _prompt: String) -> cap_chat::BoxFuture<'_, Result<()>> {
            let events = self.events.clone();
            Box::pin(async move {
                events
                    .send(ChatEvent::Delta {
                        role: ChatRole::Assistant,
                        text: "reply".into(),
                    })
                    .await?;
                Ok(())
            })
        }
        fn abort(&mut self) -> cap_chat::BoxFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn close(self: Box<Self>) -> cap_chat::BoxFuture<'static, Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }
    struct StreamingBackend {
        opens: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl ChatBackend for StreamingBackend {
        fn open<'a>(
            &'a self,
            _params: cap_chat::ChatSessionParams,
            events: mpsc::Sender<ChatEvent>,
        ) -> cap_chat::BoxFuture<'a, Result<Box<dyn ChatSession>>> {
            let opens = Arc::clone(&self.opens);
            Box::pin(async move {
                opens.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(StreamingSession { events }) as Box<dyn ChatSession>)
            })
        }
    }
    impl ChatBackend for FakeBackend {
        fn open<'a>(
            &'a self,
            _params: cap_chat::ChatSessionParams,
            _tx: mpsc::Sender<ChatEvent>,
        ) -> cap_chat::BoxFuture<'a, Result<Box<dyn ChatSession>>> {
            let opens = Arc::clone(&self.opens);
            Box::pin(async move {
                opens.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(FakeSession {
                    aborted: Arc::new(AtomicBool::new(false)),
                    sends: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                    abort_fails: false,
                }) as Box<dyn ChatSession>)
            })
        }
    }

    fn start_ctx(services: dar_extension_sdk::ServiceRegistry) -> StartCtx {
        let paths = host_api::HostPaths::new(std::env::current_dir().unwrap()).unwrap();
        let (shutdown_tx, shutdown) = watch::channel(false);
        // Leak the sender so the channel never closes: a dropped sender makes
        // `ShutdownToken::cancelled()` resolve immediately (as if shutdown had
        // fired), which would end every SSE stream in tests using this helper
        // as soon as `stream()`'s shutdown-signal future is polled.
        std::mem::forget(shutdown_tx);
        let register = host_api::RegisterCtx {
            bus: host_api::EventBus::new(),
            http: host_api::HttpRegistry::default(),
            foreground: host_api::ForegroundRegistry::default(),
            services,
            paths: paths.clone(),
            config: host_api::ConfigStore::default(),
            shutdown: host_api::ShutdownToken::new(shutdown),
        };
        StartCtx {
            shutdown: register.shutdown.clone(),
            paths,
            config: register.config.clone(),
            host: register.into_start_services().unwrap(),
        }
    }

    fn test_root() -> PathBuf {
        std::env::temp_dir().join(uuid::Uuid::new_v4().to_string())
    }

    #[test]
    fn newest_session_resumes_and_idle_sessions_expire() {
        let root = test_root();
        let sessions = root.join("data/chat/sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(
            sessions.join("2026-01-01_a.jsonl"),
            r#"{"type":"session","id":"resume-me"}"#,
        )
        .unwrap();
        fs::write(
            sessions.join("2026-01-02_b.jsonl"),
            r#"{"type":"session","id":"resume-oc","backend":"opencode"}"#,
        )
        .unwrap();

        // The pi<->opencode cross-resume guard: an untagged (legacy pi)
        // header resumes only under "pi", a "backend":"opencode" header only
        // under "opencode" — never the other way around.
        let pi_pick = newest_session(&sessions, "pi").unwrap();
        assert_eq!(pi_pick.id, "resume-me");
        let opencode_pick = newest_session(&sessions, "opencode").unwrap();
        assert_eq!(opencode_pick.id, "resume-oc");

        // idle_minutes remains accepted for config compatibility, but no
        // longer influences fresh-by-default startup.
    }

    fn register_ctx(root: PathBuf, config_value: serde_json::Value) -> host_api::RegisterCtx {
        let paths = host_api::HostPaths::new(root).unwrap();
        let (_, shutdown) = watch::channel(false);
        let mut values = HashMap::new();
        values.insert("chat-web".to_string(), config_value);
        host_api::RegisterCtx {
            bus: host_api::EventBus::new(),
            http: host_api::HttpRegistry::default(),
            foreground: host_api::ForegroundRegistry::default(),
            services: dar_extension_sdk::ServiceRegistry::default(),
            paths,
            config: host_api::ConfigStore::from_values(values),
            shutdown: host_api::ShutdownToken::new(shutdown),
        }
    }

    #[tokio::test]
    async fn enabled_false_registers_nothing() {
        let root = test_root();
        fs::create_dir_all(&root).unwrap();
        let mut ctx = register_ctx(root, serde_json::json!({ "enabled": false }));

        let extension = ChatWebExtension::default();
        extension.register(&mut ctx).await.unwrap();

        assert!(
            extension.state.get().is_none(),
            "enabled: false must not mount any routes, tab, or coordinator service"
        );
    }

    #[test]
    fn migration_uses_an_existing_empty_shared_directory() {
        let root = test_root();
        fs::create_dir_all(root.join("data/chat/sessions")).unwrap();
        let tui = root.join("data/tui/sessions");
        fs::create_dir_all(&tui).unwrap();
        fs::write(
            tui.join("2026-01-01_a.jsonl"),
            r#"{"type":"session","id":"legacy"}"#,
        )
        .unwrap();
        migrate_tui_sessions(&root).unwrap();
        assert!(root.join("data/chat/sessions/2026-01-01_a.jsonl").exists());
    }

    #[tokio::test]
    async fn stream_does_not_open_backend_before_first_send() {
        let opens = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut services = dar_extension_sdk::ServiceRegistry::default();
        services
            .register::<dyn ChatBackend>(
                "fake",
                Arc::new(FakeBackend {
                    opens: Arc::clone(&opens),
                }),
            )
            .unwrap();
        let state = Arc::new(AppState {
            config: Config {
                backend: Some("fake".into()),
                ..Config::default()
            },
            root: test_root(),
            start: std::sync::OnceLock::from(start_ctx(services)),
            sessions: Mutex::new(HashMap::new()),
            live_id: Mutex::new(None),
            transition: Mutex::new(()),
            meta: Mutex::new(HashMap::new()),
        });

        let response = stream(
            Path("test".into()),
            State(Arc::clone(&state)),
            HeaderMap::new(),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(state
            .session("test")
            .await
            .unwrap()
            .inner
            .lock()
            .await
            .is_none());
        assert_eq!(opens.load(Ordering::SeqCst), 0);
        assert_eq!(
            send(
                Path("test".into()),
                State(state),
                Json(Send {
                    command_id: "one".into(),
                    message: "hello".into()
                }),
            )
            .await
            .into_response()
            .status(),
            StatusCode::ACCEPTED
        );
        assert_eq!(opens.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stream_ends_when_host_shutdown_fires() {
        let paths = host_api::HostPaths::new(std::env::current_dir().unwrap()).unwrap();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let register = host_api::RegisterCtx {
            bus: host_api::EventBus::new(),
            http: host_api::HttpRegistry::default(),
            foreground: host_api::ForegroundRegistry::default(),
            services: dar_extension_sdk::ServiceRegistry::default(),
            paths: paths.clone(),
            config: host_api::ConfigStore::default(),
            shutdown: host_api::ShutdownToken::new(shutdown_rx),
        };
        let start = StartCtx {
            shutdown: register.shutdown.clone(),
            paths,
            config: register.config.clone(),
            host: register.into_start_services().unwrap(),
        };
        let state = Arc::new(AppState {
            config: Config::default(),
            root: test_root(),
            start: std::sync::OnceLock::from(start),
            sessions: Mutex::new(HashMap::new()),
            live_id: Mutex::new(None),
            transition: Mutex::new(()),
            meta: Mutex::new(HashMap::new()),
        });

        let response = stream(
            Path("test".into()),
            State(Arc::clone(&state)),
            HeaderMap::new(),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body();

        // Nothing has been published, so absent the shutdown signal this
        // stream would sit open indefinitely (mirroring a browser tab
        // holding it open). Flipping the host's shutdown watch channel must
        // end it promptly.
        shutdown_tx.send(true).unwrap();
        let frame = tokio::time::timeout(std::time::Duration::from_secs(1), body.frame())
            .await
            .expect("stream must end promptly once host shutdown fires");
        assert!(
            frame.is_none(),
            "stream should terminate with no further frames"
        );
    }

    #[tokio::test]
    async fn rejected_http_send_has_no_user_event_or_active_turn() {
        let s = session(Box::new(RejectingSession));
        let state = Arc::new(AppState {
            config: Config::default(),
            root: test_root(),
            start: std::sync::OnceLock::new(),
            sessions: Mutex::new(HashMap::from([("test".into(), Arc::clone(&s))])),
            live_id: Mutex::new(None),
            transition: Mutex::new(()),
            meta: Mutex::new(HashMap::new()),
        });
        let mut events = s.events.subscribe();

        let response = send(
            Path("test".into()),
            State(state),
            Json(Send {
                command_id: "rejected-1".into(),
                message: "hello".into(),
            }),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(s.active_turns.load(Ordering::SeqCst), 0);
        assert!(s.history.lock().unwrap().is_empty());
        assert!(events.try_recv().is_err());
        assert!(!s.command_ids.lock().await.contains("rejected-1"));
    }

    #[tokio::test]
    async fn accepted_turn_transcript_failure_aborts_without_shared_side_effects() {
        let aborted = Arc::new(AtomicBool::new(false));
        let s = session(Box::new(FakeSession {
            aborted: Arc::clone(&aborted),
            sends: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            abort_fails: false,
        }));
        fs::create_dir(&s.transcript).unwrap();
        let mut wire_events = s.tx.subscribe();
        let mut shared_events = s.events.subscribe();
        let mut backend = s.inner.lock().await.take().unwrap();

        let result = s
            .accept_turn(backend.as_mut(), "prompt".into(), "display".into(), vec![])
            .await;

        assert!(result.is_err());
        assert!(aborted.load(Ordering::SeqCst));
        assert_eq!(s.active_turns.load(Ordering::SeqCst), 0);
        assert!(s.history.lock().unwrap().is_empty());
        assert!(wire_events.try_recv().is_err());
        assert!(shared_events.try_recv().is_err());
        fs::remove_dir(&s.transcript).unwrap();
    }

    #[test]
    fn backend_transcript_failure_terminalizes_clients_without_panicking() {
        let s = session(Box::new(RejectingSession));
        s.active_turns.store(1, Ordering::SeqCst);
        fs::create_dir(&s.transcript).unwrap();
        let generation = s.generation.load(Ordering::SeqCst);
        let mut wire_events = s.tx.subscribe();
        let mut shared_events = s.events.subscribe();

        assert!(!s.publish_if_current(
            generation,
            ChatEvent::Delta {
                role: ChatRole::Assistant,
                text: "reply".into(),
            },
        ));

        assert_eq!(s.active_turns.load(Ordering::SeqCst), 0);
        assert_eq!(wire_events.try_recv().unwrap().kind, "error");
        assert_eq!(wire_events.try_recv().unwrap().kind, "aborted");
        assert!(matches!(
            shared_events.try_recv().unwrap(),
            ChatEvent::Error(_)
        ));
        assert!(matches!(
            shared_events.try_recv().unwrap(),
            ChatEvent::TurnFinished { ok: false, .. }
        ));
        assert!(!s.publish_if_current(
            generation,
            ChatEvent::Delta {
                role: ChatRole::Assistant,
                text: "late".into(),
            },
        ));
        fs::remove_dir(&s.transcript).unwrap();
    }

    #[tokio::test]
    async fn coordinator_send_waits_for_resume_transition() {
        let sends = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let session = session(Box::new(FakeSession {
            sends: Arc::clone(&sends),
            aborted: Arc::new(AtomicBool::new(false)),
            abort_fails: false,
        }));
        let state = Arc::new(AppState {
            config: Config::default(),
            root: test_root(),
            start: std::sync::OnceLock::new(),
            sessions: Mutex::new(HashMap::from([("main".into(), session)])),
            live_id: Mutex::new(None),
            transition: Mutex::new(()),
            meta: Mutex::new(HashMap::new()),
        });
        let transition = state.transition.lock().await;
        let sending = {
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                chat::ChatCoordinator::send_turn(state.as_ref(), "prompt".into(), "display".into())
                    .await
            })
        };
        tokio::task::yield_now().await;
        assert_eq!(
            sends.load(Ordering::SeqCst),
            0,
            "coordinator must not cross resume transition"
        );
        drop(transition);
        sending.await.unwrap().unwrap();
        assert_eq!(sends.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn coordinator_publishes_user_before_eager_backend_output() {
        let opens = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut services = dar_extension_sdk::ServiceRegistry::default();
        services
            .register::<dyn ChatBackend>(
                "fake",
                Arc::new(StreamingBackend {
                    opens: Arc::clone(&opens),
                }),
            )
            .unwrap();
        let state = AppState {
            config: Config {
                backend: Some("fake".into()),
                ..Config::default()
            },
            root: test_root(),
            start: std::sync::OnceLock::from(start_ctx(services)),
            sessions: Mutex::new(HashMap::new()),
            live_id: Mutex::new(None),
            transition: Mutex::new(()),
            meta: Mutex::new(HashMap::new()),
        };
        let session = state.session("main").await.unwrap();
        let mut events = session.events.subscribe();

        chat::ChatCoordinator::send_turn(&state, "prompt".into(), "display".into())
            .await
            .unwrap();

        assert!(matches!(
            events.recv().await.unwrap(),
            ChatEvent::User { text } if text == "display"
        ));
        assert!(matches!(
            events.recv().await.unwrap(),
            ChatEvent::Delta { role: ChatRole::Assistant, text } if text == "reply"
        ));
        assert_eq!(session.active_turns.load(Ordering::SeqCst), 1);
        assert_eq!(opens.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn rejected_coordinator_send_has_no_user_event_or_active_turn() {
        let s = session(Box::new(RejectingSession));
        let state = AppState {
            config: Config::default(),
            root: test_root(),
            start: std::sync::OnceLock::new(),
            sessions: Mutex::new(HashMap::from([("main".into(), Arc::clone(&s))])),
            live_id: Mutex::new(None),
            transition: Mutex::new(()),
            meta: Mutex::new(HashMap::new()),
        };
        let mut events = s.events.subscribe();

        assert!(
            chat::ChatCoordinator::send_turn(&state, "prompt".into(), "display".into())
                .await
                .is_err()
        );
        assert_eq!(s.active_turns.load(Ordering::SeqCst), 0);
        assert!(s.history.lock().unwrap().is_empty());
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn compact_posts_a_command_and_usage_is_persisted_for_sse() {
        let sends = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let s = session(Box::new(FakeSession {
            aborted: Arc::new(AtomicBool::new(false)),
            sends: Arc::clone(&sends),
            abort_fails: false,
        }));
        let state = Arc::new(AppState {
            config: Config::default(),
            root: test_root(),
            start: std::sync::OnceLock::new(),
            sessions: Mutex::new(HashMap::from([("test".into(), Arc::clone(&s))])),
            live_id: Mutex::new(None),
            transition: Mutex::new(()),
            meta: Mutex::new(HashMap::new()),
        });

        assert_eq!(
            router(Arc::clone(&state))
                .oneshot(
                    axum::http::Request::builder()
                        .method("POST")
                        .uri("/test/compact")
                        .header("content-type", "application/json")
                        .body(Body::from(r#"{"command_id":"compact-1"}"#))
                        .unwrap(),
                )
                .await
                .unwrap()
                .status(),
            StatusCode::ACCEPTED
        );
        assert_eq!(sends.load(Ordering::SeqCst), 1);
        assert_eq!(
            load_transcript(&s.transcript).unwrap()[0].text.as_deref(),
            Some("/compact")
        );

        s.publish(ChatEvent::ContextUsage {
            tokens_used: 12_345,
            context_window: Some(200_000),
        });
        let usage = load_transcript(&s.transcript).unwrap().pop_back().unwrap();
        assert_eq!(usage.kind, "context_usage");
        assert_eq!(usage.tokens_used, Some(12_345));
        assert_eq!(usage.context_window, Some(200_000));
    }

    #[tokio::test]
    async fn upload_accepts_attachment_and_rejects_duplicate_command_id() {
        let sends = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let s = session(Box::new(FakeSession {
            aborted: Arc::new(AtomicBool::new(false)),
            sends: Arc::clone(&sends),
            abort_fails: false,
        }));
        let root = test_root();
        let state = Arc::new(AppState {
            config: Config::default(),
            root: root.clone(),
            start: std::sync::OnceLock::new(),
            sessions: Mutex::new(HashMap::from([("test".into(), Arc::clone(&s))])),
            live_id: Mutex::new(None),
            transition: Mutex::new(()),
            meta: Mutex::new(HashMap::new()),
        });
        let body = "--x\r\nContent-Disposition: form-data; name=\"command_id\"\r\n\r\nupload-1\r\n--x\r\nContent-Disposition: form-data; name=\"message\"\r\n\r\ninspect this\r\n--x\r\nContent-Disposition: form-data; name=\"attachment\"; filename=\"note.txt\"\r\nContent-Type: text/plain\r\n\r\nhello\r\n--x--\r\n";
        let request = || {
            axum::http::Request::builder()
                .method("POST")
                .uri("/test/upload")
                .header("content-type", "multipart/form-data; boundary=x")
                .body(Body::from(body))
                .unwrap()
        };
        let app = router(state);
        assert_eq!(
            app.clone().oneshot(request()).await.unwrap().status(),
            StatusCode::ACCEPTED
        );
        assert_eq!(
            app.oneshot(request()).await.unwrap().status(),
            StatusCode::CONFLICT
        );
        assert_eq!(sends.load(Ordering::SeqCst), 1);
        let event = load_transcript(&s.transcript).unwrap().pop_front().unwrap();
        assert_eq!(event.attachments[0].name, "note.txt");
        assert!(root
            .join("data/chat/uploads/test/upload-1/0-note.txt")
            .exists());
    }

    #[tokio::test]
    async fn upload_rejects_invalid_and_oversize_bodies() {
        let state = Arc::new(AppState {
            config: Config::default(),
            root: test_root(),
            start: std::sync::OnceLock::new(),
            sessions: Mutex::new(HashMap::new()),
            live_id: Mutex::new(None),
            transition: Mutex::new(()),
            meta: Mutex::new(HashMap::new()),
        });
        let invalid = "--x\r\nContent-Disposition: form-data; name=\"attachment\"; filename=\"bad.exe\"\r\nContent-Type: application/octet-stream\r\n\r\nbad\r\n--x--\r\n";
        let request = |body: Vec<u8>| {
            let length = body.len();
            axum::http::Request::builder()
                .method("POST")
                .uri("/test/upload")
                .header("content-type", "multipart/form-data; boundary=x")
                .header("content-length", length)
                .body(Body::from(body))
                .unwrap()
        };
        assert_eq!(
            router(Arc::clone(&state))
                .oneshot(request(invalid.as_bytes().to_vec()))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        let mut oversize = b"--x\r\nContent-Disposition: form-data; name=\"attachment\"; filename=\"large.txt\"\r\nContent-Type: text/plain\r\n\r\n".to_vec();
        oversize.extend(vec![b'x'; MAX_UPLOAD_BYTES + 1]);
        oversize.extend(b"\r\n--x--\r\n");
        assert_eq!(
            router(state)
                .oneshot(request(oversize))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn fanout_has_monotonic_sequence_ids() {
        let s = session(Box::new(FakeSession {
            aborted: Arc::new(AtomicBool::new(false)),
            sends: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            abort_fails: false,
        }));
        let mut a = s.tx.subscribe();
        let mut b = s.tx.subscribe();
        s.publish(ChatEvent::Delta {
            role: ChatRole::Assistant,
            text: "one".into(),
        });
        s.publish(ChatEvent::Delta {
            role: ChatRole::Assistant,
            text: "two".into(),
        });
        assert_eq!(
            (a.recv().await.unwrap().seq, a.recv().await.unwrap().seq),
            (1, 2)
        );
        assert_eq!(
            (b.recv().await.unwrap().seq, b.recv().await.unwrap().seq),
            (1, 2)
        );
    }

    #[tokio::test]
    async fn renderer_sequence_preserves_roles_tools_errors_and_abort() {
        let s = session(Box::new(FakeSession {
            aborted: Arc::new(AtomicBool::new(false)),
            sends: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            abort_fails: false,
        }));
        let mut events = s.tx.subscribe();
        s.active_turns.store(1, Ordering::SeqCst);
        for event in [
            ChatEvent::Delta {
                role: ChatRole::Thinking,
                text: "considering ".into(),
            },
            ChatEvent::Delta {
                role: ChatRole::Thinking,
                text: "options".into(),
            },
            ChatEvent::Delta {
                role: ChatRole::Assistant,
                text: "* answer".into(),
            },
            ChatEvent::ToolCall {
                id: "call-1".into(),
                name: "shell".into(),
                args: r#"{\"command\":\"pwd\"}"#.into(),
            },
            ChatEvent::ToolOutput {
                id: "call-1".into(),
                text: "partial".into(),
                is_error: false,
                done: false,
            },
            ChatEvent::ToolOutput {
                id: "call-1".into(),
                text: "complete".into(),
                is_error: true,
                done: true,
            },
            ChatEvent::Error("backend warning".into()),
            ChatEvent::TurnFinished {
                ok: false,
                error: Some("aborted".into()),
            },
        ] {
            s.publish(event);
        }
        let events: Vec<_> = (0..8).map(|_| events.try_recv().unwrap()).collect();
        assert_eq!(
            events.iter().map(|event| event.seq).collect::<Vec<_>>(),
            (1..=8).collect::<Vec<_>>()
        );
        assert_eq!(events[0].kind, "thinking");
        assert_eq!(events[2].kind, "delta");
        assert_eq!(events[3].name.as_deref(), Some("shell"));
        assert_eq!(events[4].text.as_deref(), Some("partial"));
        assert_eq!(events[5].text.as_deref(), Some("complete"));
        assert_eq!(events[5].is_error, Some(true));
        assert_eq!(events[5].done, Some(true));
        assert_eq!(events[6].error.as_deref(), Some("backend warning"));
        assert_eq!(events[7].kind, "aborted");
        assert_eq!(events[7].error.as_deref(), Some("aborted"));
    }

    #[tokio::test]
    async fn stale_backend_terminal_event_cannot_finish_a_new_turn() {
        let s = session(Box::new(FakeSession {
            aborted: Arc::new(AtomicBool::new(false)),
            sends: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            abort_fails: false,
        }));
        s.generation.store(2, Ordering::SeqCst);
        s.active_turns.store(1, Ordering::SeqCst);

        s.publish_if_current(
            1,
            ChatEvent::TurnFinished {
                ok: false,
                error: Some("old backend".into()),
            },
        );

        assert_eq!(s.active_turns.load(Ordering::SeqCst), 1);
        assert!(s.history.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn autonomous_start_and_finish_preserve_submitted_active_turn() {
        let s = session(Box::new(FakeSession {
            aborted: Arc::new(AtomicBool::new(false)),
            sends: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            abort_fails: false,
        }));
        s.active_turns.store(1, Ordering::SeqCst);
        let generation = s.generation.load(Ordering::SeqCst);
        let mut wire = s.tx.subscribe();

        assert!(s.publish_if_current(
            generation,
            ChatEvent::TurnStarted {
                origin: TurnOrigin::Autonomous,
            },
        ));
        assert_eq!(s.active_turns.load(Ordering::SeqCst), 2);
        let started = wire.recv().await.unwrap();
        assert_eq!(started.kind, "started");
        assert_eq!(started.origin.as_deref(), Some("autonomous"));

        assert!(s.publish_if_current(
            generation,
            ChatEvent::TurnFinished {
                ok: false,
                error: Some("autonomous failed".into()),
            },
        ));
        assert_eq!(s.active_turns.load(Ordering::SeqCst), 1);
        assert_eq!(wire.recv().await.unwrap().kind, "aborted");
    }

    #[tokio::test]
    async fn abort_is_server_authoritative_and_emits_terminal_event() {
        let flag = Arc::new(AtomicBool::new(false));
        let sends = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let s = session(Box::new(FakeSession {
            aborted: Arc::clone(&flag),
            sends: Arc::clone(&sends),
            abort_fails: false,
        }));
        let state = Arc::new(AppState {
            config: Config::default(),
            root: test_root(),
            start: std::sync::OnceLock::new(),
            sessions: Mutex::new(HashMap::from([("test".into(), Arc::clone(&s))])),
            live_id: Mutex::new(None),
            transition: Mutex::new(()),
            meta: Mutex::new(HashMap::new()),
        });
        let mut events = s.tx.subscribe();
        s.active_turns.store(1, Ordering::SeqCst);
        assert_eq!(
            abort(Path("test".into()), State(state))
                .await
                .into_response()
                .status(),
            StatusCode::ACCEPTED
        );
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap();
        tokio::task::yield_now().await;
        assert!(flag.load(Ordering::SeqCst));
        assert_eq!(event.kind, "aborted");
        assert_eq!(event.error.as_deref(), Some("aborted"));
        *s.inner.lock().await = Some(Box::new(FakeSession {
            aborted: Arc::new(AtomicBool::new(false)),
            sends: Arc::clone(&sends),
            abort_fails: false,
        }));
        s.inner
            .lock()
            .await
            .as_mut()
            .unwrap()
            .send_turn("next turn".into())
            .await
            .unwrap();
        assert_eq!(sends.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn abort_still_terminates_when_backend_abort_fails() {
        let s = session(Box::new(FakeSession {
            aborted: Arc::new(AtomicBool::new(false)),
            sends: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            abort_fails: true,
        }));
        let state = Arc::new(AppState {
            config: Config::default(),
            root: test_root(),
            start: std::sync::OnceLock::new(),
            sessions: Mutex::new(HashMap::from([("test".into(), Arc::clone(&s))])),
            live_id: Mutex::new(None),
            transition: Mutex::new(()),
            meta: Mutex::new(HashMap::new()),
        });
        let mut events = s.tx.subscribe();
        s.active_turns.store(1, Ordering::SeqCst);

        assert_eq!(
            abort(Path("test".into()), State(state))
                .await
                .into_response()
                .status(),
            StatusCode::ACCEPTED
        );
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.kind, "aborted");
        assert_eq!(s.active_turns.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn abort_terminalizes_every_accepted_turn() {
        let s = session(Box::new(FakeSession {
            aborted: Arc::new(AtomicBool::new(false)),
            sends: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            abort_fails: false,
        }));
        let state = Arc::new(AppState {
            config: Config::default(),
            root: test_root(),
            start: std::sync::OnceLock::new(),
            sessions: Mutex::new(HashMap::from([("test".into(), Arc::clone(&s))])),
            live_id: Mutex::new(None),
            transition: Mutex::new(()),
            meta: Mutex::new(HashMap::new()),
        });
        let mut events = s.tx.subscribe();
        s.active_turns.store(2, Ordering::SeqCst);

        assert_eq!(
            abort(Path("test".into()), State(state))
                .await
                .into_response()
                .status(),
            StatusCode::ACCEPTED
        );
        assert_eq!(events.recv().await.unwrap().kind, "aborted");
        assert_eq!(events.recv().await.unwrap().kind, "aborted");
        assert_eq!(s.active_turns.load(Ordering::SeqCst), 0);
        assert!(!s.abort_requested.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn abort_terminalizes_before_a_hung_backend_abort() {
        let s = session(Box::new(HangingAbortSession));
        let state = Arc::new(AppState {
            config: Config::default(),
            root: test_root(),
            start: std::sync::OnceLock::new(),
            sessions: Mutex::new(HashMap::from([("test".into(), Arc::clone(&s))])),
            live_id: Mutex::new(None),
            transition: Mutex::new(()),
            meta: Mutex::new(HashMap::new()),
        });
        let mut events = s.tx.subscribe();
        s.active_turns.store(1, Ordering::SeqCst);

        assert_eq!(
            abort(Path("test".into()), State(state))
                .await
                .into_response()
                .status(),
            StatusCode::ACCEPTED
        );
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_millis(50), events.recv())
                .await
                .unwrap()
                .unwrap()
                .kind,
            "aborted"
        );
        assert_eq!(s.active_turns.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn http_stream_fans_out_and_late_joiner_replays_identically() {
        let opens = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut services = dar_extension_sdk::ServiceRegistry::default();
        services
            .register::<dyn ChatBackend>(
                "fake",
                Arc::new(StreamingBackend {
                    opens: Arc::clone(&opens),
                }),
            )
            .unwrap();
        let state = Arc::new(AppState {
            config: Config {
                backend: Some("fake".into()),
                ..Config::default()
            },
            root: test_root(),
            start: std::sync::OnceLock::from(start_ctx(services)),
            sessions: Mutex::new(HashMap::new()),
            live_id: Mutex::new(None),
            transition: Mutex::new(()),
            meta: Mutex::new(HashMap::new()),
        });
        let app = router(Arc::clone(&state));
        let stream_a = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/test/stream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let stream_b = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/test/stream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(stream_a.status(), StatusCode::OK);
        assert_eq!(stream_b.status(), StatusCode::OK);

        let send_request = |command_id: &str| {
            axum::http::Request::builder()
                .method("POST")
                .uri("/test/send")
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"command_id":"{command_id}","message":"hello"}}"#
                )))
                .unwrap()
        };
        assert_eq!(
            app.clone()
                .oneshot(send_request("one"))
                .await
                .unwrap()
                .status(),
            StatusCode::ACCEPTED
        );
        assert_eq!(
            app.clone()
                .oneshot(send_request("one"))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );

        let mut body_a = stream_a.into_body();
        let mut body_b = stream_b.into_body();
        let mut concurrent = Vec::new();
        for body in [&mut body_a, &mut body_b] {
            let mut event = String::new();
            for _ in 0..2 {
                let frame = tokio::time::timeout(std::time::Duration::from_secs(1), body.frame())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap()
                    .into_data()
                    .unwrap();
                event.push_str(core::str::from_utf8(&frame).unwrap());
            }
            assert!(event.contains("id: 1"));
            assert!(event.contains("reply"));
            concurrent.push(event);
        }
        assert_eq!(concurrent[0], concurrent[1]);

        let late = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/test/stream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let mut late = late.into_body();
        let mut replay = String::new();
        for _ in 0..2 {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(1), late.frame())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .into_data()
                .unwrap();
            replay.push_str(core::str::from_utf8(&frame).unwrap());
        }
        assert_eq!(concurrent[0], replay);

        assert_eq!(
            app.clone()
                .oneshot(
                    axum::http::Request::builder()
                        .method("POST")
                        .uri("/test/abort")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
                .status(),
            StatusCode::ACCEPTED
        );
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(1), body_a.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap();
        let terminal = String::from_utf8(terminal.to_vec()).unwrap();
        assert!(terminal.contains("aborted"));
        assert_eq!(
            app.oneshot(send_request("two")).await.unwrap().status(),
            StatusCode::ACCEPTED
        );
        let mut reused = String::new();
        for _ in 0..2 {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(1), body_a.frame())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .into_data()
                .unwrap();
            reused.push_str(core::str::from_utf8(&frame).unwrap());
        }
        assert!(reused.contains("reply"));
        assert_eq!(opens.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn http_stream_cannot_miss_an_event_during_subscription() {
        let state = Arc::new(AppState {
            config: Config::default(),
            root: test_root(),
            start: std::sync::OnceLock::new(),
            sessions: Mutex::new(HashMap::new()),
            live_id: Mutex::new(None),
            transition: Mutex::new(()),
            meta: Mutex::new(HashMap::new()),
        });
        let session = state.session("test").await.unwrap();
        let (sent_tx, sent_rx) = std::sync::mpsc::channel();
        let (proceed_tx, proceed_rx) = std::sync::mpsc::channel();
        let (subscribed_tx, subscribed_rx) = std::sync::mpsc::channel();
        let (snapshot_tx, snapshot_rx) = std::sync::mpsc::channel();
        let (stream_proceed_tx, stream_proceed_rx) = std::sync::mpsc::channel();
        *session.pause_after_send.lock().unwrap() = Some(Arc::new(PublishPause {
            sent: sent_tx,
            proceed: std::sync::Mutex::new(proceed_rx),
        }));
        *session.pause_after_subscribe.lock().unwrap() = Some(Arc::new(StreamPause {
            subscribed: subscribed_tx,
            snapshot_done: snapshot_tx,
            proceed: std::sync::Mutex::new(stream_proceed_rx),
        }));
        let publisher = {
            let session = Arc::clone(&session);
            tokio::task::spawn_blocking(move || {
                session.publish(ChatEvent::Delta {
                    role: ChatRole::Assistant,
                    text: "during subscribe".into(),
                });
            })
        };
        sent_rx.recv().unwrap();
        let runtime = tokio::runtime::Handle::current();
        let subscriber = tokio::task::spawn_blocking(move || {
            runtime.block_on(async move {
                router(state)
                    .oneshot(
                        axum::http::Request::builder()
                            .uri("/test/stream")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap()
            })
        });
        subscribed_rx.recv().unwrap();
        stream_proceed_tx.send(()).unwrap();
        snapshot_rx.recv().unwrap();
        proceed_tx.send(()).unwrap();
        publisher.await.unwrap();
        let response = subscriber.await.unwrap();
        let frame = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            response.into_body().frame(),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .into_data()
        .unwrap();
        let event = String::from_utf8(frame.to_vec()).unwrap();
        assert!(event.contains("id: 1"));
        assert!(event.contains("during subscribe"));
    }

    #[tokio::test]
    async fn stale_last_event_id_replays_durable_tail_after_restart() {
        let root = test_root();
        let state = Arc::new(AppState {
            config: Config::default(),
            root: root.clone(),
            start: std::sync::OnceLock::new(),
            sessions: Mutex::new(HashMap::new()),
            live_id: Mutex::new(None),
            transition: Mutex::new(()),
            meta: Mutex::new(HashMap::new()),
        });
        let session = state.session("resume").await.unwrap();
        session.publish_user("first".into(), vec![]).unwrap();
        session.publish(ChatEvent::Delta {
            role: ChatRole::Assistant,
            text: "second".into(),
        });
        let restarted = Arc::new(AppState {
            config: Config::default(),
            root,
            start: std::sync::OnceLock::new(),
            sessions: Mutex::new(HashMap::new()),
            live_id: Mutex::new(None),
            transition: Mutex::new(()),
            meta: Mutex::new(HashMap::new()),
        });
        let mut headers = HeaderMap::new();
        headers.insert("last-event-id", "0".parse().unwrap());
        let response = stream(Path("resume".into()), State(restarted), headers)
            .await
            .into_response();
        let mut body = response.into_body();
        let mut replay = String::new();
        for _ in 0..2 {
            replay.push_str(
                core::str::from_utf8(&body.frame().await.unwrap().unwrap().into_data().unwrap())
                    .unwrap(),
            );
        }
        assert!(
            replay.contains("id: 1")
                && replay.contains("id: 2")
                && replay.contains("first")
                && replay.contains("second")
        );
    }

    #[tokio::test]
    async fn history_renders_persisted_transcript() {
        let state = Arc::new(AppState {
            config: Config::default(),
            root: test_root(),
            start: std::sync::OnceLock::new(),
            sessions: Mutex::new(HashMap::new()),
            live_id: Mutex::new(None),
            transition: Mutex::new(()),
            meta: Mutex::new(HashMap::new()),
        });
        state
            .session("history")
            .await
            .unwrap()
            .publish_user("saved <message>".into(), vec![])
            .unwrap();
        let body = history(Path("history".into()), State(state))
            .await
            .into_response()
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("renderChatEvent") && html.contains("saved \\u003cmessage\\u003e"));
    }

    #[tokio::test]
    async fn history_page_is_not_prefix_aware_but_other_routes_are() {
        let state = Arc::new(AppState {
            config: Config::default(),
            root: test_root(),
            start: std::sync::OnceLock::new(),
            sessions: Mutex::new(HashMap::new()),
            live_id: Mutex::new(None),
            transition: Mutex::new(()),
            meta: Mutex::new(HashMap::new()),
        });
        let get = |uri: &str| {
            axum::http::Request::builder()
                .uri(uri)
                .body(Body::empty())
                .unwrap()
        };
        let app = router(state);
        let index = app.clone().oneshot(get("/")).await.unwrap();
        assert_eq!(index.headers()["x-prefix-aware"], "1");
        // The standalone history page has no shell shim; it must stay
        // un-marked so the fleet proxy's compat rewriter prefixes its URLs.
        let history = app.oneshot(get("/main/history")).await.unwrap();
        assert!(history.headers().get("x-prefix-aware").is_none());
    }

    #[tokio::test]
    async fn lagged_subscriber_recovers_from_transcript() {
        let state = Arc::new(AppState {
            config: Config::default(),
            root: test_root(),
            start: std::sync::OnceLock::new(),
            sessions: Mutex::new(HashMap::new()),
            live_id: Mutex::new(None),
            transition: Mutex::new(()),
            meta: Mutex::new(HashMap::new()),
        });
        let session = state.session("lag").await.unwrap();
        let response = stream(
            Path("lag".into()),
            State(Arc::clone(&state)),
            HeaderMap::new(),
        )
        .await
        .into_response();
        for number in 1..=300 {
            session.publish(ChatEvent::Delta {
                role: ChatRole::Assistant,
                text: number.to_string(),
            });
        }
        let mut body = response.into_body();
        let first = String::from_utf8(
            body.frame()
                .await
                .unwrap()
                .unwrap()
                .into_data()
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(first.contains("id: 1") && first.contains("\"text\":\"1\""));
    }

    #[test]
    fn tab_fragment_has_a_usable_composer() {
        let tab = ChatTab {
            agent_name: "Test Agent".into(),
            agent_description: Some("Does <things>".into()),
            agent_avatar: None,
        };
        let html = tab.render().unwrap();
        assert!(!html.contains("class=\"chat-avatar\""));
        assert!(html.contains("id=\"chat-hero-line\""));
        assert!(html.contains("id=\"chat-composer\""));
        assert!(html.contains("<small class=\"chat-desc\">Does &lt;things&gt;</small>"));
        // Belt-and-braces: even if the JS singleton fails to attach, the inline
        // handler blocks a native submit / full-page reload.
        assert!(html.contains("onsubmit=\"event.preventDefault()\""));
        assert!(html.contains("data-agent-name="));
        assert!(html.contains("id=\"chat-input\"") && html.contains("<textarea"));
        assert!(html.contains("id=\"chat-attachments\"") && html.contains("hidden"));
        assert!(html.contains("id=\"chat-attach\""));
        // Drag-and-drop overlay + cap-exceeded hint (ALG-411).
        assert!(html.contains("id=\"chat-dropzone\"") && html.contains("Drop files to attach"));
        let dropzone_pos = html.find("id=\"chat-dropzone\"").unwrap();
        let dropzone_tag_end = dropzone_pos + html[dropzone_pos..].find('>').unwrap();
        assert!(html[dropzone_pos..dropzone_tag_end].contains("hidden"));
        assert!(html.contains("id=\"chat-cap-hint\""));
        assert!(html.contains(".chat-dropzone") && html.contains(".chat-dropzone[hidden]"));
        assert!(html.contains(".chat-cap-hint"));
        assert!(html.contains(".chat-web {\n  position: relative"));
        assert!(html.contains("id=\"chat-send\""));
        // Guards against the renderer losing the fleet-proxy prefix read.
        assert!(html.contains("window.__dashPrefix"));
        let abort_pos = html.find("id=\"chat-abort\"").unwrap();
        let abort_tag_end = abort_pos + html[abort_pos..].find('>').unwrap();
        assert!(html[abort_pos..abort_tag_end].contains("hidden"));
        assert!(!html.contains("chat-compact"));
        assert!(!html.contains(">Compact<"));
        assert!(html.contains("@media (max-width: 760px)"));
        assert!(html.contains("overflow-wrap: anywhere"));
        // the pending-reply loader needs its dots animation styled
        assert!(html.contains("animation: chat-pulse"));
        assert!(!html.contains("\\\"chat-composer\\\""));
        // The self-refreshing tab owns its own EventSource + JS lifecycle.
        assert!(tab.self_refreshing());
        assert!(tab.passive_default());
        for marker in [
            "case 'thinking'",
            "case 'tool_call'",
            "case 'tool_output'",
            "case 'aborted'",
            "<strong>",
            "<pre><code",
            "<ul>",
        ] {
            assert!(html.contains(marker), "renderer contains {marker}");
        }
    }

    #[tokio::test]
    async fn new_endpoint_resets_transcript_and_persists_reset() {
        let sends = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let s = session(Box::new(FakeSession {
            aborted: Arc::new(AtomicBool::new(false)),
            sends: Arc::clone(&sends),
            abort_fails: false,
        }));
        s.publish_user("hi".into(), vec![]).unwrap();
        let user_seq = load_transcript(&s.transcript).unwrap().back().unwrap().seq;
        let state = Arc::new(AppState {
            config: Config::default(),
            root: test_root(),
            start: std::sync::OnceLock::new(),
            sessions: Mutex::new(HashMap::from([("test".into(), Arc::clone(&s))])),
            live_id: Mutex::new(None),
            transition: Mutex::new(()),
            meta: Mutex::new(HashMap::new()),
        });

        assert_eq!(
            router(state)
                .oneshot(
                    axum::http::Request::builder()
                        .method("POST")
                        .uri("/test/new")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
                .status(),
            StatusCode::ACCEPTED
        );

        let events = load_transcript(&s.transcript).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "reset");
        assert!(events[0].seq > user_seq);
    }

    #[tokio::test]
    async fn publish_maps_question_events() {
        let s = session(Box::new(FakeSession {
            aborted: Arc::new(AtomicBool::new(false)),
            sends: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            abort_fails: false,
        }));
        s.publish(ChatEvent::QuestionAsked {
            request_id: "req-1".into(),
            questions: vec![QuestionInfo {
                header: "Pick".into(),
                question: "Which?".into(),
                options: vec![cap_chat::QuestionOption {
                    label: "A".into(),
                    description: "first".into(),
                }],
                multiple: false,
                custom: true,
            }],
        });
        s.publish(ChatEvent::QuestionResolved {
            request_id: "req-1".into(),
            answers: vec![vec!["A".into()]],
            rejected: false,
        });

        let events = load_transcript(&s.transcript).unwrap();
        assert_eq!(events[0].kind, "question");
        assert_eq!(events[0].id.as_deref(), Some("req-1"));
        assert_eq!(
            events[0].questions.as_ref().unwrap()[0].options[0].label,
            "A"
        );
        assert_eq!(events[1].kind, "question_done");
        assert_eq!(events[1].text.as_deref(), Some("A"));
        assert_eq!(events[1].is_error, Some(false));

        let wire = serde_json::to_string(&events[0]).unwrap();
        assert!(wire.contains(r#""type":"question""#));
        assert!(wire.contains(r#""questions":[{"header":"Pick""#));
    }

    #[tokio::test]
    async fn answer_route_forwards_to_backend_and_409s_without_session() {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let s = session(Box::new(AnsweringSession {
            calls: Arc::clone(&calls),
            fails: false,
        }));
        let state = Arc::new(AppState {
            config: Config::default(),
            root: test_root(),
            start: std::sync::OnceLock::new(),
            sessions: Mutex::new(HashMap::from([("test".into(), Arc::clone(&s))])),
            live_id: Mutex::new(None),
            transition: Mutex::new(()),
            meta: Mutex::new(HashMap::new()),
        });
        let request = || {
            axum::http::Request::builder()
                .method("POST")
                .uri("/test/answer")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"request_id":"req-1","answers":[["A"]]}"#))
                .unwrap()
        };

        assert_eq!(
            router(Arc::clone(&state))
                .oneshot(request())
                .await
                .unwrap()
                .status(),
            StatusCode::ACCEPTED
        );
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            [("req-1".to_owned(), vec![vec!["A".to_owned()]])]
        );

        *s.inner.lock().await = None;
        assert_eq!(
            router(state).oneshot(request()).await.unwrap().status(),
            StatusCode::CONFLICT
        );
    }

    #[tokio::test]
    async fn answer_route_conflicts_on_backend_error() {
        let s = session(Box::new(RejectingSession));
        let state = Arc::new(AppState {
            config: Config::default(),
            root: test_root(),
            start: std::sync::OnceLock::new(),
            sessions: Mutex::new(HashMap::from([("test".into(), Arc::clone(&s))])),
            live_id: Mutex::new(None),
            transition: Mutex::new(()),
            meta: Mutex::new(HashMap::new()),
        });

        let response = router(state)
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/test/answer")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"request_id":"req-1","answers":[["A"]]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("does not support"));
    }

    #[test]
    fn attachment_prompt_paths_include_the_session_segment() {
        // Uploads land at data/chat/uploads/{session}/{command}/{file}; the
        // path told to the agent must match or every read ends in ENOENT.
        let attachments = vec![Attachment {
            name: "logo.png".into(),
            url: "/chat/main/attachment/upload-1/0-logo.png".into(),
            image: true,
        }];
        let prompt = attachment_prompt("look", &attachments, std::path::Path::new("/agent"));
        assert!(
            prompt.contains("/agent/data/chat/uploads/main/upload-1/0-logo.png"),
            "{prompt}"
        );
    }

    #[test]
    fn agent_avatar_accepts_emoji_url_and_contained_image() {
        let root = test_root();
        fs::create_dir_all(root.join("assets")).unwrap();
        fs::write(root.join("assets/me.png"), b"png").unwrap();
        let avatar_for = |value: &str| {
            fs::write(root.join("agent.yaml"), format!("avatar: \"{value}\"\n")).unwrap();
            agent_avatar(&root)
        };
        assert_eq!(avatar_for("🦉"), Some(Avatar::Text("🦉".into())));
        assert_eq!(
            avatar_for("https://x.test/a.png"),
            Some(Avatar::Url("https://x.test/a.png".into()))
        );
        assert_eq!(
            avatar_for("assets/me.png"),
            Some(Avatar::File(
                root.join("assets/me.png").canonicalize().unwrap()
            ))
        );
        assert_eq!(avatar_for("assets/missing.png"), None);
        assert_eq!(avatar_for("../../etc/hosts"), None);
        assert_eq!(avatar_for(""), None);
        fs::remove_file(root.join("agent.yaml")).unwrap();
        assert_eq!(agent_avatar(&root), None);

        let tab = |avatar| ChatTab {
            agent_name: "Owl".into(),
            agent_description: None,
            agent_avatar: Some(avatar),
        };
        let html = tab(Avatar::Text("🦉".into())).render().unwrap();
        assert!(html.contains(r#"<span class="chat-avatar" aria-hidden="true">🦉</span>"#));
        assert!(html.contains(r#"<span class="chat-hero-avatar" aria-hidden="true">🦉</span>"#));
        let html = tab(Avatar::File(root.join("assets/me.png")))
            .render()
            .unwrap();
        assert!(html.contains(r#"<img data-avatar-src="/chat/avatar" alt="Owl">"#));
    }

    #[tokio::test]
    async fn avatar_route_serves_only_the_configured_image() {
        let root = test_root();
        fs::create_dir_all(&root).unwrap();
        let state = Arc::new(AppState {
            config: Config::default(),
            root: root.clone(),
            start: std::sync::OnceLock::new(),
            sessions: Mutex::new(HashMap::new()),
            live_id: Mutex::new(None),
            transition: Mutex::new(()),
            meta: Mutex::new(HashMap::new()),
        });
        let get = || async {
            router(Arc::clone(&state))
                .oneshot(
                    axum::http::Request::builder()
                        .uri("/avatar")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
        };
        fs::write(root.join("agent.yaml"), "avatar: \"🦉\"\n").unwrap();
        assert_eq!(get().await.status(), StatusCode::NOT_FOUND);

        fs::write(root.join("me.svg"), "<svg/>").unwrap();
        fs::write(root.join("agent.yaml"), "avatar: me.svg\n").unwrap();
        let response = get().await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/svg+xml");
        assert!(response.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("sandbox"));
    }

    #[test]
    fn agent_display_name_falls_back() {
        let named = test_root();
        fs::create_dir_all(&named).unwrap();
        fs::write(named.join("agent.yaml"), "name: Twc\n").unwrap();
        assert_eq!(agent_display_name(&named), "Twc");

        let id_only = test_root();
        fs::create_dir_all(&id_only).unwrap();
        fs::write(id_only.join("agent.yaml"), "id: twc\n").unwrap();
        assert_eq!(agent_display_name(&id_only), "twc");

        let missing = test_root();
        assert_eq!(agent_display_name(&missing), "Agent");
    }

    #[tokio::test]
    async fn history_routes_list_and_page_archived_sessions() {
        let root = test_root();
        let dir = root.join("data/chat/sessions");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("2026-01-01_a.jsonl"), "{\"type\":\"session\",\"id\":\"past\"}\n{\"type\":\"message_end\",\"message\":{\"role\":\"user\",\"content\":\"remember this\"}}\n").unwrap();
        let state = Arc::new(AppState {
            config: Config::default(),
            root,
            start: std::sync::OnceLock::new(),
            sessions: Mutex::new(HashMap::new()),
            live_id: Mutex::new(None),
            transition: Mutex::new(()),
            meta: Mutex::new(HashMap::new()),
        });
        let app = router(state);
        let index = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/sessions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(index.status(), StatusCode::OK);
        let transcript = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/sessions/past?count=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(transcript.status(), StatusCode::OK);
        let bytes = transcript.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8(bytes.to_vec())
            .unwrap()
            .contains("remember this"));
    }

    #[tokio::test]
    async fn fresh_turn_title_resolution_does_not_deadlock_live_id() {
        let root = test_root();
        let dir = root.join("data/chat/sessions");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("backend.jsonl"),
            "{\"type\":\"session\",\"id\":\"backend\",\"backend\":\"fake\"}\n",
        )
        .unwrap();
        let mut services = dar_extension_sdk::ServiceRegistry::default();
        services
            .register::<dyn ChatBackend>(
                "fake",
                Arc::new(TitlerBackend {
                    mode: TitlerMode::Success,
                    closed: Arc::new(AtomicBool::new(false)),
                }),
            )
            .unwrap();
        let web = session(Box::new(RejectingSession));
        let state = Arc::new(AppState {
            config: Config {
                backend: Some("fake".into()),
                ..Config::default()
            },
            root,
            start: std::sync::OnceLock::from(start_ctx(services)),
            sessions: Mutex::new(HashMap::new()),
            live_id: Mutex::new(None),
            meta: Mutex::new(HashMap::new()),
            transition: Mutex::new(()),
        });
        web.opened_after_ms.store(0, Ordering::SeqCst);
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            state.finish_turn(web, 0, "hello".into(), "reply".into()),
        )
        .await
        .expect("title resolution must not deadlock");
        assert_eq!(state.live_id.lock().await.as_deref(), Some("backend"));
    }

    #[tokio::test]
    async fn titler_success_timeout_and_failure_are_isolated_and_cleaned() {
        for mode in [
            TitlerMode::Success,
            TitlerMode::Timeout,
            TitlerMode::Failure,
        ] {
            let root = test_root();
            let closed = Arc::new(AtomicBool::new(false));
            let mut services = dar_extension_sdk::ServiceRegistry::default();
            services
                .register::<dyn ChatBackend>(
                    "fake",
                    Arc::new(TitlerBackend {
                        mode,
                        closed: Arc::clone(&closed),
                    }),
                )
                .unwrap();
            let web = session(Box::new(RejectingSession));
            let state = AppState {
                config: Config {
                    backend: Some("fake".into()),
                    ..Config::default()
                },
                root: root.clone(),
                start: std::sync::OnceLock::from(start_ctx(services)),
                sessions: Mutex::new(HashMap::new()),
                live_id: Mutex::new(Some("live".into())),
                meta: Mutex::new(HashMap::new()),
                transition: Mutex::new(()),
            };
            state
                .finish_turn(web, 0, "hello".into(), "reply".into())
                .await;
            assert!(closed.load(Ordering::SeqCst), "titler session must close");
            assert!(
                !root
                    .join("data/chat/.titler")
                    .read_dir()
                    .is_ok_and(|mut entries| entries.next().is_some()),
                "titler temp directory must be empty"
            );
            assert!(
                chat::archive::list(&root.join("data/chat/sessions")).is_empty(),
                "titler must not create archive entries"
            );
            assert_eq!(
                state
                    .meta
                    .lock()
                    .await
                    .get("live")
                    .and_then(|m| m.title.as_deref()),
                matches!(mode, TitlerMode::Success).then_some("Useful title")
            );
        }
    }

    #[test]
    fn renderer_source_is_re_execution_safe_under_node() {
        // The fragment is re-spliced whenever the Chat tab is (re)activated, so
        // the script must evaluate any number of times with no top-level
        // redeclaration (e.g. `const esc` collisions) and no throw.
        let renderer = format!("{}/src/renderer.js", env!("CARGO_MANIFEST_DIR"));
        let script = r#"const fs=require('fs');const src=fs.readFileSync(process.argv[1],'utf8');eval(src);eval(src);"#;
        let status = std::process::Command::new("node")
            .args(["-e", script, &renderer])
            .status()
            .expect("node is available for browser renderer tests");
        assert!(status.success());
    }

    #[test]
    fn browser_renderer_restores_draft_after_rejected_send() {
        let renderer = format!("{}/src/renderer.js", env!("CARGO_MANIFEST_DIR"));
        let script = r#"const handlers={},style={setProperty(){}};const element=(id)=>({id,style:{...style},dataset:{},value:'',disabled:false,hidden:false,innerHTML:'',scrollHeight:10,scrollTop:0,clientHeight:10,getBoundingClientRect:()=>({top:0}),querySelectorAll:()=>[]});const elements=Object.fromEntries(['chat-root','chat-transcript','chat-input','chat-chips','chat-send','chat-abort','chat-token-meter'].map(id=>[id,element(id)]));elements['chat-root'].dataset.agentName='Agent';global.document={getElementById:id=>elements[id]||null,addEventListener:(type,fn)=>handlers[type]=fn};global.window={innerHeight:1000,addEventListener(){}};global.EventSource=function(){};global.crypto={randomUUID:()=> 'command-id'};global.fetch=async()=>({ok:false,status:409,text:async()=>JSON.stringify({error:'backend rejected turn'})});require(process.argv[1]);const app=window.__chatWeb,file={name:'notes.txt'};app.pending=[file];elements['chat-input'].value='keep this';handlers.input({target:elements['chat-input']});handlers.submit({target:{id:'chat-composer'},preventDefault(){}});setTimeout(()=>{if(app.draft!=='keep this'||elements['chat-input'].value!=='keep this'||app.pending[0]!==file||app.turns!==0||elements['chat-send'].disabled||!elements['chat-transcript'].innerHTML.includes('backend rejected turn'))process.exit(1)},0);"#;
        let status = std::process::Command::new("node")
            .args(["-e", script, &renderer])
            .status()
            .expect("node is available for browser renderer tests");
        assert!(status.success());
    }

    #[test]
    fn browser_resume_replay_does_not_set_busy_state() {
        let renderer = format!("{}/src/renderer.js", env!("CARGO_MANIFEST_DIR"));
        let script = r#"const handlers={},el=id=>({id,dataset:{},style:{setProperty(){}},value:'',disabled:false,hidden:false,innerHTML:'',scrollHeight:1,scrollTop:0,clientHeight:1,getBoundingClientRect:()=>({top:0}),querySelectorAll:()=>[]});const es=Object.fromEntries(['chat-root','chat-transcript','chat-input','chat-chips','chat-send','chat-abort','chat-token-meter','chat-context-warning'].map(id=>[id,el(id)]));global.document={hidden:false,getElementById:id=>es[id]||null,addEventListener:(t,f)=>handlers[t]=f};global.window={innerHeight:1000,addEventListener(){}};global.EventSource=function(){};global.crypto={randomUUID:()=> 'x'};require(process.argv[1]);window.renderChatEvent({type:'user',text:'old',historical:true});window.renderChatEvent({type:'delta',text:'answer',historical:true});setTimeout(()=>{if(window.__chatWeb.turns!==0||!es['chat-abort'].disabled)process.exit(1)},0);"#;
        assert!(std::process::Command::new("node")
            .args(["-e", script, &renderer])
            .status()
            .unwrap()
            .success());
    }

    #[test]
    fn browser_renderer_handles_the_representative_event_sequence() {
        let renderer = format!("{}/src/renderer.js", env!("CARGO_MANIFEST_DIR"));
        let script = r#"const r=require(process.argv[1]);let b=[];for(const e of [{type:'thinking',text:'plan '},{type:'thinking',text:'it'},{type:'delta',text:'* **answer**\n```txt\n**code**\n```'},{type:'tool_call',id:'x',name:'shell',args:'{}'},{type:'tool_output',id:'x',text:'partial'},{type:'tool_output',id:'x',text:'failed',is_error:true,done:true},{type:'error',error:'warning'},{type:'aborted',error:'aborted'},{type:'user',text:'run `x --y` now'}])b=r.reduce(b,e);let h=r.html(b);if(b.length!==6||b[0].text!=='plan it'||(h.match(/data-tool-id=/g)||[]).length!==1||!h.includes('failed')||!h.includes('is-error is-done')||!h.includes('<ul><li><strong>answer</strong></li></ul>')||!h.includes('<pre><code data-language="txt">**code**\n</code></pre>')||!h.includes('warning')||!h.includes('Interrupted')||!h.includes('<code>x --y</code>')||r.usageText({tokens_used:12,context_window:100})!=='12 / 100 tokens'||r.usageText({tokens_used:12})!=='12 tokens')process.exit(1);"#;
        let status = std::process::Command::new("node")
            .args(["-e", script, &renderer])
            .status()
            .expect("node is available for browser renderer tests");
        assert!(status.success());
    }

    #[test]
    fn browser_renderer_uses_gfm_tables_and_sanitizes() {
        let renderer = format!("{}/src/renderer.js", env!("CARGO_MANIFEST_DIR"));
        let marked = format!("{}/src/vendor/marked.min.js", env!("CARGO_MANIFEST_DIR"));
        let script = r#"global.marked=require(process.argv[2]);global.DOMPurify={sanitize:s=>s};const r=require(process.argv[1]);let h=r.markdown('| A | B |\n|---|---|\n| 1 | 2 |');if(!h.includes('<table>')||!h.includes('<td>1</td>'))process.exit(1);h=r.markdown('[<b>text</b> and **bold**](https://example.com)');if(h.includes('<b>text</b>')||!h.includes('&lt;b&gt;text&lt;/b&gt;')||!h.includes('<strong>bold</strong>'))process.exit(1);"#;
        let status = std::process::Command::new("node")
            .args(["-e", script, &renderer, &marked])
            .status()
            .expect("node is available for browser renderer tests");
        assert!(status.success());
    }

    #[test]
    fn browser_renderer_preserves_expanded_details_across_repaint() {
        let renderer = format!("{}/src/renderer.js", env!("CARGO_MANIFEST_DIR"));
        let script = r#"const handlers={},style={setProperty(){}};const element=(id)=>({id,style:{...style},dataset:{},value:'',disabled:false,hidden:false,innerHTML:'',scrollHeight:10,scrollTop:0,clientHeight:10,getBoundingClientRect:()=>({top:0}),querySelectorAll:()=>[]});const primed=[{dataset:{bi:'0'},open:false},{dataset:{bi:'1'},open:false}];const elements=Object.fromEntries(['chat-root','chat-transcript','chat-input','chat-chips','chat-send','chat-abort','chat-token-meter'].map(id=>[id,element(id)]));elements['chat-root'].dataset.agentName='Agent';elements['chat-transcript'].querySelectorAll=sel=>sel==='details[open]'?[{dataset:{bi:'0'},open:true}]:primed;global.document={getElementById:id=>elements[id]||null,addEventListener:(type,fn)=>handlers[type]=fn};global.window={innerHeight:1000,addEventListener(){}};global.EventSource=function(){};global.crypto={randomUUID:()=>'command-id'};const r=require(process.argv[1]);if(!r.html([{kind:'thinking',text:'t'}]).includes('data-bi="0"'))process.exit(1);window.renderChatEvent({type:'thinking',text:'x'});setTimeout(()=>{if(primed[0].open!==true||primed[1].open!==false)process.exit(1)},0);"#;
        let status = std::process::Command::new("node")
            .args(["-e", script, &renderer])
            .status()
            .expect("node is available for browser renderer tests");
        assert!(status.success());
    }

    #[test]
    fn browser_renderer_renders_question_lifecycle() {
        let renderer = format!("{}/src/renderer.js", env!("CARGO_MANIFEST_DIR"));
        let script = r#"const r=require(process.argv[1]);let b=r.reduce([],{type:'question',id:'req-1',questions:[{header:'Pick',question:'Which?',options:[{label:'A',description:'first'},{label:'B',description:''}],multiple:false,custom:false}]});let h=r.html(b);if(!h.includes('chat-q-opt')||!h.includes('data-label="A"')||!h.includes('>question<'))process.exit(1);if(!r.html(b,{},true).includes('disabled'))process.exit(1);b=r.reduce(b,{type:'question_done',id:'req-1',text:'A',is_error:false});h=r.html(b);if(!h.includes('answered')||!h.includes('disabled')||!h.includes('>A<'))process.exit(1);let d=r.reduce(r.reduce([],{type:'question',id:'req-2',questions:[{header:'H',question:'Q',options:[{label:'A'}]}]}),{type:'aborted',error:'aborted'});if(!r.html(d).includes('dismissed'))process.exit(1);let custom=r.reduce([],{type:'question',id:'req-3',questions:[{header:'H',question:'Q',options:[],custom:true}]});if(!r.html(custom,{},true).includes('readonly'))process.exit(1);"#;
        let status = std::process::Command::new("node")
            .args(["-e", script, &renderer])
            .status()
            .expect("node is available for browser renderer tests");
        assert!(status.success());
    }

    #[test]
    fn browser_renderer_posts_answer_on_option_click() {
        let renderer = format!("{}/src/renderer.js", env!("CARGO_MANIFEST_DIR"));
        let script = r#"const handlers={},style={setProperty(){}};const element=(id)=>({id,style:{...style},dataset:{},value:'',disabled:false,hidden:false,innerHTML:'',scrollHeight:10,scrollTop:0,clientHeight:10,getBoundingClientRect:()=>({top:0}),querySelectorAll:()=>[]});const elements=Object.fromEntries(['chat-root','chat-transcript','chat-input','chat-chips','chat-send','chat-abort','chat-token-meter'].map(id=>[id,element(id)]));elements['chat-root'].dataset.agentName='Agent';global.document={getElementById:id=>elements[id]||null,addEventListener:(type,fn)=>handlers[type]=fn,querySelector:()=>null};global.window={innerHeight:1000,addEventListener(){}};global.EventSource=function(){};global.crypto={randomUUID:()=>'command-id'};let calls=0,captured=null;global.fetch=async(url,opts)=>{calls++;captured={url,body:opts.body};return{ok:true,status:202,text:async()=>'{}'}};require(process.argv[1]);window.renderChatEvent({type:'question',id:'req-1',questions:[{header:'H',question:'Q',options:[{label:'A',description:''}]}]});const target={closest:sel=>sel==='.chat-q-opt'?{dataset:{qbi:'0',qi:'0',label:'A'}}:null};handlers.click({target});handlers.click({target});setTimeout(()=>{if(calls!==1)process.exit(1);if(!captured||captured.url!=='/chat/main/answer')process.exit(1);const body=JSON.parse(captured.body);if(body.request_id!=='req-1'||JSON.stringify(body.answers)!=='[["A"]]')process.exit(1);},0);"#;
        let status = std::process::Command::new("node")
            .args(["-e", script, &renderer])
            .status()
            .expect("node is available for browser renderer tests");
        assert!(status.success());
    }

    #[test]
    fn browser_renderer_handles_drag_drop_and_paste() {
        let renderer = format!("{}/src/renderer.js", env!("CARGO_MANIFEST_DIR"));
        let script = r#"const handlers={},style={setProperty(){}};const element=(id)=>({id,style:{...style},dataset:{},value:'',disabled:false,hidden:false,innerHTML:'',textContent:'',scrollHeight:10,scrollTop:0,clientHeight:10,getBoundingClientRect:()=>({top:0}),querySelectorAll:()=>[]});const elements=Object.fromEntries(['chat-root','chat-transcript','chat-input','chat-chips','chat-send','chat-abort','chat-token-meter','chat-dropzone','chat-cap-hint'].map(id=>[id,element(id)]));elements['chat-root'].dataset.agentName='Agent';elements['chat-dropzone'].hidden=true;global.document={getElementById:id=>elements[id]||null,addEventListener:(type,fn)=>handlers[type]=fn};global.window={innerHeight:1000,addEventListener(){}};global.EventSource=function(){};global.crypto={randomUUID:()=>'command-id'};require(process.argv[1]);const app=window.__chatWeb;const zone={closest:sel=>sel==='#chat-root'?elements['chat-root']:null};const outside={closest:()=>null};const filesDt=n=>({types:{includes:()=>true},files:Array.from({length:n},(_,i)=>({name:`f${i}.txt`}))});
handlers.dragenter({target:zone,dataTransfer:filesDt(1),preventDefault(){}});
if(app.dragDepth!==1||elements['chat-dropzone'].hidden!==false)process.exit(1);
handlers.dragleave({target:zone,dataTransfer:filesDt(1)});
if(app.dragDepth!==0||elements['chat-dropzone'].hidden!==true)process.exit(1);
let navigated=false;
handlers.dragover({target:outside,dataTransfer:filesDt(1),preventDefault(){navigated=true;}});
if(!navigated)process.exit(1);
handlers.drop({target:zone,dataTransfer:filesDt(9),preventDefault(){}});
if(app.pending.length!==8||elements['chat-dropzone'].hidden!==true)process.exit(1);
if(!elements['chat-cap-hint'].textContent.includes('1 file skipped'))process.exit(1);
app.pending=[];
let pasted=false;
handlers.paste({target:elements['chat-input'],clipboardData:{files:[new File(['x'],'image.png',{type:'image/png'})]},preventDefault(){pasted=true;}});
if(!pasted||app.pending.length!==1||!/^pasted-\d+-\d+\.png$/.test(app.pending[0].name))process.exit(1);
if(elements['chat-cap-hint'].textContent.includes('skipped'))process.exit(1);
"#;
        let status = std::process::Command::new("node")
            .args(["-e", script, &renderer])
            .status()
            .expect("node is available for browser renderer tests");
        assert!(status.success());
    }

    #[test]
    fn transcripts_are_ordered_and_isolated() {
        let root = test_root();
        let first = root.join("one.jsonl");
        let second = root.join("two.jsonl");
        for (path, seq) in [(&first, 1), (&first, 2), (&second, 1)] {
            append_transcript(
                path,
                &WireEvent {
                    seq,
                    ts: now_ms(),
                    kind: "delta".into(),
                    text: Some(seq.to_string()),
                    id: None,
                    name: None,
                    args: None,
                    is_error: None,
                    done: None,
                    error: None,
                    tokens_used: None,
                    context_window: None,
                    attachments: vec![],
                    questions: None,
                    origin: None,
                    historical: false,
                },
            )
            .unwrap();
        }
        assert_eq!(
            load_transcript(&first)
                .unwrap()
                .into_iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(
            load_transcript(&second)
                .unwrap()
                .into_iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            [1]
        );
    }
}
