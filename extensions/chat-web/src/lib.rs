//! Browser chat surface. It is deliberately an opt-in extension: without an
//! `extensions.chat-web` section it mounts neither routes nor dashboard tab.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    sync::Arc,
};

mod apps;
mod artifacts;

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
/// Default idle time before a resumable slot's backend is closed.
const DEFAULT_IDLE_MINUTES: u64 = 5;
const IDLE_SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
/// Bound on a reaped backend's `close()`; a hung close is abandoned.
const IDLE_CLOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    backend: Option<String>,
    command: Option<String>,
    /// Minutes of inactivity before an idle conversation's backend process is
    /// closed (default 5, `0` disables). The conversation stays resumable.
    idle_minutes: Option<u64>,
    /// Runtime kill switch: `false` skips mounting routes, the dashboard tab,
    /// and the chat coordinator service, even though the extension still
    /// links (build-time selection is by section presence). Mirrors the
    /// scheduler extension's `enabled` flag.
    enabled: Option<bool>,
    apps: apps::AppsConfig,
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
                transition: Mutex::new(()),
                slot_registry: Arc::default(),
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
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(transcript)?;
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
                root: ctx.paths.root().to_path_buf(),
            }))?;
            ctx.http.mount(host_api::HttpMount {
                namespace: "/chat".into(),
                router: router(state),
                routes: vec![
                    "/".into(),
                    "/sessions".into(),
                    "/sessions/{id}".into(),
                    "/avatar".into(),
                    "/notify.mp3".into(),
                    "/notify.js".into(),
                    "/events".into(),
                    "/apps/config".into(),
                    "/{session}/apps/{record}".into(),
                    "/{session}/apps/{record}/rpc".into(),
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
            if let Err(error) = apps::start(Arc::clone(state)).await {
                tracing::warn!(%error, "MCP Apps sandbox unavailable; keeping text fallback");
            }
            let idle_minutes = state.config.idle_minutes.unwrap_or(DEFAULT_IDLE_MINUTES);
            if idle_minutes > 0 {
                let state = Arc::clone(state);
                let mut shutdown = state
                    .start
                    .get()
                    .expect("start context set above")
                    .shutdown
                    .clone();
                let idle = std::time::Duration::from_secs(idle_minutes.saturating_mul(60));
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            _ = shutdown.cancelled() => break,
                            _ = tokio::time::sleep(IDLE_SWEEP_INTERVAL) => {
                                state.reap_idle(now_ms(), idle).await;
                            }
                        }
                    }
                });
            }
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
    root: std::path::PathBuf,
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
            r#"<style>{}</style><section class="chat-web" id="chat-root" data-agent-name="{}"{}><aside class="chat-sidebar" aria-label="Conversations"><div class="chat-sidebar-head"><strong>Chats</strong><div class="chat-sidebar-controls"><button type="button" id="chat-search-open" class="chat-icon" aria-label="Search conversations"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><circle cx="10.5" cy="10.5" r="6.5"/><path d="m16 16 4 4"/></svg></button><button type="button" id="chat-sidebar-toggle" data-sidebar-toggle class="chat-icon" aria-label="Toggle sidebar"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M4 5h16M4 12h16M4 19h16"/></svg></button></div></div><div id="chat-history-list" class="chat-history-list" aria-live="polite"></div></aside><div class="chat-main"><header class="chat-header"><button type="button" data-sidebar-toggle class="chat-icon chat-mobile-sidebar-toggle" aria-label="Open conversations"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M4 5h16M4 12h16M4 19h16"/></svg></button><div>{}<span class="chat-title"><span class="chat-name-row"><strong>{}</strong>{}</span>{}</span><span id="chat-session-title" class="chat-session-title" aria-label="Current conversation" data-untitled="true">New chat</span></div><button type="button" class="chat-new" onclick="window.__chatWeb&amp;&amp;window.__chatWeb.newChat()"><svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" aria-hidden="true"><path d="M12 5v14M5 12h14"/></svg>New chat</button></header><div class="chat-dropzone" id="chat-dropzone" hidden>Drop files to attach</div><div class="chat-transcript" id="chat-transcript" role="log" aria-live="polite"></div><div class="chat-hero" id="chat-hero">{}<div class="chat-hero-line" id="chat-hero-line"></div></div><form class="chat-dock" id="chat-composer" autocomplete="off" onsubmit="event.preventDefault()"><div class="chat-chips" id="chat-chips"></div><div class="chat-cap-hint" id="chat-cap-hint"></div><div class="chat-compose-line"><div class="chat-row"><button type="button" id="chat-attach" class="chat-icon" aria-label="Attach files"><svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M21.4 11.1l-9.2 9.1a6 6 0 01-8.4-8.4L13 2.6a4 4 0 015.6 5.6l-9.2 9.2a2 2 0 01-2.8-2.8l8.5-8.5"/></svg></button><input type="file" id="chat-attachments" multiple hidden><div id="chat-slash-menu" class="chat-slash-menu" role="listbox" aria-label="Commands" hidden></div><textarea class="chat-input" id="chat-input" rows="1" placeholder="Message {}" aria-label="Message" role="combobox" aria-autocomplete="list" aria-expanded="false" aria-controls="chat-slash-menu"></textarea><button type="button" id="chat-abort" class="chat-icon" aria-label="Stop" hidden><svg width="16" height="16" viewBox="0 0 24 24" fill="currentColor"><rect x="6" y="6" width="12" height="12" rx="2"/></svg></button><button type="submit" id="chat-send" class="chat-icon" aria-label="Send" disabled><svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2"><path d="M12 19V5M5 12l7-7 7 7"/></svg></button></div><div class="chat-context"><button type="button" id="chat-context-ring" class="chat-context-ring" aria-describedby="chat-context-tooltip" hidden><svg width="32" height="32" viewBox="0 0 32 32" fill="none" stroke-width="2.5" aria-hidden="true"><circle class="chat-context-track" cx="16" cy="16" r="14"/><circle class="chat-context-fill" cx="16" cy="16" r="14" pathLength="100"/></svg><span class="chat-context-pct" aria-hidden="true"></span></button><div id="chat-context-tooltip" class="chat-context-tooltip" role="tooltip" hidden></div></div></div><div id="chat-context-warning" class="chat-context-warning" hidden>Context is filling up — send /compact to summarize.</div></form></div><div id="chat-search-palette" class="chat-search-palette" role="dialog" aria-modal="true" aria-label="Search conversations" hidden><div class="chat-search-surface"><input type="search" id="chat-search-input" aria-label="Search titles and messages" placeholder="Search titles and message text of your chats" role="combobox" aria-autocomplete="list" aria-expanded="true" aria-controls="chat-search-results" autocomplete="off"><div id="chat-search-results" role="listbox" aria-label="Search results"></div><div id="chat-search-status" role="status" aria-live="polite"></div></div></div></section><script>{}</script><script>{}</script><script>{}</script><script>{}</script>"#,
            include_str!("chat.css"),
            escape_html_attr(&self.agent_name),
            avatar_icon_attr(self.agent_avatar.as_ref()),
            avatar_html(self.agent_avatar.as_ref(), "chat-avatar", &self.agent_name),
            escape_html_attr(&self.agent_name),
            runner_info_html(&self.root),
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
            include_str!("apps.js"),
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

/// Info icon after the agent name. Its tooltip lists the `runner:` block of
/// `agent.yaml`, read on every render so edits show after a reload.
fn runner_info_html(root: &std::path::Path) -> String {
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct Runner {
        #[serde(rename = "use")]
        runner: Option<String>,
        provider: Option<String>,
        model: Option<String>,
        #[serde(alias = "effort")]
        thinking: Option<String>,
    }
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct AgentYaml {
        runner: Runner,
    }
    let runner = fs::read_to_string(root.join("agent.yaml"))
        .ok()
        .and_then(|contents| serde_yaml::from_str::<AgentYaml>(&contents).ok())
        .unwrap_or_default()
        .runner;
    let value = |v: &Option<String>| {
        escape_html_attr(
            v.as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .unwrap_or("default"),
        )
    };
    let runner_line = runner
        .runner
        .as_deref()
        .map(|r| format!("<tr><th>Runner</th><td>{}</td></tr>", escape_html_attr(r)))
        .unwrap_or_default();
    format!(
        r#"<span class="chat-info"><button type="button" id="chat-info-button" class="chat-info-button" aria-label="Agent model info" aria-describedby="chat-info-tooltip"><svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"><circle cx="12" cy="12" r="10"/><path d="M12 11v6"/><path d="M12 7.5v.01"/></svg></button><div id="chat-info-tooltip" class="chat-info-tooltip" role="tooltip" hidden><table>{}<tr><th>Provider</th><td>{}</td></tr><tr><th>Model</th><td>{}</td></tr><tr><th>Thinking</th><td>{}</td></tr></table></div></span>"#,
        runner_line,
        value(&runner.provider),
        value(&runner.model),
        value(&runner.thinking),
    )
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

/// Data the renderer turns into the favicon and apple touch icon. The dashboard
/// gives tabs no `<head>` hook, so the renderer sets the `<link>` tags at runtime.
fn avatar_icon_attr(avatar: Option<&Avatar>) -> String {
    match avatar {
        None => String::new(),
        Some(Avatar::Text(text)) => format!(r#" data-icon-text="{}""#, escape_html_attr(text)),
        Some(Avatar::Url(url)) => format!(r#" data-icon-src="{}""#, escape_html_attr(url)),
        Some(Avatar::File(_)) => r#" data-icon-src="/chat/avatar""#.to_owned(),
    }
}

/// A live turn end in any conversation of this agent. Never replayed.
#[derive(Clone, Serialize)]
struct TurnEnded {
    /// Unique per turn end. Browser tabs use it to play one sound each.
    turn: String,
    slot: String,
    /// Archive id when the backend already reported it.
    session: Option<String>,
    kind: &'static str,
}

fn turn_events() -> &'static broadcast::Sender<TurnEnded> {
    static EVENTS: std::sync::OnceLock<broadcast::Sender<TurnEnded>> = std::sync::OnceLock::new();
    EVENTS.get_or_init(|| broadcast::channel(64).0)
}

async fn turn_events_stream(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let stream = futures_util::stream::unfold(turn_events().subscribe(), |mut live| async move {
        loop {
            match live.recv().await {
                Ok(ended) => return Some((ended, live)),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    let shutdown_signal: dar_extension_sdk::BoxFuture<'static, ()> =
        match state.start.get().map(|start| start.shutdown.clone()) {
            Some(mut token) => Box::pin(async move { token.cancelled().await }),
            None => Box::pin(std::future::pending()),
        };
    Sse::new(
        stream
            .map(|ended| {
                Ok::<_, std::convert::Infallible>(
                    Event::default().data(serde_json::to_string(&ended).expect("serializes")),
                )
            })
            .take_until(shutdown_signal),
    )
    .keep_alive(KeepAlive::default())
}

async fn notify_script() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        include_str!("notify.js"),
    )
}

/// Optional top-level `notification_sound` in `agent.yaml`: an audio file
/// inside the agent folder. Read on each request, so a change needs no restart.
fn agent_notification_sound(root: &std::path::Path) -> Option<std::path::PathBuf> {
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct AgentYaml {
        notification_sound: Option<String>,
    }
    let value = fs::read_to_string(root.join("agent.yaml"))
        .ok()
        .and_then(|contents| serde_yaml::from_str::<AgentYaml>(&contents).ok())?
        .notification_sound?;
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let file = root.join(value).canonicalize().ok().filter(|file| {
        file.is_file() && root.canonicalize().is_ok_and(|root| file.starts_with(root))
    });
    if file.is_none() {
        tracing::warn!(notification_sound = %value, "agent.yaml notification_sound is not a file inside the agent folder; using the default sound");
    }
    file
}

async fn notify_sound(State(state): State<Arc<AppState>>) -> axum::response::Response {
    let custom = agent_notification_sound(&state.root)
        .and_then(|file| fs::read(&file).ok().map(|bytes| (file, bytes)));
    let (content_type, bytes) = match custom {
        Some((file, bytes)) => {
            let ext = file
                .extension()
                .map(|ext| ext.to_string_lossy().to_ascii_lowercase());
            let content_type = match ext.as_deref() {
                Some("wav") => "audio/wav",
                Some("ogg") => "audio/ogg",
                Some("m4a") => "audio/mp4",
                _ => "audio/mpeg",
            };
            (content_type, bytes)
        }
        None => (
            "audio/mpeg",
            include_bytes!("../assets/notify.mp3").to_vec(),
        ),
    };
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        bytes,
    )
        .into_response()
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

type SlotRegistry = std::sync::Mutex<Vec<std::sync::Weak<Session>>>;

struct AppState {
    config: Config,
    root: std::path::PathBuf,
    start: std::sync::OnceLock<StartCtx>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    meta: Mutex<HashMap<String, SessionMeta>>,
    transition: Mutex<()>,
    /// Weak handles to every slot, so concurrent slots can exclude each other's archive ids.
    slot_registry: Arc<SlotRegistry>,
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
    slot: String,
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
    resolved_live_id: std::sync::Mutex<Option<String>>,
    /// Unix ms of the last open, accepted turn, or published event.
    last_active_ms: std::sync::atomic::AtomicU64,
    abort_signal: watch::Sender<bool>,
    publish_lock: std::sync::Mutex<()>,
    command_ids: Mutex<HashSet<String>>,
    app_context: std::sync::Mutex<HashMap<String, serde_json::Value>>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    app: Option<serde_json::Value>,
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
        .route("/notify.mp3", get(notify_sound))
        .route("/notify.js", get(notify_script))
        .route("/events", get(turn_events_stream))
        .route("/apps/config", get(apps::config))
        .route("/{session}/apps/{record}", get(apps::resource))
        .route("/{session}/apps/{record}/download", get(apps::download))
        .route("/{session}/apps/{record}/open", get(apps::open))
        .route("/{session}/apps/{record}/rpc", post(apps::rpc))
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

#[derive(Default, Deserialize)]
struct SessionQuery {
    q: Option<String>,
}

#[derive(serde::Serialize)]
struct SessionListEntry {
    id: String,
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    snippet: Option<String>,
    #[serde(rename = "match", skip_serializing_if = "Option::is_none")]
    match_kind: Option<chat::archive::SessionMatchKind>,
    start_time: Option<String>,
    modified_ms: u64,
    archived: bool,
    /// Backend process currently open (not merely a retained identity).
    connected: bool,
    /// Client slot ids bound to this conversation; the focused one is selected.
    slots: Vec<String>,
    busy: bool,
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

async fn session_index(
    State(state): State<Arc<AppState>>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<Vec<SessionListEntry>>, StatusCode> {
    let dir = state.root.join("data/chat/sessions");
    // archive id -> (connected, busy, slot ids)
    let mut live: HashMap<String, (bool, bool, Vec<String>)> = HashMap::new();
    let slots: Vec<(String, Arc<Session>)> = state
        .sessions
        .lock()
        .await
        .iter()
        .map(|(id, slot)| (id.clone(), Arc::clone(slot)))
        .collect();
    for (slot_id, slot) in slots {
        if let Some(id) = state.resolve_live_id(&slot).await {
            let busy = slot.active_turns.load(std::sync::atomic::Ordering::SeqCst) > 0;
            // A locked backend is mid-open/turn/close, so treat it as connected.
            let connected = slot.inner.try_lock().map_or(true, |inner| inner.is_some());
            let entry = live.entry(id).or_default();
            entry.0 |= connected;
            entry.1 |= busy;
            entry.2.push(slot_id);
        }
    }
    let meta = state.meta.lock().await.clone();
    tokio::task::spawn_blocking(move || {
        let query = query.q.as_deref().map(str::trim).filter(|q| !q.is_empty());
        let candidates: Vec<_> = if let Some(query) = query {
            let mut paths: Vec<_> = fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.extension()
                        .is_some_and(|extension| extension == "jsonl")
                })
                .collect();
            paths.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
            let mut seen = HashSet::new();
            paths
                .into_iter()
                .filter_map(|path| {
                    let (session, messages) = chat::archive::read_session_for_search(&path)?;
                    if !valid_backend_id(&session.id) || !seen.insert(session.id.clone()) {
                        return None;
                    }
                    let label = meta
                        .get(&session.id)
                        .and_then(|item| item.title.as_deref())
                        .filter(|title| !title.trim().is_empty())
                        .unwrap_or(&session.label);
                    let matched = chat::archive::match_session(label, &messages, query)?;
                    Some((session, path, Some(matched)))
                })
                .collect()
        } else {
            chat::archive::list(&dir)
                .into_iter()
                .filter_map(|session| {
                    let path = chat::archive::session_file_by_id(&dir, &session.id)?;
                    Some((session, path, None))
                })
                .collect()
        };
        let mut sessions: Vec<_> = candidates
            .into_iter()
            .filter_map(|(session, path, matched)| {
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
                let label = title.clone().unwrap_or(session.label);
                let snippet = matched.as_ref().and_then(|matched| matched.snippet.clone());
                let match_kind = matched.map(|matched| matched.kind);
                Some(SessionListEntry {
                    connected: live.get(&session.id).is_some_and(|l| l.0),
                    busy: live.get(&session.id).is_some_and(|l| l.1),
                    slots: live
                        .get(&session.id)
                        .map(|l| l.2.clone())
                        .unwrap_or_default(),
                    label,
                    id: session.id,
                    title,
                    snippet,
                    match_kind,
                    start_time: session.start_time,
                    modified_ms,
                    archived: item_meta.archived.unwrap_or(false),
                })
            })
            .collect();
        sessions.sort_by_key(|session| {
            (
                session.match_kind != Some(chat::archive::SessionMatchKind::Title),
                std::cmp::Reverse(session.modified_ms),
            )
        });
        if query.is_some() {
            sessions.truncate(chat::archive::LIST_LIMIT);
        }
        Json(sessions)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
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
        Some(mut page) => {
            let mut events = VecDeque::new();
            for value in &mut page.events {
                if let Ok(mut event) = serde_json::from_value::<WireEvent>(value.clone()) {
                    apps::capture_result(Some(&state.root), &mut event, &events);
                    if let Some(text) = &event.text {
                        value["text"] = serde_json::json!(text)
                    }
                    if let Some(app) = &event.app {
                        value["app"] = app.clone()
                    }
                    events.push_back(event);
                }
            }
            Json(page).into_response()
        }
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
    if let Some((slot_id, _)) = state.slot_for_live(&id).await {
        if let Err(error) = state.reset_session_unlocked(&slot_id).await {
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
    Path(_current_slot): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<ResumeRequest>,
) -> impl IntoResponse {
    let dir = state.root.join("data/chat/sessions");
    if !valid_backend_id(&body.id) || chat::archive::session_file_by_id(&dir, &body.id).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let _transition = state.transition.lock().await;
    if let Some((slot, _)) = state.slot_for_live(&body.id).await {
        return slot_response(&slot);
    }
    let session = match state.session(&body.id).await {
        Ok(session) => session,
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };
    let already_replayed = !session
        .history
        .lock()
        .expect("chat-web history mutex poisoned")
        .is_empty();
    let mut offset = (!already_replayed).then_some(0);
    while let Some(start) = offset {
        let Some(page) = chat::archive::read_events(&dir, &body.id, start, chat::archive::READ_MAX)
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
            let mut history = session
                .history
                .lock()
                .expect("chat-web history mutex poisoned");
            apps::capture_result(Some(&state.root), &mut event, &history);
            if let Err(error) = append_transcript(&session.transcript, &event) {
                return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
            }
            history.push_back(event.clone());
            let _ = session.tx.send(event);
        }
        offset = page.next_offset;
    }
    *session
        .resolved_live_id
        .lock()
        .expect("chat-web identity mutex poisoned") = Some(body.id.clone());
    session
        .suppress_resume
        .store(false, std::sync::atomic::Ordering::SeqCst);
    slot_response(&body.id)
}

fn slot_response(slot: &str) -> Response {
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "slot": slot })),
    )
        .into_response()
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
            slot: id.to_owned(),
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
            resolved_live_id: std::sync::Mutex::new(None),
            last_active_ms: std::sync::atomic::AtomicU64::new(now_ms()),
            abort_signal,
            publish_lock: std::sync::Mutex::new(()),
            command_ids: Mutex::new(HashSet::new()),
            app_context: std::sync::Mutex::new(HashMap::new()),
            history: std::sync::Mutex::new(history),
            transcript,
            #[cfg(test)]
            pause_after_send: std::sync::Mutex::new(None),
            #[cfg(test)]
            pause_after_subscribe: std::sync::Mutex::new(None),
        });
        self.slot_registry
            .lock()
            .expect("chat-web slot registry poisoned")
            .push(Arc::downgrade(&session));
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
        let resolved = self.resolve_live_id(&session).await;
        let resume_session_id = if session
            .suppress_resume
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            None
        } else {
            resolved
        };
        let interface = if session.slot == "main" {
            "chat"
        } else {
            "web chat"
        };
        let params = chat::agent_session_params_with_interface(start, &session_dir, interface)
            .command(self.config.command.as_deref().unwrap_or(""))
            .resume_session_id(resume_session_id)
            .build();
        let generation = session
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        session.touch();
        let (event_tx, mut event_rx) = mpsc::channel::<ChatEvent>(128);
        let sink = Arc::clone(&session);
        let identity_sink = Arc::clone(&session);
        let identity_registry = Arc::clone(&self.slot_registry);
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                if let ChatEvent::SessionIdentity { id } = &event {
                    // Not a UI event: bind the slot's archive id (first binding wins).
                    let mut resolved = identity_sink
                        .resolved_live_id
                        .lock()
                        .expect("chat-web identity mutex poisoned");
                    if resolved.is_none()
                        && identity_sink
                            .generation
                            .load(std::sync::atomic::Ordering::SeqCst)
                            == generation
                    {
                        let others = held_ids_except(&identity_registry, &identity_sink);
                        if others.contains(id) {
                            tracing::warn!(
                                id = %id,
                                "chat backend reported a session id already bound to another chat-web slot"
                            );
                        }
                        *resolved = Some(id.clone());
                    }
                    drop(resolved);
                    let _ = sink.events.send(event);
                    continue;
                }
                let _acceptance = sink.acceptance_lock.lock().await;
                if sink.publish_if_current(generation, event.clone()) {
                    let _ = sink.events.send(event);
                }
            }
        });
        backend.open(params, event_tx).await
    }

    /// Close backends of resumable, idle slots. Transcript, history and the
    /// resolved archive id are kept, so the next send reopens the same
    /// conversation via `resume_session_id`. Slots without a backend-reported
    /// identity are never closed (a reopen would silently start fresh).
    /// Closes run in detached, time-bounded tasks so a hung backend cannot
    /// stall the sweep, other sessions, or the `transition` lock.
    async fn reap_idle(&self, now_ms: u64, idle: std::time::Duration) {
        use std::sync::atomic::Ordering::SeqCst;
        let idle_ms = idle.as_millis() as u64;
        let is_idle = |s: &Session| {
            s.active_turns.load(SeqCst) == 0
                && now_ms.saturating_sub(s.last_active_ms.load(SeqCst)) >= idle_ms
                && s.resolved_live_id
                    .lock()
                    .expect("chat-web identity mutex poisoned")
                    .is_some()
        };
        let slots: Vec<Arc<Session>> = self.sessions.lock().await.values().cloned().collect();
        for session in slots {
            if !is_idle(&session) {
                continue;
            }
            // Same order as submit/send/reset: transition, then inner.
            let _transition = self.transition.lock().await;
            // abort() skips `transition`; never wait on a busy slot.
            let Ok(mut inner) = session.inner.try_lock() else {
                continue;
            };
            // Order: transition, inner, acceptance (same as accept_turn). The
            // event pump takes only acceptance, so autonomous TurnStarted
            // cannot slip between the final idle check and the generation bump.
            let _acceptance = session.acceptance_lock.lock().await;
            if inner.is_none() || !is_idle(&session) {
                continue;
            }
            let backend = inner.take().expect("checked above");
            session.generation.fetch_add(1, SeqCst);
            drop(_acceptance);
            drop(inner);
            tokio::spawn(async move {
                match tokio::time::timeout(IDLE_CLOSE_TIMEOUT, backend.close()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "chat-web idle backend close failed")
                    }
                    Err(_) => tracing::warn!("chat-web idle backend close timed out"),
                }
            });
        }
    }

    /// Archive id bound by the backend's `SessionIdentity` or a `/resume` replay.
    async fn resolve_live_id(&self, session: &Session) -> Option<String> {
        session
            .resolved_live_id
            .lock()
            .expect("chat-web identity mutex poisoned")
            .clone()
    }

    async fn slot_for_live(&self, archive_id: &str) -> Option<(String, Arc<Session>)> {
        let slots: Vec<(String, Arc<Session>)> = self
            .sessions
            .lock()
            .await
            .iter()
            .map(|(id, slot)| (id.clone(), Arc::clone(slot)))
            .collect();
        for (id, slot) in slots {
            if self.resolve_live_id(&slot).await.as_deref() == Some(archive_id) {
                return Some((id, slot));
            }
        }
        None
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
        let Some(id) = self.resolve_live_id(&web_session).await else {
            return;
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
                app: None,
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

fn held_ids_except(registry: &SlotRegistry, session: &Session) -> HashSet<String> {
    let slots = registry.lock().expect("chat-web slot registry poisoned");
    slots
        .iter()
        .filter_map(std::sync::Weak::upgrade)
        .filter(|slot| !std::ptr::eq(Arc::as_ptr(slot), session))
        .filter_map(|slot| {
            slot.resolved_live_id
                .lock()
                .expect("chat-web identity mutex poisoned")
                .clone()
        })
        .collect()
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
    fn touch(&self) {
        self.last_active_ms
            .store(now_ms(), std::sync::atomic::Ordering::SeqCst);
    }

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
        let context = self
            .app_context
            .lock()
            .expect("app context poisoned")
            .clone();
        let prompt = if context.is_empty() {
            prompt
        } else {
            format!(
                "{prompt}\n\n<mcp-app-context>\n{}\n</mcp-app-context>",
                serde_json::to_string(&context)?
            )
        };
        backend.send_turn(prompt).await?;
        {
            let mut pending = self.app_context.lock().expect("app context poisoned");
            for (view, value) in context {
                if pending.get(&view) == Some(&value) {
                    pending.remove(&view);
                }
            }
        }
        if let Err(error) = self.publish_user(display.clone(), attachments) {
            let _ = backend.abort().await;
            return Err(error.context("failed to persist accepted chat turn"));
        }
        let _ = self.events.send(ChatEvent::User { text: display });
        self.active_turns
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.touch();
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
            app: None,
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
            ChatEvent::User { .. }
            | ChatEvent::SessionReset
            | ChatEvent::SessionIdentity { .. } => return true,
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
        let mut event = WireEvent {
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
            app: None,
        };
        let mut history = self
            .history
            .lock()
            .expect("chat-web history mutex poisoned");
        apps::capture_result(self.transcript.ancestors().nth(4), &mut event, &history);
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
        self.touch();
        history.push_back(event.clone());
        if matches!(kind, "finished" | "aborted") {
            let slot = self
                .transcript
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            let _ = turn_events().send(TurnEnded {
                turn: format!("{slot}:{}", event.seq),
                session: self
                    .resolved_live_id
                    .lock()
                    .expect("chat-web live id mutex poisoned")
                    .clone(),
                slot,
                kind: if kind == "finished" {
                    "finished"
                } else {
                    "aborted"
                },
            });
        }
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
            app: None,
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
    let mut events = VecDeque::new();
    for line in BufReader::new(file).lines() {
        let mut event = serde_json::from_str(&line?)?;
        apps::capture_result(path.ancestors().nth(4), &mut event, &events);
        events.push_back(event);
    }
    Ok(events)
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
                    Ok(events) => events
                        .into_iter()
                        .map(|mut event| {
                            event.historical = true;
                            event
                        })
                        .collect(),
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
                .map(|mut event| {
                    event.historical = true;
                    event
                })
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
            message: match body.instructions.as_deref().map(str::trim) {
                Some(instructions) if !instructions.is_empty() => {
                    format!("/compact {instructions}")
                }
                _ => "/compact".into(),
            },
        }),
    )
    .await
}

#[derive(Deserialize)]
struct Compact {
    command_id: String,
    #[serde(default)]
    instructions: Option<String>,
}

static FRESH_SLOT_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

async fn new_session_route(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let _transition = state.transition.lock().await;
    let reusable = match state.sessions.lock().await.get(&id).cloned() {
        Some(slot) => {
            state.resolve_live_id(&slot).await.is_none()
                && slot.inner.lock().await.is_none()
                && slot
                    .history
                    .lock()
                    .expect("chat-web history mutex poisoned")
                    .is_empty()
                && slot.active_turns.load(std::sync::atomic::Ordering::SeqCst) == 0
        }
        None => false,
    };
    if reusable {
        return slot_response(&id);
    }
    let fresh = format!(
        "w{}-{}",
        now_ms(),
        FRESH_SLOT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    );
    match state.session(&fresh).await {
        Ok(_) => slot_response(&fresh),
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

    // Approved seam: issue Testing Decisions, result capture through chat SSE.
    // Catches runner escaping and name association; misses iframe lifecycle.
    #[tokio::test]
    async fn app_markers_are_hidden_in_runner_output_and_bound_to_the_call() {
        for output in [
            "visible [mcp-app-result:0123456789abcdef0123456789abcdef]".to_owned(),
            serde_json::to_string(&"visible [mcp-app-result:0123456789abcdef0123456789abcdef]")
                .unwrap(),
            serde_json::to_string(
                &serde_json::to_string(
                    &"visible [mcp-app-result:0123456789abcdef0123456789abcdef]",
                )
                .unwrap(),
            )
            .unwrap(),
        ] {
            let root = test_root();
            let records = root.join("data/mcp-apps/results");
            fs::create_dir_all(&records).unwrap();
            fs::write(records.join("0123456789abcdef0123456789abcdef.json"), serde_json::to_vec(&serde_json::json!({
                "tool":"draw__canvas", "server":"draw", "resource_uri":"ui://canvas", "args":{"shape":"box"},
                "result":{"content":[{"type":"text","text":"visible"}]}, "created_at":now_ms()
            })).unwrap()).unwrap();
            let state = slot_state(root, HashMap::new());
            let session = state.session("test").await.unwrap();
            session.publish(ChatEvent::ToolCall {
                id: "c".into(),
                name: "dar.draw__canvas".into(),
                args: "{}".into(),
            });
            session.publish(ChatEvent::ToolOutput {
                id: "c".into(),
                text: output,
                is_error: false,
                done: true,
            });
            let response = router(state)
                .oneshot(
                    axum::http::Request::builder()
                        .uri("/test/stream")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let mut body = response.into_body();
            let mut data = String::new();
            for _ in 0..2 {
                data.push_str(
                    std::str::from_utf8(&body.frame().await.unwrap().unwrap().into_data().unwrap())
                        .unwrap(),
                );
            }
            assert!(!data.contains("[mcp-app-result:"), "{data}");
            assert!(
                data.contains("recordId")
                    && data.contains("ui://canvas")
                    && data.contains("visible"),
                "{data}"
            );
        }
    }

    // Approved in ALG-532 Testing Decisions: public host HTTP/JSON-RPC seam.
    // Catches built-in catalog availability and scoped artifact access; misses browser enforcement.
    async fn artifact_http_state() -> (Arc<AppState>, String, String) {
        use dar_extension_sdk::artifacts::{ArtifactMetadataInput, ArtifactStore, ExportRoot};
        let root = test_root();
        fs::create_dir_all(root.join("exports")).unwrap();
        fs::write(
            root.join("exports/report.html"),
            "<h1>Quarterly report</h1>",
        )
        .unwrap();
        let store = Arc::new(ArtifactStore::open(root.join("vault"), 1024).unwrap());
        let metadata = store
            .stage_from_export_root(
                &ExportRoot::open(root.join("exports")).unwrap(),
                "report.html",
                ArtifactMetadataInput {
                    filename: "report.html".into(),
                    media_type: Some("text/html".into()),
                    caption: None,
                },
            )
            .unwrap();
        let uri = format!("dar-artifact://{}", metadata.id);
        let record = dar_extension_sdk::mcp_apps::ResultRecord {
            tool: "artifact_publish".into(), pi_tool_name: Some("mcp__dar__artifact_publish".into()),
            server: "dar".into(), resource_uri: "ui://dar/artifact-viewer".into(),
            args: serde_json::json!({"path":"report.html"}),
            result: serde_json::json!({"content":[{"type":"text","text":"artifact published"}], "structuredContent":{
                "uri": uri, "filename":"report.html", "mediaType":"text/html", "bytes":25, "title":"Quarterly report"
            }}), created_at: now_ms()/1000,
        }.write(&root).unwrap();
        let mut services = dar_extension_sdk::ServiceRegistry::default();
        services
            .register::<ArtifactStore>("artifact-store", store)
            .unwrap();
        let config = Config::default();
        config.apps.bound_port.set(28181).unwrap();
        let state = Arc::new(AppState {
            config,
            root,
            start: std::sync::OnceLock::from(start_ctx(services)),
            sessions: Mutex::new(HashMap::new()),
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
            meta: Mutex::new(HashMap::new()),
        });
        let session = state.session("test").await.unwrap();
        session.publish(ChatEvent::ToolCall {
            id: "artifact".into(),
            name: "artifact_publish".into(),
            args: "{}".into(),
        });
        session.publish(ChatEvent::ToolOutput {
            id: "artifact".into(),
            text: format!("artifact published [mcp-app-result:{record}]"),
            is_error: false,
            done: true,
        });
        (state, record, uri)
    }

    // ALG-537 Testing Decisions approve router HTTP/JSON-RPC and transcript seams.
    // Catches per-record policy and permissions; misses browser enforcement.
    async fn preview_http_state(
        url: &str,
        frameable: bool,
        config: Config,
    ) -> (Arc<AppState>, String) {
        let root = test_root();
        let store = Arc::new(
            dar_extension_sdk::artifacts::ArtifactStore::open(root.join("vault"), 1024).unwrap(),
        );
        let record = dar_extension_sdk::mcp_apps::ResultRecord {
            tool: "web_preview".into(),
            pi_tool_name: Some("mcp__dar__web_preview".into()),
            server: "dar".into(),
            resource_uri: "ui://dar/web-preview".into(),
            args: serde_json::json!({"url":"https://redirect.example.org/start","title":"Preview example"}),
            result: serde_json::json!({"structuredContent":{
                "url":"https://redirect.example.org/start","finalUrl":url,"title":"Preview example","frameable":frameable,
                "reason":if frameable {serde_json::Value::Null} else {serde_json::json!("unreachable")},
                "status":if frameable {serde_json::json!(200)} else {serde_json::Value::Null}
            }}),
            created_at: now_ms() / 1000,
        }
        .write(&root)
        .unwrap();
        let mut services = dar_extension_sdk::ServiceRegistry::default();
        services.register("artifact-store", store).unwrap();
        config.apps.bound_port.set(28181).unwrap();
        let start = start_ctx(services);
        start.host.set_http_addr("127.0.0.1:28180".parse().unwrap());
        let state = Arc::new(AppState {
            config,
            root,
            start: std::sync::OnceLock::from(start),
            sessions: Mutex::new(HashMap::new()),
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
            meta: Mutex::new(HashMap::new()),
        });
        let session = state.session("test").await.unwrap();
        session.publish(ChatEvent::ToolCall {
            id: "preview".into(),
            name: "web_preview".into(),
            args: serde_json::json!({"url":"https://redirect.example.org/start"}).to_string(),
        });
        session.publish(ChatEvent::ToolOutput {
            id: "preview".into(),
            text: format!("web preview shown: {url} [mcp-app-result:{record}]"),
            is_error: false,
            done: true,
        });
        (state, record)
    }

    #[tokio::test]
    async fn web_preview_content_allows_only_its_loopback_page_origin() {
        let (state, id) = preview_http_state(
            "http://127.0.0.1:5173/demo?theme=dark#section",
            true,
            Config::default(),
        )
        .await;
        let response = router(Arc::clone(&state))
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/test/apps/{id}"))
                    .header("host", "127.0.0.1:28180")
                    .header("referer", "http://127.0.0.1:28180/chat")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(
            body["meta"]["ui"]["csp"]["frameDomains"],
            serde_json::json!(["http://127.0.0.1:5173"])
        );
        assert_eq!(
            body["meta"]["ui"]["csp"]["resourceDomains"],
            serde_json::json!([])
        );
        assert_eq!(
            body["meta"]["ui"]["csp"]["connectDomains"],
            serde_json::json!([])
        );
        let url = url::Url::parse(body["contentUrl"].as_str().unwrap()).unwrap();
        let response = apps::sandbox_router(state)
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("{}?{}", url.path(), url.query().unwrap()))
                    .header("host", "dar.localhost:28181")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let csp = response.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap();
        assert_eq!(
            csp.split(';')
                .map(str::trim)
                .find(|directive| directive.starts_with("frame-src ")),
            Some("frame-src http://127.0.0.1:5173")
        );
    }

    #[tokio::test]
    async fn web_preview_transcript_describes_the_page_without_artifact_fields() {
        let (state, _) = preview_http_state(
            "http://127.0.0.1:5173/demo#section",
            true,
            Config::default(),
        )
        .await;
        let response = router(state)
            .oneshot(
                axum::http::Request::builder()
                    .uri("/test/stream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let mut body = response.into_body();
        body.frame().await.unwrap().unwrap();
        let frame = body.frame().await.unwrap().unwrap().into_data().unwrap();
        let event: serde_json::Value = serde_json::from_str(
            std::str::from_utf8(&frame)
                .unwrap()
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .unwrap(),
        )
        .unwrap();
        let descriptor = &event["app"];
        assert_eq!(
            descriptor["previewUrl"],
            "http://127.0.0.1:5173/demo#section"
        );
        assert_eq!(descriptor["previewTitle"], "Preview example");
        assert_eq!(descriptor["previewFrameable"], true);
        for key in [
            "artifactFilename",
            "artifactTitle",
            "artifactMediaType",
            "artifactBytes",
        ] {
            assert!(
                descriptor.get(key).is_none(),
                "unexpected {key}: {descriptor}"
            );
        }
    }

    #[tokio::test]
    async fn web_preview_refuses_chat_and_every_sandbox_listener() {
        for url in [
            "http://127.0.0.1:28180/chat",
            "http://localhost:28180/dashboard",
            "http://127.0.0.1:28181/sandbox/example",
            "http://localhost:28181/content/example",
            "http://[::1]:28181/",
            "https://draw.localhost:28181/",
            "https://localhost.:28181/",
            "https://draw.example.net/",
        ] {
            let mut config = Config::default();
            config
                .apps
                .sandbox_origins
                .insert("draw".into(), "https://draw.example.net".into());
            let (state, id) = preview_http_state(url, true, config).await;
            let response = router(state)
                .oneshot(
                    axum::http::Request::builder()
                        .uri(format!("/test/apps/{id}"))
                        .header("host", "127.0.0.1:28180")
                        .header("referer", "http://127.0.0.1:28180/chat")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{url}");
        }
    }

    #[tokio::test]
    async fn web_preview_cannot_gain_artifact_access_from_extra_result_fields() {
        use dar_extension_sdk::artifacts::{ArtifactMetadataInput, ArtifactStore, ExportRoot};
        let (state, original_id) =
            preview_http_state("http://localhost:5173/", true, Config::default()).await;
        fs::create_dir_all(state.root.join("exports")).unwrap();
        fs::write(state.root.join("exports/private.txt"), "Private report").unwrap();
        let store = state
            .start
            .get()
            .unwrap()
            .host
            .services
            .get_named::<ArtifactStore>("artifact-store")
            .unwrap();
        let metadata = store
            .stage_from_export_root(
                &ExportRoot::open(state.root.join("exports")).unwrap(),
                "private.txt",
                ArtifactMetadataInput {
                    filename: "private.txt".into(),
                    media_type: Some("text/plain".into()),
                    caption: None,
                },
            )
            .unwrap();
        let uri = format!("dar-artifact://{}", metadata.id);
        let mut record =
            dar_extension_sdk::mcp_apps::read_result(&state.root, &original_id, "web_preview")
                .unwrap()
                .unwrap();
        record.result["structuredContent"]["uri"] = serde_json::json!(uri);
        record.result["structuredContent"]["filename"] = serde_json::json!("private.txt");
        record.result["structuredContent"]["mediaType"] = serde_json::json!("text/plain");
        let id = record.write(&state.root).unwrap();
        let session = state.session("test").await.unwrap();
        session.publish(ChatEvent::ToolCall {
            id: "preview-extra".into(),
            name: "web_preview".into(),
            args: "{}".into(),
        });
        session.publish(ChatEvent::ToolOutput {
            id: "preview-extra".into(),
            text: format!("web preview [mcp-app-result:{id}]"),
            is_error: false,
            done: true,
        });
        let app = router(Arc::clone(&state));
        for (method, params) in [
            ("resources/read", serde_json::json!({"uri":uri})),
            (
                "tools/call",
                serde_json::json!({"name":"artifact_publish","arguments":{"path":"private.txt"}}),
            ),
            (
                "ui/message",
                serde_json::json!({"role":"user","content":[{"type":"text","text":"From a preview"}]}),
            ),
            (
                "ui/update-model-context",
                serde_json::json!({"structuredContent":{"preview":true}}),
            ),
        ] {
            let response = app.clone().oneshot(axum::http::Request::builder()
                .uri(format!("/test/apps/{id}/rpc")).method("POST").header("content-type", "application/json")
                .body(Body::from(serde_json::json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}).to_string())).unwrap()).await.unwrap();
            let body: serde_json::Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            assert!(body["error"].is_object(), "{method}: {body}");
        }
        for action in ["download", "open"] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(format!("/test/apps/{id}/{action}"))
                        .header("host", "127.0.0.1:28180")
                        .header("referer", "http://127.0.0.1:28180/chat")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{action}");
        }
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/test/apps/{id}"))
                    .header("host", "127.0.0.1:28180")
                    .header("referer", "http://127.0.0.1:28180/chat")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        let url = url::Url::parse(body["contentUrl"].as_str().unwrap()).unwrap();
        let response = apps::sandbox_router(state)
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/open/{id}?{}", url.query().unwrap()))
                    .header("host", "dar.localhost:28181")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn web_preview_link_card_has_no_frame_origin() {
        let (state, id) =
            preview_http_state("https://docs.example.org/page", false, Config::default()).await;
        let response = router(Arc::clone(&state))
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/test/apps/{id}"))
                    .header("host", "127.0.0.1:28180")
                    .header("referer", "http://127.0.0.1:28180/chat")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(
            body["meta"]["ui"]["csp"]["frameDomains"],
            serde_json::json!([])
        );
        let url = url::Url::parse(body["contentUrl"].as_str().unwrap()).unwrap();
        let response = apps::sandbox_router(state)
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("{}?{}", url.path(), url.query().unwrap()))
                    .header("host", "dar.localhost:28181")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let csp = response.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap();
        assert_eq!(
            csp.split(';')
                .map(str::trim)
                .find(|directive| directive.starts_with("frame-src ")),
            Some("frame-src 'none'")
        );
    }

    #[tokio::test]
    async fn web_preview_policy_uses_only_validated_final_url_origins() {
        for (url, expected_origin) in [
            ("http://localhost:5173/page", Some("http://localhost:5173")),
            ("http://[::1]:5173/page", Some("http://[::1]:5173")),
            (
                "https://docs.example.org/page?theme=dark#details",
                Some("https://docs.example.org"),
            ),
            ("ftp://files.example.org/page", None),
            ("http://docs.example.org/page", None),
            ("https://reader:example@docs.example.org/page", None),
            ("https://reader@docs.example.org/page", None),
        ] {
            let (state, id) = preview_http_state(url, true, Config::default()).await;
            let response = router(state)
                .oneshot(
                    axum::http::Request::builder()
                        .uri(format!("/test/apps/{id}"))
                        .header("host", "127.0.0.1:28180")
                        .header("referer", "http://127.0.0.1:28180/chat")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            if let Some(origin) = expected_origin {
                assert_eq!(response.status(), StatusCode::OK);
                let body: serde_json::Value = serde_json::from_slice(
                    &response.into_body().collect().await.unwrap().to_bytes(),
                )
                .unwrap();
                assert_eq!(
                    body["meta"]["ui"]["csp"]["frameDomains"],
                    serde_json::json!([origin])
                );
            } else {
                assert_eq!(response.status(), StatusCode::NOT_FOUND);
            }
        }
    }

    #[tokio::test]
    async fn artifact_view_bootstraps_without_an_external_mcp_catalog() {
        let (state, id, _) = artifact_http_state().await;
        let response = router(state)
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/test/apps/{id}"))
                    .header("host", "127.0.0.1:28180")
                    .header("referer", "http://127.0.0.1:28180/chat")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["server"], "dar");
        assert_eq!(body["sandboxOrigin"], "http://dar.localhost:28181");
        assert_eq!(
            body["meta"]["ui"]["csp"]["resourceDomains"],
            serde_json::json!([
                "https://cdn.jsdelivr.net",
                "https://cdnjs.cloudflare.com",
                "https://unpkg.com",
                "https://fonts.googleapis.com",
                "https://fonts.gstatic.com"
            ])
        );
        assert_eq!(
            body["meta"]["ui"]["csp"]["connectDomains"],
            serde_json::json!([])
        );
        assert_eq!(
            body["meta"]["ui"]["csp"]["frameDomains"],
            serde_json::json!([])
        );
        assert_eq!(
            body["result"]["structuredContent"]["filename"],
            "report.html"
        );
        assert!(body["html"].as_str().is_some_and(|html| !html.is_empty()));
    }

    #[tokio::test]
    async fn artifact_view_can_read_only_its_result_and_cannot_publish() {
        let (state, id, uri) = artifact_http_state().await;
        use dar_extension_sdk::artifacts::{ArtifactMetadataInput, ArtifactStore, ExportRoot};
        let store = state
            .start
            .get()
            .unwrap()
            .host
            .services
            .get_named::<ArtifactStore>("artifact-store")
            .unwrap();
        fs::write(state.root.join("exports/private.txt"), "Other conversation").unwrap();
        let other = store
            .stage_from_export_root(
                &ExportRoot::open(state.root.join("exports")).unwrap(),
                "private.txt",
                ArtifactMetadataInput {
                    filename: "private.txt".into(),
                    media_type: Some("text/plain".into()),
                    caption: None,
                },
            )
            .unwrap();
        let other_uri = format!("dar-artifact://{}", other.id);
        let app = router(state);
        for (session, method, params, allowed) in [
            (
                "test",
                "resources/read",
                serde_json::json!({"uri":uri}),
                true,
            ),
            (
                "test",
                "resources/read",
                serde_json::json!({"uri":other_uri}),
                false,
            ),
            (
                "other",
                "resources/read",
                serde_json::json!({"uri":uri}),
                false,
            ),
            (
                "test",
                "tools/call",
                serde_json::json!({"name":"artifact_publish","arguments":{"path":"report.html"}}),
                false,
            ),
        ] {
            let response = app.clone().oneshot(axum::http::Request::builder()
                .uri(format!("/{session}/apps/{id}/rpc")).method("POST").header("content-type","application/json")
                .body(Body::from(serde_json::to_vec(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})).unwrap())).unwrap()).await.unwrap();
            let body: serde_json::Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            if allowed {
                assert_eq!(body["result"]["contents"][0]["uri"], uri);
                assert_eq!(body["result"]["contents"][0]["mimeType"], "text/html");
                assert_eq!(
                    body["result"]["contents"][0]["text"],
                    "<h1>Quarterly report</h1>"
                );
            } else {
                assert!(body["error"].is_object(), "{body}");
            }
        }
    }

    // Approved ALG-532 built-in provider seam: standard MCP resources/read and catalog.
    // Catches vault-to-wire byte representation; misses browser image decoding.
    #[tokio::test]
    async fn artifact_provider_serves_binary_and_lists_a_model_only_tool() {
        use dar_extension_sdk::{
            artifacts::{ArtifactMetadataInput, ArtifactStore, ExportRoot},
            mcp_apps::McpApps,
        };
        let (state, _, _) = artifact_http_state().await;
        let store = state
            .start
            .get()
            .unwrap()
            .host
            .services
            .get_named::<ArtifactStore>("artifact-store")
            .unwrap();
        fs::write(state.root.join("exports/chart.png"), b"\x89PNG\r\n\x1a\n").unwrap();
        let metadata = store
            .stage_from_export_root(
                &ExportRoot::open(state.root.join("exports")).unwrap(),
                "chart.png",
                ArtifactMetadataInput {
                    filename: "chart.png".into(),
                    media_type: Some("image/png".into()),
                    caption: None,
                },
            )
            .unwrap();
        let provider = artifacts::ArtifactApp { store };
        let tools = provider.list_tools().await.unwrap();
        assert_eq!(
            tools,
            serde_json::json!([
                {"name":"artifact_publish","server":"dar","resourceUri":"ui://dar/artifact-viewer","visibility":["model"]},
                {"name":"web_preview","server":"dar","resourceUri":"ui://dar/web-preview","visibility":["model"]}
            ])
        );
        let uri = format!("dar-artifact://{}", metadata.id);
        let resource = provider.read_resource("dar", &uri).await.unwrap();
        assert_eq!(
            resource,
            serde_json::json!({"contents":[{"uri":uri,"mimeType":"image/png","blob":"iVBORw0KGgo="}]})
        );
        assert!(provider
            .call_tool("dar", "artifact_publish", serde_json::json!({}))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn artifact_download_is_verified_attachment_for_its_own_conversation() {
        let (state, id, uri) = artifact_http_state().await;
        let app = router(Arc::clone(&state));
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/test/apps/{id}/download"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/octet-stream"
        );
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        assert_eq!(
            response.headers()[header::CONTENT_DISPOSITION],
            "attachment; filename=\"report.html\"; filename*=UTF-8''report.html"
        );
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "<h1>Quarterly report</h1>"
        );
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/other/apps/{id}/download"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let artifact_id = uri.strip_prefix("dar-artifact://").unwrap();
        fs::write(
            state.root.join("vault").join(artifact_id).join("data"),
            "<h1>Altered   report</h1>",
        )
        .unwrap();
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/test/apps/{id}/download"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // ALG-534 Testing Decisions approve HTTP status, headers, and response bytes.
    // Catches ticket/session/host checks and top-level policy; misses browser CSP enforcement.
    #[tokio::test]
    async fn artifact_open_uses_fresh_sandbox_ticket_and_serves_isolated_html() {
        let (state, id, _) = artifact_http_state().await;
        let app = router(Arc::clone(&state));
        let sandbox = apps::sandbox_router(Arc::clone(&state));
        let mut urls = vec![];
        for _ in 0..2 {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(format!("/test/apps/{id}/open"))
                        .header("host", "127.0.0.1:28180")
                        .header("referer", "http://127.0.0.1:28180/chat")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers()[header::X_CONTENT_TYPE_OPTIONS],
                "nosniff"
            );
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            let body: serde_json::Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            let url = url::Url::parse(body["url"].as_str().unwrap()).unwrap();
            assert_eq!(
                url.origin().ascii_serialization(),
                "http://dar.localhost:28181"
            );
            assert_eq!(url.path(), format!("/open/{id}"));
            urls.push(url);
        }
        assert_ne!(urls[0].query(), urls[1].query());
        let url = &urls[0];
        let path = format!("{}?{}", url.path(), url.query().unwrap());
        let response = sandbox
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(&path)
                    .header("host", "dar.localhost:28181")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "text/html");
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let csp = response.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap();
        assert!(csp.contains("sandbox allow-scripts allow-popups allow-popups-to-escape-sandbox"));
        assert!(csp.contains("connect-src 'none'"));
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "<h1>Quarterly report</h1>"
        );
        for (path, host) in [
            (
                format!("/open/{id}?session=test&ticket=wrong"),
                "dar.localhost:28181",
            ),
            (
                path.replace("session=test", "session=other"),
                "dar.localhost:28181",
            ),
            (path, "draw.localhost:28181"),
        ] {
            let response = sandbox
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(path)
                        .header("host", host)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            assert_eq!(
                response.headers()[header::X_CONTENT_TYPE_OPTIONS],
                "nosniff"
            );
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        }
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/other/apps/{id}/open"))
                    .header("host", "127.0.0.1:28180")
                    .header("referer", "http://127.0.0.1:28180/chat")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn artifact_open_serves_supported_types_and_refuses_other_binary_files() {
        use dar_extension_sdk::artifacts::{ArtifactMetadataInput, ArtifactStore, ExportRoot};
        use dar_extension_sdk::mcp_apps::read_result;
        let (state, template_id, _) = artifact_http_state().await;
        let template = read_result(&state.root, &template_id, "artifact_publish")
            .unwrap()
            .unwrap();
        let store = state
            .start
            .get()
            .unwrap()
            .host
            .services
            .get_named::<ArtifactStore>("artifact-store")
            .unwrap();
        let session = state.session("test").await.unwrap();
        let app = router(Arc::clone(&state));
        let sandbox = apps::sandbox_router(Arc::clone(&state));
        for (filename, media_type, bytes, content_type, csp) in [
            (
                "report.pdf",
                Some("application/pdf"),
                "%PDF-1.7\n",
                "application/pdf",
                "default-src 'none'; frame-ancestors 'none'; sandbox",
            ),
            (
                "note.md",
                Some("text/markdown"),
                "# A note",
                "text/html; charset=utf-8",
                "sandbox allow-scripts allow-popups allow-popups-to-escape-sandbox",
            ),
            (
                "picture.png",
                Some("image/png"),
                "PNG",
                "image/png",
                "default-src 'none'; img-src 'self'",
            ),
            (
                "picture.jpg",
                Some("image/jpeg"),
                "JPEG",
                "image/jpeg",
                "default-src 'none'; img-src 'self'",
            ),
            (
                "picture.gif",
                Some("image/gif"),
                "GIF",
                "image/gif",
                "default-src 'none'; img-src 'self'",
            ),
            (
                "picture.webp",
                Some("image/webp"),
                "WEBP",
                "image/webp",
                "default-src 'none'; img-src 'self'",
            ),
            (
                "picture.svg",
                Some("image/svg+xml"),
                "<svg/>",
                "image/svg+xml",
                "default-src 'none'; style-src 'unsafe-inline'; sandbox",
            ),
            (
                "note.txt",
                Some(" TEXT/PLAIN; charset=utf-8 "),
                "plain text",
                "text/plain; charset=utf-8",
                "default-src 'none'",
            ),
            (
                "data.json",
                Some("application/json"),
                "{\"count\":2}",
                "text/plain; charset=utf-8",
                "default-src 'none'",
            ),
            (
                "main.rs",
                None,
                "fn main() {}",
                "text/plain; charset=utf-8",
                "default-src 'none'",
            ),
            ("download.zip", Some("application/zip"), "ZIP", "", ""),
            (
                "opaque.html",
                Some("application/octet-stream"),
                "<h1>binary</h1>",
                "",
                "",
            ),
        ] {
            fs::write(state.root.join("exports").join(filename), bytes).unwrap();
            let metadata = store
                .stage_from_export_root(
                    &ExportRoot::open(state.root.join("exports")).unwrap(),
                    filename,
                    ArtifactMetadataInput {
                        filename: filename.into(),
                        media_type: media_type.map(str::to_owned),
                        caption: None,
                    },
                )
                .unwrap();
            let mut record = template.clone();
            record.args = serde_json::json!({"path":filename});
            record.result["structuredContent"] = serde_json::json!({"uri":format!("dar-artifact://{}",metadata.id),"filename":filename,"mediaType":media_type,"bytes":bytes.len()});
            let id = record.write(&state.root).unwrap();
            session.publish(ChatEvent::ToolCall {
                id: id.clone(),
                name: "artifact_publish".into(),
                args: record.args.to_string(),
            });
            session.publish(ChatEvent::ToolOutput {
                id: id.clone(),
                text: format!("published [mcp-app-result:{id}]"),
                is_error: false,
                done: true,
            });
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(format!("/test/apps/{id}/open"))
                        .header("host", "127.0.0.1:28180")
                        .header("referer", "http://127.0.0.1:28180/chat")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            if content_type.is_empty() {
                assert_eq!(response.status(), StatusCode::NOT_FOUND, "{filename}");
                assert_eq!(
                    response.headers()[header::X_CONTENT_TYPE_OPTIONS],
                    "nosniff"
                );
                assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
                let response = app
                    .clone()
                    .oneshot(
                        axum::http::Request::builder()
                            .uri(format!("/test/apps/{id}"))
                            .header("host", "127.0.0.1:28180")
                            .header("referer", "http://127.0.0.1:28180/chat")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let body: serde_json::Value = serde_json::from_slice(
                    &response.into_body().collect().await.unwrap().to_bytes(),
                )
                .unwrap();
                let url = url::Url::parse(body["contentUrl"].as_str().unwrap()).unwrap();
                let response = sandbox
                    .clone()
                    .oneshot(
                        axum::http::Request::builder()
                            .uri(format!("/open/{id}?{}", url.query().unwrap()))
                            .header("host", "dar.localhost:28181")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::NOT_FOUND, "{filename}");
                continue;
            }
            assert_eq!(response.status(), StatusCode::OK, "{filename}");
            let body: serde_json::Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            let url = url::Url::parse(body["url"].as_str().unwrap()).unwrap();
            let response = sandbox
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(format!("{}?{}", url.path(), url.query().unwrap()))
                        .header("host", "dar.localhost:28181")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{filename}");
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                content_type,
                "{filename}"
            );
            assert!(
                response.headers()[header::CONTENT_SECURITY_POLICY]
                    .to_str()
                    .unwrap()
                    .contains(csp),
                "{filename}"
            );
            assert_eq!(
                response.headers()[header::X_CONTENT_TYPE_OPTIONS],
                "nosniff"
            );
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            if filename.ends_with(".pdf") {
                assert_eq!(response.headers()[header::CONTENT_DISPOSITION], "inline");
            }
            if !filename.ends_with(".md") {
                assert_eq!(
                    response.into_body().collect().await.unwrap().to_bytes(),
                    bytes,
                    "{filename}"
                );
            }
        }
    }

    #[tokio::test]
    async fn artifact_open_refuses_third_party_records_even_with_valid_tickets() {
        let state = apps_http_state(Config::default(), Arc::new(AppCatalog)).await;
        let id = "0123456789abcdef0123456789abcdef";
        let app = router(Arc::clone(&state));
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/test/apps/{id}/open"))
                    .header("host", "127.0.0.1:28180")
                    .header("referer", "http://127.0.0.1:28180/chat")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/test/apps/{id}"))
                    .header("host", "127.0.0.1:28180")
                    .header("referer", "http://127.0.0.1:28180/chat")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        let url = url::Url::parse(body["contentUrl"].as_str().unwrap()).unwrap();
        let response = apps::sandbox_router(state)
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/open/{id}?{}", url.query().unwrap()))
                    .header("host", "draw.localhost:28181")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }

    #[tokio::test]
    async fn artifact_open_errors_keep_security_headers_when_query_is_missing() {
        let (state, id, _) = artifact_http_state().await;
        let response = apps::sandbox_router(state)
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/open/{id}"))
                    .header("host", "dar.localhost:28181")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response
                .headers()
                .get(header::X_CONTENT_TYPE_OPTIONS)
                .and_then(|value| value.to_str().ok()),
            Some("nosniff")
        );
        assert_eq!(
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-store")
        );
    }

    #[tokio::test]
    async fn artifact_transcript_describes_files_and_marks_only_replay_historical() {
        let (state, _, _) = artifact_http_state().await;
        let response = router(Arc::clone(&state))
            .oneshot(
                axum::http::Request::builder()
                    .uri("/test/stream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let mut body = response.into_body();
        let mut events = Vec::new();
        for _ in 0..2 {
            let frame = body.frame().await.unwrap().unwrap().into_data().unwrap();
            let text = std::str::from_utf8(&frame).unwrap();
            let data = text
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .unwrap();
            events.push(serde_json::from_str::<serde_json::Value>(data).unwrap());
        }
        assert_eq!(events[1]["app"]["artifactFilename"], "report.html");
        assert_eq!(events[1]["app"]["artifactTitle"], "Quarterly report");
        assert_eq!(events[1]["app"]["artifactMediaType"], "text/html");
        assert_eq!(events[1]["app"]["artifactBytes"], 25);
        assert!(events.iter().all(|event| event["historical"] == true));
        state
            .session("test")
            .await
            .unwrap()
            .publish(ChatEvent::Delta {
                role: ChatRole::Assistant,
                text: "live after replay".into(),
            });
        let frame = tokio::time::timeout(std::time::Duration::from_secs(1), body.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap();
        let data = std::str::from_utf8(&frame)
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap();
        let event: serde_json::Value = serde_json::from_str(data).unwrap();
        assert_eq!(event["text"], "live after replay");
        assert!(event.get("historical").is_none());
    }

    // ALG-534 Testing Decisions approve the public JSON-RPC seam.
    // Catches refusals from artifact views; misses browser message relay isolation.
    #[tokio::test]
    async fn artifact_views_refuse_messages_and_model_context() {
        let (state, id, _) = artifact_http_state().await;
        let app = router(state);
        for (method, params) in [
            (
                "ui/update-model-context",
                serde_json::json!({"structuredContent":{"selection":"forged context"}}),
            ),
            (
                "ui/message",
                serde_json::json!({"role":"user","content":{"type":"text","text":"forged message"}}),
            ),
        ] {
            let response = app.clone().oneshot(axum::http::Request::builder().method("POST")
                .uri(format!("/test/apps/{id}/rpc")).header("content-type","application/json")
                .body(Body::from(serde_json::to_vec(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})).unwrap())).unwrap()).await.unwrap();
            let body: serde_json::Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            assert_eq!(body["error"]["code"], -32000, "{method}: {body}");
            assert_eq!(
                body["error"]["message"],
                "artifact views cannot send messages or update model context"
            );
        }
    }

    struct AppCatalog;
    impl dar_extension_sdk::mcp_apps::McpApps for AppCatalog {
        fn list_tools(&self) -> dar_extension_sdk::BoxFuture<'_, Result<serde_json::Value>> {
            Box::pin(async {
                Ok(serde_json::json!([
                    {"server":"draw"}, {"server":"canva"}, {"server":"UPPER"}, {"server":"OTHER"}
                ]))
            })
        }
        fn read_ui_resource<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
        ) -> dar_extension_sdk::BoxFuture<'a, Result<serde_json::Value>> {
            Box::pin(async {
                Ok(serde_json::json!({"html":"<!doctype html><p>Canvas ready</p>","meta":{}}))
            })
        }
        fn call_tool<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: serde_json::Value,
        ) -> dar_extension_sdk::BoxFuture<'a, Result<serde_json::Value>> {
            Box::pin(async { anyhow::bail!("not available") })
        }
        fn read_resource<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
        ) -> dar_extension_sdk::BoxFuture<'a, Result<serde_json::Value>> {
            Box::pin(async { anyhow::bail!("not available") })
        }
    }
    // Approved HTTP seam: ALG-530 comment d1fef21c-b9cc-4116-85c9-c00796ceda3e, section 5.
    // Catches host/session/resource binding; misses browser enforcement of headers.
    #[tokio::test]
    async fn app_bootstrap_returns_html_and_full_result_only_to_owning_session() {
        let root = test_root();
        let records = root.join("data/mcp-apps/results");
        fs::create_dir_all(&records).unwrap();
        let id = "0123456789abcdef0123456789abcdef";
        fs::write(records.join(format!("{id}.json")), serde_json::to_vec(&serde_json::json!({
            "tool":"draw__canvas", "server":"draw", "resource_uri":"ui://canvas", "args":{"shape":"box"},
            "result":{"content":[{"type":"text","text":"visible"}],"structuredContent":{"count":1},"_meta":{"secretViewData":17}}, "created_at":now_ms()
        })).unwrap()).unwrap();
        let mut services = dar_extension_sdk::ServiceRegistry::default();
        services
            .register::<dyn dar_extension_sdk::mcp_apps::McpApps>(
                dar_extension_sdk::mcp_apps::MCP_APPS_SERVICE,
                Arc::new(AppCatalog),
            )
            .unwrap();
        let config = Config::default();
        config.apps.bound_port.set(28181).unwrap();
        let state = Arc::new(AppState {
            config,
            root,
            start: std::sync::OnceLock::from(start_ctx(services)),
            sessions: Mutex::new(HashMap::new()),
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
            meta: Mutex::new(HashMap::new()),
        });
        let session = state.session("test").await.unwrap();
        session.publish(ChatEvent::ToolCall {
            id: "c".into(),
            name: "draw__canvas".into(),
            args: "{}".into(),
        });
        session.publish(ChatEvent::ToolOutput {
            id: "c".into(),
            text: format!("visible [mcp-app-result:{id}]"),
            is_error: false,
            done: true,
        });
        let sandbox_app = apps::sandbox_router(Arc::clone(&state));
        let app = router(state);
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/test/apps/{id}"))
                    .header("host", "CHAT.localhost:28180")
                    .header("referer", "http://chat.localhost:28180/chat")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["html"], "<!doctype html><p>Canvas ready</p>");
        assert_eq!(body["result"]["_meta"]["secretViewData"], 17);
        assert_eq!(body["sandboxOrigin"], "http://draw.localhost:28181");
        let sandbox = url::Url::parse(body["sandboxUrl"].as_str().unwrap()).unwrap();
        let content = url::Url::parse(body["contentUrl"].as_str().unwrap()).unwrap();
        assert_eq!(sandbox.path(), format!("/sandbox/{id}"));
        assert_eq!(content.path(), format!("/content/{id}"));
        assert_eq!(sandbox.query(), content.query());
        let content_path = format!("{}?{}", content.path(), content.query().unwrap());
        let response = sandbox_app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(&content_path)
                    .header("host", "DRAW.localhost:28181")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        let csp = response.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap();
        assert!(
            csp.contains("connect-src 'none'; frame-src 'none';"),
            "{csp}"
        );
        assert!(
            csp.contains(
                "frame-ancestors http://chat.localhost:28180 http://draw.localhost:28181;"
            ),
            "{csp}"
        );
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "<!doctype html><p>Canvas ready</p>"
        );
        let proxy_path = format!("{}?{}", sandbox.path(), sandbox.query().unwrap());
        let response = sandbox_app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(&proxy_path)
                    .header("host", "draw.localhost:28181")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let csp = response.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap();
        assert!(
            csp.contains("frame-src http://draw.localhost:28181;"),
            "{csp}"
        );
        assert!(csp.contains("worker-src 'none';"), "{csp}");
        for path in [&content_path, &proxy_path] {
            for host in [
                "other.localhost:28181",
                "127.0.0.1:28180",
                "draw.localhost:28182",
            ] {
                let response = sandbox_app
                    .clone()
                    .oneshot(
                        axum::http::Request::builder()
                            .uri(path)
                            .header("host", host)
                            .header("x-forwarded-host", "draw.localhost:28181")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::NOT_FOUND, "{host}");
            }
            let response = sandbox_app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(path.replace("session=test", "session=other"))
                        .header("host", "draw.localhost:28181")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/other/apps/{id}"))
                    .header("host", "chat.localhost:28180")
                    .header("referer", "http://chat.localhost:28180/chat")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        fs::remove_file(records.join(format!("{id}.json"))).unwrap();
        for path in [&content_path, &proxy_path] {
            let response = sandbox_app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(path)
                        .header("host", "draw.localhost:28181")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
    }

    async fn apps_http_state(
        config: Config,
        catalog: Arc<dyn dar_extension_sdk::mcp_apps::McpApps>,
    ) -> Arc<AppState> {
        let root = test_root();
        let records = root.join("data/mcp-apps/results");
        fs::create_dir_all(&records).unwrap();
        let mut services = dar_extension_sdk::ServiceRegistry::default();
        services
            .register::<dyn dar_extension_sdk::mcp_apps::McpApps>(
                dar_extension_sdk::mcp_apps::MCP_APPS_SERVICE,
                catalog,
            )
            .unwrap();
        config.apps.bound_port.set(28181).unwrap();
        let state = Arc::new(AppState {
            config,
            root,
            start: std::sync::OnceLock::from(start_ctx(services)),
            sessions: Mutex::new(HashMap::new()),
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
            meta: Mutex::new(HashMap::new()),
        });
        let session = state.session("test").await.unwrap();
        for (server, id) in [
            ("draw", "0123456789abcdef0123456789abcdef"),
            ("canva", "abcdef0123456789abcdef0123456789"),
            ("UPPER", "11111111111111111111111111111111"),
            ("OTHER", "22222222222222222222222222222222"),
        ] {
            let tool = format!("{server}__canvas");
            fs::write(records.join(format!("{id}.json")), serde_json::to_vec(&serde_json::json!({
                "tool":tool, "server":server, "resource_uri":"ui://canvas", "args":{}, "result":{}, "created_at":now_ms()
            })).unwrap()).unwrap();
            session.publish(ChatEvent::ToolCall {
                id: server.into(),
                name: tool,
                args: "{}".into(),
            });
            session.publish(ChatEvent::ToolOutput {
                id: server.into(),
                text: format!("ready [mcp-app-result:{id}]"),
                is_error: false,
                done: true,
            });
        }
        state
    }

    // Approved HTTP seam, comment d1fef21c-b9cc-4116-85c9-c00796ceda3e section 5.
    // Catches usable isolated defaults for supported server IDs; misses browser DNS resolution.
    #[tokio::test]
    async fn uppercase_app_servers_load_on_distinct_default_origins() {
        let state = apps_http_state(Config::default(), Arc::new(AppCatalog)).await;
        let app = router(Arc::clone(&state));
        let sandbox = apps::sandbox_router(state);
        let mut views = vec![];
        for id in [
            "11111111111111111111111111111111",
            "22222222222222222222222222222222",
        ] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(format!("/test/apps/{id}"))
                        .header("host", "127.0.0.1:28180")
                        .header("referer", "http://127.0.0.1:28180/")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body: serde_json::Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            views.push(body);
        }
        assert_ne!(views[0]["sandboxOrigin"], views[1]["sandboxOrigin"]);
        for (view, other) in [(&views[0], &views[1]), (&views[1], &views[0])] {
            let content = url::Url::parse(view["contentUrl"].as_str().unwrap()).unwrap();
            let other = url::Url::parse(other["sandboxOrigin"].as_str().unwrap()).unwrap();
            for (host, status) in [(&content, StatusCode::OK), (&other, StatusCode::NOT_FOUND)] {
                let response = sandbox
                    .clone()
                    .oneshot(
                        axum::http::Request::builder()
                            .uri(format!("{}?{}", content.path(), content.query().unwrap()))
                            .header(
                                "host",
                                &host[url::Position::BeforeHost..url::Position::AfterPort],
                            )
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), status);
                if status == StatusCode::OK {
                    assert_eq!(
                        response.into_body().collect().await.unwrap().to_bytes(),
                        "<!doctype html><p>Canvas ready</p>"
                    );
                }
            }
        }
    }

    // Approved public HTTP seam, comment d1fef21c-b9cc-4116-85c9-c00796ceda3e section 5.
    // Catches configured host assignment and chat-origin validation; misses browser isolation.
    #[tokio::test]
    async fn app_origin_configuration_keeps_local_servers_separate_and_remote_sharing_explicit() {
        let id = "0123456789abcdef0123456789abcdef";
        for (apps, chat, expected) in [
            (
                serde_json::json!({}),
                "http://127.0.0.1:28180",
                Some("http://draw.localhost:28181"),
            ),
            (
                serde_json::json!({"sandbox_origins":{"draw":"http://localhost:28181"}}),
                "http://127.0.0.1:28180",
                Some("http://localhost:28181"),
            ),
            (
                serde_json::json!({"sandbox_origins":{"draw":"http://localhost:28181"}}),
                "http://localhost:28180",
                None,
            ),
            (
                serde_json::json!({"sandbox_origins":{"draw":"loopback"}}),
                "http://localhost:28180",
                Some("http://127.0.0.1:28181"),
            ),
            (
                serde_json::json!({"sandbox_origins":{"draw":"loopback"}}),
                "http://127.0.0.1:28180",
                Some("http://localhost:28181"),
            ),
            (
                serde_json::json!({"sandbox_origins":{"draw":"loopback"}}),
                "https://chat.example",
                None,
            ),
            (
                serde_json::json!({"sandbox_origins":{"draw":"loopback"}}),
                "http://[::1]:28180",
                Some("http://localhost:28181"),
            ),
            (
                serde_json::json!({"sandbox_origins":{"draw":"loopback"}}),
                "http://agent.localhost:28180",
                Some("http://127.0.0.1:28181"),
            ),
            (
                serde_json::json!({"sandbox_origins":{"draw":"loopback","canva":"loopback"}}),
                "http://localhost:28180",
                None,
            ),
            (
                serde_json::json!({"sandbox_origin":"loopback"}),
                "http://127.0.0.1:28180",
                None,
            ),
            (
                serde_json::json!({"sandbox_origins":{"draw":"http://localhost:28181","canva":"http://localhost:28182"}}),
                "http://127.0.0.1:28180",
                None,
            ),
            (
                serde_json::json!({"sandbox_origin":"https://apps.example"}),
                "https://chat.example",
                Some("https://apps.example"),
            ),
            (
                serde_json::json!({"sandbox_origin":"https://apps.example","sandbox_origins":{"canva":"https://apps.example:8443"}}),
                "https://chat.example",
                None,
            ),
            (
                serde_json::json!({"sandbox_origins":{"draw":"data:text/html,unsafe"}}),
                "http://127.0.0.1:28180",
                None,
            ),
            (
                serde_json::json!({"sandbox_origin":"https://fallback.example.edu","sandbox_origins":{"draw":"https://canvas.example.com","canva":"https://other.example.com"}}),
                "https://chat.example.net",
                None,
            ),
            (
                serde_json::json!({"sandbox_origin":"https://fallback.example.edu","sandbox_origins":{"draw":"https://canvas.example.com","canva":"https://other.example.org"}}),
                "https://agent.example.com",
                None,
            ),
            (
                serde_json::json!({"sandbox_origin":"https://fallback.example.edu","sandbox_origins":{"draw":"https://canvas.example.com","canva":"https://deep.canvas.example.com"}}),
                "https://chat.example.net",
                None,
            ),
            (
                serde_json::json!({"sandbox_origin":"https://apps.example.com"}),
                "https://agent.example.com",
                None,
            ),
            (
                serde_json::json!({"sandbox_origin":"https://fallback.example.edu","sandbox_origins":{"draw":"https://canvas.example.net","canva":"https://other.example.org"}}),
                "https://agent.example.com",
                Some("https://canvas.example.net"),
            ),
            (
                serde_json::json!({"sandbox_origins":{"draw":"http://alpha.localhost:28181","canva":"http://deep.alpha.localhost:28181"}}),
                "http://127.0.0.1:28180",
                None,
            ),
            (
                serde_json::json!({"sandbox_origins":{"invalid-id":"http://invalid-id.localhost:28181"}}),
                "http://127.0.0.1:28180",
                Some("http://draw.localhost:28181"),
            ),
            (
                serde_json::json!({"sandbox_origins":{"draw":"http://draw.localhost.:28181"}}),
                "http://127.0.0.1:28180",
                None,
            ),
        ] {
            let config: Config = serde_json::from_value(serde_json::json!({"apps":apps})).unwrap();
            let state = apps_http_state(config, Arc::new(AppCatalog)).await;
            let host = url::Url::parse(chat).unwrap();
            let response = router(state)
                .oneshot(
                    axum::http::Request::builder()
                        .uri(format!("/test/apps/{id}"))
                        .header(
                            "host",
                            &host[url::Position::BeforeHost..url::Position::AfterPort],
                        )
                        .header("referer", format!("{chat}/chat"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            if let Some(expected) = expected {
                assert_eq!(response.status(), StatusCode::OK, "{apps}");
                let body: serde_json::Value = serde_json::from_slice(
                    &response.into_body().collect().await.unwrap().to_bytes(),
                )
                .unwrap();
                assert_eq!(body["sandboxOrigin"], expected);
            } else {
                assert_eq!(response.status(), StatusCode::NOT_FOUND, "{apps}");
            }
        }
        let state = apps_http_state(Config::default(), Arc::new(AppCatalog)).await;
        for referer in [None, Some("http://attacker.example/chat")] {
            let mut request = axum::http::Request::builder()
                .uri(format!("/test/apps/{id}"))
                .header("host", "127.0.0.1:28180")
                .header("x-forwarded-host", "attacker.example");
            if let Some(referer) = referer {
                request = request.header("referer", referer);
            }
            let response = router(Arc::clone(&state))
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
        // `dar dash` rewrites Host to the agent loopback port and forwards the
        // browser-visible host. Only a loopback peer may supply that header.
        let config: Config = serde_json::from_value(
            serde_json::json!({"apps":{"sandbox_origin":"https://apps.example.net"}}),
        )
        .unwrap();
        let state = apps_http_state(config, Arc::new(AppCatalog)).await;
        for (peer, expected) in [
            (Some("127.0.0.1:50000"), StatusCode::OK),
            (Some("100.64.0.7:50000"), StatusCode::NOT_FOUND),
            (None, StatusCode::NOT_FOUND),
        ] {
            let mut request = axum::http::Request::builder()
                .uri(format!("/test/apps/{id}"))
                .header("host", "127.0.0.1:28180")
                .header("x-forwarded-host", "dash.example.com")
                .header("referer", "https://dash.example.com/agent/28180/chat")
                .body(Body::empty())
                .unwrap();
            if let Some(peer) = peer {
                request.extensions_mut().insert(axum::extract::ConnectInfo(
                    peer.parse::<std::net::SocketAddr>().unwrap(),
                ));
            }
            let response = router(Arc::clone(&state)).oneshot(request).await.unwrap();
            assert_eq!(response.status(), expected, "{peer:?}");
        }
    }

    struct ChangingAppCatalog(std::sync::atomic::AtomicUsize);
    impl dar_extension_sdk::mcp_apps::McpApps for ChangingAppCatalog {
        fn list_tools(&self) -> dar_extension_sdk::BoxFuture<'_, Result<serde_json::Value>> {
            Box::pin(async { Ok(serde_json::json!([{"server":"draw"},{"server":"canva"}])) })
        }
        fn read_ui_resource<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
        ) -> dar_extension_sdk::BoxFuture<'a, Result<serde_json::Value>> {
            let version = self.0.fetch_add(1, Ordering::SeqCst) + 1;
            Box::pin(async move {
                Ok(
                    serde_json::json!({"html":format!("<p>Resource version {version}</p>"),
                "meta":{"ui":{"csp":{"resourceDomains":[format!("https://version{version}.example")]}}}}),
                )
            })
        }
        fn call_tool<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: serde_json::Value,
        ) -> dar_extension_sdk::BoxFuture<'a, Result<serde_json::Value>> {
            Box::pin(async { anyhow::bail!("not available") })
        }
        fn read_resource<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
        ) -> dar_extension_sdk::BoxFuture<'a, Result<serde_json::Value>> {
            Box::pin(async { anyhow::bail!("not available") })
        }
    }

    // Approved public HTTP seam, comment d1fef21c-b9cc-4116-85c9-c00796ceda3e section 5.
    // Catches immutable resource/CSP tickets and cross-server record rejection; misses browser CSP.
    #[tokio::test]
    async fn app_tickets_bind_exact_resource_snapshot_and_server_record() {
        let state = apps_http_state(
            Config::default(),
            Arc::new(ChangingAppCatalog(std::sync::atomic::AtomicUsize::new(0))),
        )
        .await;
        let app = router(Arc::clone(&state));
        let sandbox = apps::sandbox_router(state);
        let mut tickets = vec![];
        for _ in 0..2 {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri("/test/apps/0123456789abcdef0123456789abcdef")
                        .header("host", "127.0.0.1:28180")
                        .header("referer", "http://127.0.0.1:28180/")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body: serde_json::Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            tickets.push(body);
        }
        for (ticket, text, domain) in [
            (
                &tickets[0],
                "<p>Resource version 1</p>",
                "https://version1.example",
            ),
            (
                &tickets[1],
                "<p>Resource version 2</p>",
                "https://version2.example",
            ),
        ] {
            let url = url::Url::parse(ticket["contentUrl"].as_str().unwrap()).unwrap();
            let path = format!("{}?{}", url.path(), url.query().unwrap());
            let response = sandbox
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(&path)
                        .header("host", "draw.localhost:28181")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let csp = response.headers()[header::CONTENT_SECURITY_POLICY]
                .to_str()
                .unwrap();
            assert!(csp.contains(domain), "{csp}");
            assert_eq!(
                response.into_body().collect().await.unwrap().to_bytes(),
                text
            );
            for route in ["content", "sandbox"] {
                let path = path.replace("/content/", &format!("/{route}/")).replace(
                    "0123456789abcdef0123456789abcdef",
                    "abcdef0123456789abcdef0123456789",
                );
                let response = sandbox
                    .clone()
                    .oneshot(
                        axum::http::Request::builder()
                            .uri(path)
                            .header("host", "canva.localhost:28181")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::NOT_FOUND);
            }
        }
    }

    struct ContextSession {
        prompts: Arc<std::sync::Mutex<Vec<String>>>,
        reject: Arc<AtomicBool>,
    }
    impl ChatSession for ContextSession {
        fn send_turn(&mut self, prompt: String) -> cap_chat::BoxFuture<'_, Result<()>> {
            let rejected = self.reject.load(Ordering::SeqCst);
            if !rejected {
                self.prompts.lock().unwrap().push(prompt);
            }
            Box::pin(async move {
                if rejected {
                    anyhow::bail!("busy")
                }
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
    // Approved seam: common chat submission from Testing Decisions / Chat glue.
    // Catches latest context delivery, retries and ui/message; misses real runner prompting.
    #[tokio::test]
    async fn view_context_is_consumed_only_after_acceptance_by_both_message_paths() {
        let root = test_root();
        let records = root.join("data/mcp-apps/results");
        fs::create_dir_all(&records).unwrap();
        let id = "0123456789abcdef0123456789abcdef";
        fs::write(records.join(format!("{id}.json")), serde_json::to_vec(&serde_json::json!({"tool":"draw__canvas","server":"draw","resource_uri":"ui://canvas","args":{},"result":{},"created_at":now_ms()})).unwrap()).unwrap();
        let state = slot_state(root, HashMap::new());
        let session = state.session("test").await.unwrap();
        let prompts = Arc::new(std::sync::Mutex::new(vec![]));
        let reject = Arc::new(AtomicBool::new(true));
        *session.inner.lock().await = Some(Box::new(ContextSession {
            prompts: prompts.clone(),
            reject: reject.clone(),
        }));
        session.publish(ChatEvent::ToolCall {
            id: "c".into(),
            name: "draw__canvas".into(),
            args: "{}".into(),
        });
        session.publish(ChatEvent::ToolOutput {
            id: "c".into(),
            text: format!("fallback [mcp-app-result:{id}]"),
            is_error: false,
            done: true,
        });
        let app = router(state);
        let rpc = |method: &str, params: serde_json::Value| {
            axum::http::Request::builder().method("POST").uri(format!("/test/apps/{id}/rpc")).header("content-type","application/json").body(Body::from(serde_json::to_vec(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})).unwrap())).unwrap()
        };
        let response = app
            .clone()
            .oneshot(rpc(
                "ui/update-model-context",
                serde_json::json!({"structuredContent":{"selection":"box-7"}}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["result"], serde_json::json!({}));
        assert!(body.get("error").is_none());
        let send = |command: &str| {
            axum::http::Request::builder()
                .method("POST")
                .uri("/test/send")
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"command_id":"{command}","message":"move it"}}"#
                )))
                .unwrap()
        };
        assert_eq!(
            app.clone().oneshot(send("failed")).await.unwrap().status(),
            StatusCode::CONFLICT
        );
        reject.store(false, Ordering::SeqCst);
        assert_eq!(
            app.clone().oneshot(send("retry")).await.unwrap().status(),
            StatusCode::ACCEPTED
        );
        assert!(prompts.lock().unwrap()[0].contains("box-7"));
        assert_eq!(
            app.clone().oneshot(send("next")).await.unwrap().status(),
            StatusCode::ACCEPTED
        );
        assert!(!prompts.lock().unwrap()[1].contains("box-7"));
        app.clone()
            .oneshot(rpc(
                "ui/update-model-context",
                serde_json::json!({"structuredContent":{"selection":"circle-3"}}),
            ))
            .await
            .unwrap();
        let response = app
            .oneshot(rpc(
                "ui/message",
                serde_json::json!({"role":"user","content":{"type":"text","text":"color it"}}),
            ))
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert!(body.get("result").is_some(), "{body}");
        let prompts = prompts.lock().unwrap();
        assert!(prompts[2].contains("circle-3") && prompts[2].contains("color it"));
    }

    struct HangingAppCatalog {
        called: Arc<tokio::sync::Notify>,
    }
    impl dar_extension_sdk::mcp_apps::McpApps for HangingAppCatalog {
        fn list_tools(&self) -> dar_extension_sdk::BoxFuture<'_, Result<serde_json::Value>> {
            AppCatalog.list_tools()
        }
        fn read_ui_resource<'a>(
            &'a self,
            server: &'a str,
            uri: &'a str,
        ) -> dar_extension_sdk::BoxFuture<'a, Result<serde_json::Value>> {
            AppCatalog.read_ui_resource(server, uri)
        }
        fn call_tool<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: serde_json::Value,
        ) -> dar_extension_sdk::BoxFuture<'a, Result<serde_json::Value>> {
            Box::pin(async move {
                self.called.notify_one();
                std::future::pending().await
            })
        }
        fn read_resource<'a>(
            &'a self,
            server: &'a str,
            uri: &'a str,
        ) -> dar_extension_sdk::BoxFuture<'a, Result<serde_json::Value>> {
            AppCatalog.read_resource(server, uri)
        }
    }
    // Approved seam: app tools/call and normal chat submission HTTP interfaces.
    // Catches a stalled upstream blocking chat or waiting forever; misses browser timers.
    #[tokio::test]
    async fn hanging_app_call_times_out_while_normal_chat_still_accepts_turns() {
        let root = test_root();
        let records = root.join("data/mcp-apps/results");
        fs::create_dir_all(&records).unwrap();
        let id = "0123456789abcdef0123456789abcdef";
        fs::write(records.join(format!("{id}.json")),serde_json::to_vec(&serde_json::json!({"tool":"draw__canvas","server":"draw","resource_uri":"ui://canvas","args":{},"result":{},"created_at":now_ms()})).unwrap()).unwrap();
        let called = Arc::new(tokio::sync::Notify::new());
        let mut services = dar_extension_sdk::ServiceRegistry::default();
        services
            .register::<dyn dar_extension_sdk::mcp_apps::McpApps>(
                dar_extension_sdk::mcp_apps::MCP_APPS_SERVICE,
                Arc::new(HangingAppCatalog {
                    called: called.clone(),
                }),
            )
            .unwrap();
        let state = Arc::new(AppState {
            config: Config::default(),
            root,
            start: std::sync::OnceLock::from(start_ctx(services)),
            sessions: Mutex::new(HashMap::new()),
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
            meta: Mutex::new(HashMap::new()),
        });
        let session = state.session("test").await.unwrap();
        let sends = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        *session.inner.lock().await = Some(Box::new(FakeSession {
            aborted: Arc::new(AtomicBool::new(false)),
            sends: sends.clone(),
            abort_fails: false,
        }));
        session.publish(ChatEvent::ToolCall {
            id: "c".into(),
            name: "draw__canvas".into(),
            args: "{}".into(),
        });
        session.publish(ChatEvent::ToolOutput {
            id: "c".into(),
            text: format!("fallback [mcp-app-result:{id}]"),
            is_error: false,
            done: true,
        });
        let app = router(state);
        let app_request=axum::http::Request::builder().method("POST").uri(format!("/test/apps/{id}/rpc")).header("content-type","application/json").body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"refresh","arguments":{}}}"#)).unwrap();
        let waiting = tokio::spawn(app.clone().oneshot(app_request));
        tokio::time::timeout(std::time::Duration::from_secs(1), called.notified())
            .await
            .unwrap();
        let send = axum::http::Request::builder()
            .method("POST")
            .uri("/test/send")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"command_id":"while-pending","message":"keep working"}"#,
            ))
            .unwrap();
        let response =
            tokio::time::timeout(std::time::Duration::from_millis(100), app.oneshot(send))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(sends.load(Ordering::SeqCst), 1);
        let response = tokio::time::timeout(std::time::Duration::from_secs(2), waiting)
            .await
            .expect("view call did not time out")
            .unwrap()
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("timed out"),
            "{body}"
        );
    }

    // Approved seam: each view's context through app RPC and public chat submission.
    // Catches context lost between opted-out views; misses iframe identity in the browser.
    #[tokio::test]
    async fn independent_views_keep_each_selection_when_reuse_is_disabled() {
        let root = test_root();
        let records = root.join("data/mcp-apps/results");
        fs::create_dir_all(&records).unwrap();
        let mut config = Config::default();
        config.apps.no_reuse = vec!["draw".into()];
        let state = Arc::new(AppState {
            config,
            root,
            start: std::sync::OnceLock::new(),
            sessions: Mutex::new(HashMap::new()),
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
            meta: Mutex::new(HashMap::new()),
        });
        let session = state.session("test").await.unwrap();
        let prompts = Arc::new(std::sync::Mutex::new(vec![]));
        *session.inner.lock().await = Some(Box::new(ContextSession {
            prompts: prompts.clone(),
            reject: Arc::new(AtomicBool::new(false)),
        }));
        let app = router(state.clone());
        for (id, selection) in [
            ("0123456789abcdef0123456789abcdef", "box-A"),
            ("fedcba9876543210fedcba9876543210", "box-B"),
        ] {
            fs::write(records.join(format!("{id}.json")),serde_json::to_vec(&serde_json::json!({"tool":"draw__canvas","server":"draw","resource_uri":"ui://canvas","args":{},"result":{},"created_at":now_ms()})).unwrap()).unwrap();
            session.publish(ChatEvent::ToolCall {
                id: id.into(),
                name: "draw__canvas".into(),
                args: "{}".into(),
            });
            session.publish(ChatEvent::ToolOutput {
                id: id.into(),
                text: format!("fallback [mcp-app-result:{id}]"),
                is_error: false,
                done: true,
            });
            let request=axum::http::Request::builder().method("POST").uri(format!("/test/apps/{id}/rpc")).header("content-type","application/json").body(Body::from(serde_json::to_vec(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"ui/update-model-context","params":{"structuredContent":{"selection":selection}}})).unwrap())).unwrap();
            assert_eq!(
                app.clone().oneshot(request).await.unwrap().status(),
                StatusCode::OK
            );
        }
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/test/send")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"command_id":"two-selections","message":"compare the selections"}"#,
            ))
            .unwrap();
        assert_eq!(
            app.oneshot(request).await.unwrap().status(),
            StatusCode::ACCEPTED
        );
        let prompts = prompts.lock().unwrap();
        assert!(
            prompts[0].contains("box-A") && prompts[0].contains("box-B"),
            "{}",
            prompts[0]
        );
    }

    // Approved seam: backend archive resume and SSE transcript replay.
    // Catches old Pi/builtin marker leakage and lost views; misses browser rendering.
    #[tokio::test]
    async fn old_runner_archives_restore_views_without_exposing_markers() {
        for backend in ["pi", "builtin"] {
            let root = test_root();
            let records = root.join("data/mcp-apps/results");
            fs::create_dir_all(&records).unwrap();
            let id = "0123456789abcdef0123456789abcdef";
            fs::write(records.join(format!("{id}.json")),serde_json::to_vec(&serde_json::json!({"tool":"draw__canvas","server":"draw","resource_uri":"ui://canvas","args":{"shape":"box"},"result":{"content":[{"type":"text","text":"visible"}]},"created_at":now_ms()})).unwrap()).unwrap();
            let dir = root.join("data/chat/sessions");
            fs::create_dir_all(&dir).unwrap();
            let marker = format!("visible [mcp-app-result:{id}]");
            let (call, result) = if backend == "pi" {
                (
                    serde_json::json!({"type":"message","message":{"role":"assistant","content":[{"type":"toolCall","id":"c","name":"draw__canvas","arguments":{"shape":"box"}}]}}),
                    serde_json::json!({"type":"message","message":{"role":"toolResult","toolCallId":"c","toolName":"draw__canvas","content":[{"type":"text","text":marker}],"isError":false}}),
                )
            } else {
                (
                    serde_json::json!({"type":"message","message":{"role":"assistant","content":"","tool_calls":[{"id":"c","type":"function","function":{"name":"draw__canvas","arguments":"{\"shape\":\"box\"}"}}]}}),
                    serde_json::json!({"type":"message","message":{"role":"tool","tool_call_id":"c","content":serde_json::to_string(&marker).unwrap()}}),
                )
            };
            fs::write(
                dir.join("2026-01-01_old.jsonl"),
                format!(
                    "{}\n{call}\n{result}\n",
                    serde_json::json!({"type":"session","id":"old","backend":backend})
                ),
            )
            .unwrap();
            let app = router(slot_state(root, HashMap::new()));
            let resume = axum::http::Request::builder()
                .method("POST")
                .uri("/main/resume")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"id":"old"}"#))
                .unwrap();
            assert_eq!(
                app.clone().oneshot(resume).await.unwrap().status(),
                StatusCode::ACCEPTED
            );
            let response = app
                .oneshot(
                    axum::http::Request::builder()
                        .uri("/old/stream")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let mut body = response.into_body();
            let mut transcript = String::new();
            for _ in 0..2 {
                transcript.push_str(
                    std::str::from_utf8(&body.frame().await.unwrap().unwrap().into_data().unwrap())
                        .unwrap(),
                );
            }
            assert!(
                !transcript.contains("[mcp-app-result:"),
                "{backend}: {transcript}"
            );
            assert!(
                transcript.contains("recordId") && transcript.contains("ui://canvas"),
                "{backend}: {transcript}"
            );
        }
    }

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
            slot: "test".into(),
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
            resolved_live_id: std::sync::Mutex::new(None),
            last_active_ms: std::sync::atomic::AtomicU64::new(now_ms()),
            abort_signal: watch::channel(false).0,
            publish_lock: std::sync::Mutex::new(()),
            command_ids: Mutex::new(HashSet::new()),
            app_context: std::sync::Mutex::new(HashMap::new()),
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
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
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

    struct EnvironmentBackend {
        prompts: mpsc::UnboundedSender<Option<String>>,
        turns: mpsc::UnboundedSender<String>,
    }

    struct EnvironmentSession(mpsc::UnboundedSender<String>);

    impl ChatSession for EnvironmentSession {
        fn send_turn(&mut self, prompt: String) -> chat::BoxFuture<'_, Result<()>> {
            self.0.send(prompt).unwrap();
            Box::pin(async { Ok(()) })
        }
        fn abort(&mut self) -> chat::BoxFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn close(self: Box<Self>) -> chat::BoxFuture<'static, Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    impl ChatBackend for EnvironmentBackend {
        fn open<'a>(
            &'a self,
            params: chat::ChatSessionParams,
            _events: mpsc::Sender<ChatEvent>,
        ) -> chat::BoxFuture<'a, Result<Box<dyn ChatSession>>> {
            self.prompts.send(params.system_prompt).unwrap();
            let turns = self.turns.clone();
            Box::pin(async move { Ok(Box::new(EnvironmentSession(turns)) as Box<dyn ChatSession>) })
        }
    }

    // ALG-546: Thinh approved extension lifecycle + public routes/coordinator
    // and ChatBackend observations; catches surface labels, misses wire encoding.
    #[tokio::test]
    async fn chat_sessions_receive_environment_only_in_their_local_system_context() {
        let root = test_root();
        fs::create_dir_all(&root).unwrap();
        let mut register = register_ctx(
            root.clone(),
            serde_json::json!({"backend":"capture","idle_minutes":0}),
        );
        let (shutdown, shutdown_rx) = watch::channel(false);
        register.shutdown = host_api::ShutdownToken::new(shutdown_rx);
        let (prompts, mut captured) = mpsc::unbounded_channel();
        let (turns, mut user_turns) = mpsc::unbounded_channel();
        register
            .services
            .register::<dyn ChatBackend>("capture", Arc::new(EnvironmentBackend { prompts, turns }))
            .unwrap();
        register
            .bus
            .register_retained(
                chat::SYSTEM_CONTEXT_TOPIC,
                chat::SystemContext {
                    text: "Agent identity".into(),
                    files: vec![chat::SystemContextFile {
                        path: "AGENTS.md".into(),
                    }],
                    reflect: false,
                },
            )
            .unwrap();
        let extension = ChatWebExtension::default();
        extension.register(&mut register).await.unwrap();
        let start = StartCtx {
            shutdown: register.shutdown.clone(),
            paths: register.paths.clone(),
            config: register.config.clone(),
            host: register.into_start_services().unwrap(),
        };
        extension.start(start.clone()).await.unwrap();
        let app = start.host.router.as_ref().clone();
        let response = app
            .clone()
            .oneshot(post(
                "/chat/browser-only/send",
                Body::from(r#"{"command_id":"one","message":"Hello"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let prompt = captured.try_recv().unwrap().unwrap();
        assert!(
            prompt.contains("[ENVIRONMENT]\ninterface: web chat\n"),
            "{prompt}"
        );
        assert_eq!(user_turns.try_recv().unwrap(), "Hello");
        let coordinator = start
            .host
            .services
            .get_named::<dyn chat::ChatCoordinator>(chat::CHAT_COORDINATOR_SERVICE)
            .unwrap();
        coordinator
            .send_turn("Shared hello".into(), "Shared hello".into())
            .await
            .unwrap();
        let prompt = captured.try_recv().unwrap().unwrap();
        assert!(
            prompt.contains("[ENVIRONMENT]\ninterface: chat\n"),
            "{prompt}"
        );
        assert_eq!(user_turns.try_recv().unwrap(), "Shared hello");
        let response = app
            .clone()
            .oneshot(post(
                "/chat/browser-only/compact",
                Body::from(r#"{"command_id":"two","instructions":"retain conclusions"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            user_turns.try_recv().unwrap(),
            "/compact retain conclusions"
        );
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/chat/browser-only/history")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let history = std::str::from_utf8(&bytes).unwrap();
        assert!(history.contains(r#""text":"Hello""#));
        assert!(history.contains(r#""text":"/compact retain conclusions""#));
        assert!(!history.contains("[ENVIRONMENT]"));
        assert_eq!(
            start
                .host
                .bus
                .read_retained::<chat::SystemContext>(chat::SYSTEM_CONTEXT_TOPIC)
                .unwrap()
                .text,
            "Agent identity"
        );
        extension.stop().await.unwrap();
        drop(shutdown);
        fs::remove_dir_all(root).unwrap();
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
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
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
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
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
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
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
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
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
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
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

    const IDLE: std::time::Duration = std::time::Duration::from_secs(300);

    struct IdleSession {
        closes: Arc<std::sync::atomic::AtomicUsize>,
        hang_close: bool,
    }
    impl ChatSession for IdleSession {
        fn send_turn(&mut self, _prompt: String) -> cap_chat::BoxFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn abort(&mut self) -> cap_chat::BoxFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn close(self: Box<Self>) -> cap_chat::BoxFuture<'static, Result<()>> {
            Box::pin(async move {
                self.closes.fetch_add(1, Ordering::SeqCst);
                if self.hang_close {
                    std::future::pending::<()>().await;
                }
                Ok(())
            })
        }
    }
    struct IdleBackend {
        resumes: Arc<std::sync::Mutex<Vec<Option<String>>>>,
        closes: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl ChatBackend for IdleBackend {
        fn open<'a>(
            &'a self,
            params: cap_chat::ChatSessionParams,
            _tx: mpsc::Sender<ChatEvent>,
        ) -> cap_chat::BoxFuture<'a, Result<Box<dyn ChatSession>>> {
            self.resumes.lock().unwrap().push(params.resume_session_id);
            let closes = Arc::clone(&self.closes);
            Box::pin(async move {
                Ok(Box::new(IdleSession {
                    closes,
                    hang_close: false,
                }) as Box<dyn ChatSession>)
            })
        }
    }
    fn idle_slot(
        closes: &Arc<std::sync::atomic::AtomicUsize>,
        hang_close: bool,
        id: Option<&str>,
    ) -> Arc<Session> {
        let slot = session(Box::new(IdleSession {
            closes: Arc::clone(closes),
            hang_close,
        }));
        *slot.resolved_live_id.lock().unwrap() = id.map(str::to_owned);
        slot.last_active_ms.store(1_000, Ordering::SeqCst);
        slot
    }
    fn idle_state(
        slots: Vec<(&str, Arc<Session>)>,
        services: dar_extension_sdk::ServiceRegistry,
    ) -> AppState {
        AppState {
            config: Config {
                backend: Some("fake".into()),
                ..Config::default()
            },
            root: test_root(),
            start: std::sync::OnceLock::from(start_ctx(services)),
            sessions: Mutex::new(slots.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()),
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
            meta: Mutex::new(HashMap::new()),
        }
    }
    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }
    fn idle_ms() -> u64 {
        IDLE.as_millis() as u64
    }

    #[tokio::test]
    async fn idle_reaper_closes_at_threshold_and_keeps_conversation() {
        let closes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let slot = idle_slot(&closes, false, Some("arch-1"));
        slot.publish_user("hello".into(), vec![]).unwrap();
        slot.last_active_ms.store(1_000, Ordering::SeqCst);
        let state = idle_state(vec![("a", Arc::clone(&slot))], Default::default());
        let before = slot.generation.load(Ordering::SeqCst);

        state.reap_idle(1_000 + idle_ms() - 1, IDLE).await;
        settle().await;
        assert_eq!(closes.load(Ordering::SeqCst), 0);
        assert!(slot.inner.lock().await.is_some());

        state.reap_idle(1_000 + idle_ms(), IDLE).await;
        settle().await;
        assert_eq!(closes.load(Ordering::SeqCst), 1);
        assert!(slot.inner.lock().await.is_none());
        assert_eq!(slot.generation.load(Ordering::SeqCst), before + 1);
        assert_eq!(slot.history.lock().unwrap().len(), 1);
        assert_eq!(
            slot.resolved_live_id.lock().unwrap().as_deref(),
            Some("arch-1")
        );
        assert!(!slot.suppress_resume.load(Ordering::SeqCst));

        state.reap_idle(1_000 + idle_ms() * 3, IDLE).await;
        settle().await;
        assert_eq!(closes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn idle_reaper_never_closes_running_turn_or_unidentified_slot() {
        let closes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let running = idle_slot(&closes, false, Some("arch-1"));
        running.active_turns.store(1, Ordering::SeqCst);
        let anonymous = idle_slot(&closes, false, None);
        let state = idle_state(
            vec![
                ("running", Arc::clone(&running)),
                ("anon", Arc::clone(&anonymous)),
            ],
            Default::default(),
        );

        state.reap_idle(u64::MAX / 2, IDLE).await;
        settle().await;

        assert_eq!(closes.load(Ordering::SeqCst), 0);
        assert!(running.inner.lock().await.is_some());
        assert!(anonymous.inner.lock().await.is_some());
    }

    #[tokio::test]
    async fn idle_reaped_slot_reopens_same_conversation_on_next_send() {
        let resumes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let closes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut services = dar_extension_sdk::ServiceRegistry::default();
        services
            .register::<dyn ChatBackend>(
                "fake",
                Arc::new(IdleBackend {
                    resumes: Arc::clone(&resumes),
                    closes: Arc::clone(&closes),
                }),
            )
            .unwrap();
        let slot = idle_slot(&closes, false, Some("arch-1"));
        let state = idle_state(vec![("main", Arc::clone(&slot))], services);

        state.reap_idle(1_000 + idle_ms(), IDLE).await;
        settle().await;
        chat::ChatCoordinator::send_turn(&state, "again".into(), "again".into())
            .await
            .unwrap();

        assert_eq!(*resumes.lock().unwrap(), vec![Some("arch-1".to_owned())]);
        assert_eq!(slot.active_turns.load(Ordering::SeqCst), 1);
        assert_eq!(
            slot.resolved_live_id.lock().unwrap().as_deref(),
            Some("arch-1")
        );
        // Fresh activity: not idle again until a new threshold passes.
        let reopened_at = slot.last_active_ms.load(Ordering::SeqCst);
        slot.active_turns.store(0, Ordering::SeqCst);
        state.reap_idle(reopened_at, IDLE).await;
        settle().await;
        assert!(slot.inner.lock().await.is_some());
    }

    #[tokio::test]
    async fn idle_reaper_rechecks_after_autonomous_turn_started_under_acceptance_lock() {
        let closes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let slot = idle_slot(&closes, false, Some("arch-1"));
        let state = Arc::new(idle_state(
            vec![("a", Arc::clone(&slot))],
            Default::default(),
        ));
        let generation = slot.generation.load(Ordering::SeqCst);
        // Event pump holds acceptance while publishing.
        let acceptance = slot.acceptance_lock.lock().await;
        let reaping = tokio::spawn({
            let state = Arc::clone(&state);
            async move { state.reap_idle(1_000 + idle_ms(), IDLE).await }
        });
        settle().await;
        assert!(
            !reaping.is_finished(),
            "reaper must wait for acceptance lock"
        );
        assert!(slot.publish_if_current(
            generation,
            ChatEvent::TurnStarted {
                origin: TurnOrigin::Autonomous
            }
        ));
        drop(acceptance);
        reaping.await.unwrap();
        settle().await;

        assert_eq!(closes.load(Ordering::SeqCst), 0);
        assert!(slot.inner.lock().await.is_some());
        assert_eq!(slot.generation.load(Ordering::SeqCst), generation);
        assert_eq!(slot.active_turns.load(Ordering::SeqCst), 1);
    }

    struct FailingOpenBackend;
    impl ChatBackend for FailingOpenBackend {
        fn open<'a>(
            &'a self,
            _params: cap_chat::ChatSessionParams,
            _tx: mpsc::Sender<ChatEvent>,
        ) -> cap_chat::BoxFuture<'a, Result<Box<dyn ChatSession>>> {
            Box::pin(async { anyhow::bail!("resume failed") })
        }
    }

    #[tokio::test]
    async fn idle_reopen_failure_keeps_history_and_id_and_accepts_nothing() {
        let closes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut services = dar_extension_sdk::ServiceRegistry::default();
        services
            .register::<dyn ChatBackend>("fake", Arc::new(FailingOpenBackend))
            .unwrap();
        let slot = idle_slot(&closes, false, Some("arch-1"));
        slot.publish_user("hello".into(), vec![]).unwrap();
        slot.last_active_ms.store(1_000, Ordering::SeqCst);
        let state = idle_state(vec![("main", Arc::clone(&slot))], services);
        state.reap_idle(1_000 + idle_ms(), IDLE).await;
        settle().await;
        let mut events = slot.events.subscribe();

        assert!(
            chat::ChatCoordinator::send_turn(&state, "again".into(), "again".into())
                .await
                .is_err()
        );

        assert!(slot.inner.lock().await.is_none());
        assert_eq!(slot.active_turns.load(Ordering::SeqCst), 0);
        assert_eq!(slot.history.lock().unwrap().len(), 1);
        assert_eq!(
            slot.resolved_live_id.lock().unwrap().as_deref(),
            Some("arch-1")
        );
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn idle_reaper_skips_slot_with_busy_backend_lock() {
        let closes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let slot = idle_slot(&closes, false, Some("arch-1"));
        let state = idle_state(vec![("a", Arc::clone(&slot))], Default::default());
        let held = slot.inner.lock().await;

        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            state.reap_idle(1_000 + idle_ms(), IDLE),
        )
        .await
        .expect("reaper must not wait on a busy backend lock");
        drop(held);
        settle().await;

        assert_eq!(closes.load(Ordering::SeqCst), 0);
        assert!(slot.inner.lock().await.is_some());
    }

    #[tokio::test]
    async fn idle_reaper_serializes_with_transition_holders() {
        let closes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let slot = idle_slot(&closes, false, Some("arch-1"));
        let state = Arc::new(idle_state(
            vec![("a", Arc::clone(&slot))],
            Default::default(),
        ));
        let transition = state.transition.lock().await;
        let reaping = tokio::spawn({
            let state = Arc::clone(&state);
            async move { state.reap_idle(1_000 + idle_ms(), IDLE).await }
        });
        settle().await;
        assert!(!reaping.is_finished());
        // An in-flight send accepts a turn before releasing transition.
        slot.active_turns.store(1, Ordering::SeqCst);
        drop(transition);
        reaping.await.unwrap();
        settle().await;

        assert_eq!(closes.load(Ordering::SeqCst), 0);
        assert!(slot.inner.lock().await.is_some());
    }

    #[tokio::test]
    async fn idle_reaper_hung_close_does_not_block_others() {
        let closes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hung = idle_slot(&closes, true, Some("arch-1"));
        let ok = idle_slot(&closes, false, Some("arch-2"));
        let state = idle_state(
            vec![("hung", Arc::clone(&hung)), ("ok", Arc::clone(&ok))],
            Default::default(),
        );

        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            state.reap_idle(1_000 + idle_ms(), IDLE),
        )
        .await
        .expect("hung close must not stall the sweep");
        settle().await;

        assert_eq!(closes.load(Ordering::SeqCst), 2);
        assert!(hung.inner.lock().await.is_none());
        assert!(ok.inner.lock().await.is_none());
        // transition lock stays free for other work.
        assert!(state.transition.try_lock().is_ok());
    }

    #[tokio::test]
    async fn rejected_coordinator_send_has_no_user_event_or_active_turn() {
        let s = session(Box::new(RejectingSession));
        let state = AppState {
            config: Config::default(),
            root: test_root(),
            start: std::sync::OnceLock::new(),
            sessions: Mutex::new(HashMap::from([("main".into(), Arc::clone(&s))])),
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
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
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
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
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
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
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
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
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
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
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
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
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
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
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
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
    async fn http_stream_fans_out_and_late_joiner_replays_historical_events() {
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
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
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
        let parse = |text: &str| {
            text.lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .map(|data| serde_json::from_str::<serde_json::Value>(data).unwrap())
                .collect::<Vec<_>>()
        };
        let live_events = parse(&concurrent[0]);
        let mut replay_events = parse(&replay);
        assert!(live_events
            .iter()
            .all(|event| event.get("historical").is_none()));
        assert!(replay_events
            .iter()
            .all(|event| event["historical"] == true));
        for event in &mut replay_events {
            event.as_object_mut().unwrap().remove("historical");
        }
        assert_eq!(live_events, replay_events);

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
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
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
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
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
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
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
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
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
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
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
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
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
    fn tab_shows_escaped_runner_info() {
        let root = std::env::temp_dir().join(format!("chat-web-info-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("agent.yaml"),
            "runner:\n  use: pi\n  provider: acme\n  model: \"m<1>\"\n  effort: high\n",
        )
        .unwrap();
        let tab = ChatTab {
            agent_name: "A".into(),
            agent_description: None,
            agent_avatar: None,
            root: root.clone(),
        };
        // Visible text only: tags become spaces, so each label sits next to its value.
        let text = |html: String| {
            let mut out = String::new();
            let mut in_tag = false;
            for c in html.chars() {
                match c {
                    '<' => in_tag = true,
                    '>' => {
                        in_tag = false;
                        out.push(' ');
                    }
                    c if !in_tag => out.push(c),
                    _ => {}
                }
            }
            out.split_whitespace().collect::<Vec<_>>().join(" ")
        };
        let html = text(tab.render().unwrap());
        assert!(html.contains("Provider acme"));
        assert!(html.contains("Model m&lt;1&gt;"));
        assert!(html.contains("Thinking high"));
        assert!(html.contains("Runner pi"));
        fs::remove_dir_all(&root).unwrap();
        let html = text(ChatTab { root, ..tab }.render().unwrap());
        assert!(html.contains("Provider default") && html.contains("Thinking default"));
    }

    #[test]
    fn tab_fragment_has_a_usable_composer() {
        let tab = ChatTab {
            agent_name: "Test Agent".into(),
            agent_description: Some("Does <things>".into()),
            agent_avatar: None,
            root: std::path::PathBuf::new(),
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

    struct CloseFlagSession(Arc<AtomicBool>);
    impl ChatSession for CloseFlagSession {
        fn send_turn(&mut self, _prompt: String) -> cap_chat::BoxFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn abort(&mut self) -> cap_chat::BoxFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn close(self: Box<Self>) -> cap_chat::BoxFuture<'static, Result<()>> {
            self.0.store(true, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    }

    fn post(uri: &str, body: Body) -> axum::http::Request<Body> {
        axum::http::Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(body)
            .unwrap()
    }

    async fn slot_of(response: Response) -> String {
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["slot"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn slot_state(root: PathBuf, sessions: HashMap<String, Arc<Session>>) -> Arc<AppState> {
        Arc::new(AppState {
            config: Config::default(),
            root,
            start: std::sync::OnceLock::new(),
            sessions: Mutex::new(sessions),
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
            meta: Mutex::new(HashMap::new()),
        })
    }

    #[tokio::test]
    async fn new_endpoint_returns_fresh_slot_and_keeps_old_backend_open() {
        let closed = Arc::new(AtomicBool::new(false));
        let s = session(Box::new(CloseFlagSession(Arc::clone(&closed))));
        s.publish_user("hi".into(), vec![]).unwrap();
        let before = load_transcript(&s.transcript).unwrap().len();
        let state = slot_state(
            test_root(),
            HashMap::from([("test".into(), Arc::clone(&s))]),
        );
        let slot = slot_of(
            router(Arc::clone(&state))
                .oneshot(post("/test/new", Body::empty()))
                .await
                .unwrap(),
        )
        .await;
        assert_ne!(slot, "test");
        assert!(state.sessions.lock().await.contains_key(&slot));
        assert!(!closed.load(Ordering::SeqCst));
        assert!(s.inner.lock().await.is_some());
        assert_eq!(load_transcript(&s.transcript).unwrap().len(), before);
    }

    type EventSenders = Arc<std::sync::Mutex<Vec<mpsc::Sender<ChatEvent>>>>;

    struct ScriptedBackend {
        identities: std::sync::Mutex<VecDeque<Option<String>>>,
        txs: EventSenders,
    }
    impl ChatBackend for ScriptedBackend {
        fn open<'a>(
            &'a self,
            _params: cap_chat::ChatSessionParams,
            tx: mpsc::Sender<ChatEvent>,
        ) -> cap_chat::BoxFuture<'a, Result<Box<dyn ChatSession>>> {
            let identity = self.identities.lock().unwrap().pop_front().flatten();
            self.txs.lock().unwrap().push(tx.clone());
            Box::pin(async move {
                if let Some(id) = identity {
                    tx.send(ChatEvent::SessionIdentity { id }).await?;
                }
                Ok(Box::new(FakeSession {
                    aborted: Arc::new(AtomicBool::new(false)),
                    sends: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                    abort_fails: false,
                }) as Box<dyn ChatSession>)
            })
        }
    }

    fn scripted_state(identities: Vec<Option<String>>) -> (Arc<AppState>, EventSenders) {
        let txs = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut services = dar_extension_sdk::ServiceRegistry::default();
        services
            .register::<dyn ChatBackend>(
                "fake",
                Arc::new(ScriptedBackend {
                    identities: std::sync::Mutex::new(identities.into()),
                    txs: Arc::clone(&txs),
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
            meta: Mutex::new(HashMap::new()),
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
        });
        (state, txs)
    }

    fn write_fake_archive(root: &std::path::Path, id: &str) {
        let dir = root.join("data/chat/sessions");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(format!("2026-01-01T00-00-00Z_{id}.jsonl")),
            format!("{{\"type\":\"session\",\"id\":\"{id}\",\"backend\":\"fake\"}}\n{{\"type\":\"message_end\",\"message\":{{\"role\":\"user\",\"content\":\"hi\"}}}}\n"),
        )
        .unwrap();
    }

    async fn queried_sessions(state: Arc<AppState>, uri: &str) -> serde_json::Value {
        let response = router(state)
            .oneshot(
                axum::http::Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn palette_query_caps_results_after_title_then_newest_ordering() {
        let state = slot_state(test_root(), HashMap::new());
        for number in 0..60 {
            let id = format!("item-{number:02}");
            write_fake_archive(&state.root, &id);
            let path = state.root.join(format!(
                "data/chat/sessions/2026-01-01T00-00-00Z_{id}.jsonl"
            ));
            writeln!(
                OpenOptions::new().append(true).open(&path).unwrap(),
                "{}",
                serde_json::json!({"message":{"role":"assistant","content":"common search"}})
            )
            .unwrap();
            fs::File::open(&path)
                .unwrap()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(number + 1))
                .unwrap();
            if number < 2 {
                state.meta.lock().await.insert(
                    id,
                    SessionMeta {
                        title: Some("Common Search project".into()),
                        archived: Some(true),
                    },
                );
            }
        }
        let result = queried_sessions(Arc::clone(&state), "/sessions?q=SEARCH%20common").await;
        let rows = result.as_array().unwrap();
        assert_eq!(rows.len(), 50);
        assert_eq!(rows[0]["id"], "item-01");
        assert_eq!(rows[1]["id"], "item-00");
        assert_eq!(rows[0]["match"], "title");
        assert_eq!(rows[1]["match"], "title");
        assert_eq!(rows[0]["archived"], true);
        assert_eq!(rows[2]["id"], "item-59");
        assert_eq!(rows[3]["id"], "item-58");
        assert_eq!(rows[49]["id"], "item-12");
        assert_eq!(rows[2]["match"], "content");
        assert_eq!(rows[2]["snippet"], "common search");
        let plain = queried_sessions(state, "/sessions?q=%20%09").await;
        assert!(plain
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row.get("match").is_none()));
    }

    #[tokio::test]
    async fn palette_query_matches_words_across_messages_but_ignores_tools_and_thinking() {
        let state = slot_state(test_root(), HashMap::new());
        write_fake_archive(&state.root, "mixed");
        let path = state
            .root
            .join("data/chat/sessions/2026-01-01T00-00-00Z_mixed.jsonl");
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({"message":{"role":"user","content":"PgBouncer request"}})
        )
        .unwrap();
        writeln!(file, "not json").unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({"message":{"role":"assistant","content":[
                {"type":"thinking","text":"thinking-secret"},
                {"type":"toolCall","text":"tool-call-secret"},
                {"type":"text","text":"Transaction pooling answer"}
            ]}})
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({"message":{"role":"toolResult","content":"tool-output-secret"}})
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({"message":{"role":"system","content":"system-secret"}})
        )
        .unwrap();
        writeln!(file, "{{unfinished").unwrap();
        let result = queried_sessions(Arc::clone(&state), "/sessions?q=POOLING%20pgbouncer").await;
        assert_eq!(result.as_array().unwrap().len(), 1);
        assert_eq!(result[0]["id"], "mixed");
        assert_eq!(result[0]["match"], "content");
        assert_eq!(result[0]["snippet"], "PgBouncer request");
        for word in [
            "thinking-secret",
            "tool-call-secret",
            "tool-output-secret",
            "system-secret",
            "missing",
        ] {
            assert_eq!(
                queried_sessions(Arc::clone(&state), &format!("/sessions?q={word}")).await,
                serde_json::json!([])
            );
        }
    }

    #[tokio::test]
    async fn sidebar_query_finds_decoded_content_and_omits_nonmatches() {
        let state = slot_state(test_root(), HashMap::new());
        write_fake_archive(&state.root, "body-only");
        let path = state
            .root
            .join("data/chat/sessions/2026-01-01T00-00-00Z_body-only.jsonl");
        writeln!(OpenOptions::new().append(true).open(path).unwrap(),
            r#"{{"type":"message_end","message":{{"role":"assistant","content":"Try a+b & café"}}}}"#).unwrap();
        write_fake_archive(&state.root, "other");
        let result = queried_sessions(
            Arc::clone(&state),
            "/sessions?q=%20a%2Bb%20%26%20caf%C3%A9%20",
        )
        .await;
        assert_eq!(result.as_array().unwrap().len(), 1);
        assert_eq!(result[0]["id"], "body-only");
        assert_eq!(result[0]["label"], "hi");
        assert_eq!(result[0]["snippet"], "Try a+b & café");
        assert_eq!(
            queried_sessions(state, "/sessions?q=absent").await,
            serde_json::json!([])
        );
    }

    #[tokio::test]
    async fn sidebar_query_discovers_older_titles_and_content_with_metadata_and_live_slots() {
        let (state, _txs) = scripted_state(vec![Some("00-body".into())]);
        for n in 0..55 {
            write_fake_archive(&state.root, &format!("new-{n:02}"));
        }
        write_fake_archive(&state.root, "00-body");
        write_fake_archive(&state.root, "01-title");
        let dir = state.root.join("data/chat/sessions");
        let body_path = dir.join("2026-01-01T00-00-00Z_00-body.jsonl");
        writeln!(
            OpenOptions::new().append(true).open(&body_path).unwrap(),
            r#"{{"type":"message_end","message":{{"role":"assistant","content":"hidden orchid"}}}}"#
        )
        .unwrap();
        let app = router(Arc::clone(&state));
        for (id, body) in [
            ("00-body", r#"{"title":"Renamed body","archived":true}"#),
            ("01-title", r#"{"title":"Orchid planning"}"#),
        ] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .method("PATCH")
                        .uri(format!("/sessions/{id}"))
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert!(response.status().is_success());
        }
        let live = state.session("client-slot").await.unwrap();
        *live.inner.lock().await = Some(state.open_session(Arc::clone(&live)).await.unwrap());
        assert_eq!(wait_resolved(&live).await, "00-body");
        live.active_turns.store(1, Ordering::SeqCst);
        // Explicit mtimes make the ordering independent of filesystem timing.
        for (path, secs) in [
            (&body_path, 2),
            (&dir.join("2026-01-01T00-00-00Z_01-title.jsonl"), 1),
        ] {
            fs::File::open(path)
                .unwrap()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        }
        let baseline = queried_sessions(Arc::clone(&state), "/sessions").await;
        assert_eq!(baseline.as_array().unwrap().len(), 50);
        assert!(baseline
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["id"] != "00-body" && s["id"] != "01-title" && s.get("snippet").is_none()));
        for uri in ["/sessions?q=", "/sessions?q=%20%09%20"] {
            assert_eq!(queried_sessions(Arc::clone(&state), uri).await, baseline);
        }
        let result = queried_sessions(Arc::clone(&state), "/sessions?q=OrChId").await;
        assert_eq!(result.as_array().unwrap().len(), 2);
        assert_eq!(result[0]["id"], "01-title");
        assert_eq!(result[0]["title"], "Orchid planning");
        assert_eq!(result[0]["match"], "title");
        assert!(result[0].get("snippet").is_none());
        assert_eq!(result[0]["archived"], false);
        assert_eq!(result[1]["id"], "00-body");
        assert_eq!(result[1]["label"], "Renamed body");
        assert_eq!(result[1]["match"], "content");
        assert_eq!(result[1]["snippet"], "hidden orchid");
        assert_eq!(result[1]["archived"], true);
        assert_eq!(result[1]["connected"], true);
        assert_eq!(result[1]["busy"], true);
        assert_eq!(result[1]["slots"], serde_json::json!(["client-slot"]));
    }

    async fn wait_resolved(slot: &Session) -> String {
        for _ in 0..200 {
            if let Some(id) = slot.resolved_live_id.lock().unwrap().clone() {
                return id;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("slot never resolved an archive id");
    }

    #[tokio::test]
    async fn concurrent_slots_bind_backend_reported_identity() {
        let (state, _txs) = scripted_state(vec![Some("id-a".into()), Some("id-b".into())]);
        write_fake_archive(&state.root, "id-a");
        write_fake_archive(&state.root, "id-b");
        let a = state.session("a").await.unwrap();
        let b = state.session("b").await.unwrap();
        *a.inner.lock().await = Some(state.open_session(Arc::clone(&a)).await.unwrap());
        *b.inner.lock().await = Some(state.open_session(Arc::clone(&b)).await.unwrap());
        assert_eq!(wait_resolved(&a).await, "id-a");
        assert_eq!(wait_resolved(&b).await, "id-b");
        b.active_turns.store(1, Ordering::SeqCst);
        let Json(index) = session_index(State(Arc::clone(&state)), Query(SessionQuery::default()))
            .await
            .unwrap();
        let entry = |id: &str| index.iter().find(|e| e.id == id).unwrap();
        assert!(entry("id-a").connected && !entry("id-a").busy);
        assert!(entry("id-b").connected && entry("id-b").busy);
        assert_eq!(entry("id-a").slots, ["a"]);
        assert_eq!(entry("id-b").slots, ["b"]);
        assert_eq!(state.slot_for_live("id-b").await.unwrap().0, "b");
    }

    #[tokio::test]
    async fn index_before_delayed_identity_leaves_slot_unresolved_then_binds() {
        let (state, txs) = scripted_state(vec![None]);
        write_fake_archive(&state.root, "id-a");
        let a = state.session("a").await.unwrap();
        *a.inner.lock().await = Some(state.open_session(Arc::clone(&a)).await.unwrap());
        let Json(index) = session_index(State(Arc::clone(&state)), Query(SessionQuery::default()))
            .await
            .unwrap();
        assert!(a.resolved_live_id.lock().unwrap().is_none());
        assert!(index.iter().all(|e| !e.connected && e.slots.is_empty()));
        let tx = txs.lock().unwrap()[0].clone();
        tx.send(ChatEvent::SessionIdentity { id: "id-a".into() })
            .await
            .unwrap();
        assert_eq!(wait_resolved(&a).await, "id-a");
        let Json(index) = session_index(State(Arc::clone(&state)), Query(SessionQuery::default()))
            .await
            .unwrap();
        assert!(index.iter().find(|e| e.id == "id-a").unwrap().connected);
    }

    #[tokio::test]
    async fn index_reports_every_connected_slot_and_drops_connected_after_idle_close() {
        let closes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let state = idle_state(
            vec![
                ("fresh-1", idle_slot(&closes, false, Some("id-a"))),
                ("fresh-2", idle_slot(&closes, false, Some("id-b"))),
            ],
            dar_extension_sdk::ServiceRegistry::default(),
        );
        write_fake_archive(&state.root, "id-a");
        write_fake_archive(&state.root, "id-b");
        let Json(index) = session_index(State(Arc::new(state)), Query(SessionQuery::default()))
            .await
            .unwrap();
        let entry = |id: &str| index.iter().find(|e| e.id == id).unwrap();
        // Both connected, but each names its own slot so the client can focus one.
        assert!(entry("id-a").connected && entry("id-b").connected);
        assert_eq!(entry("id-a").slots, ["fresh-1"]);
        assert_eq!(entry("id-b").slots, ["fresh-2"]);
    }

    #[tokio::test]
    async fn index_marks_idle_closed_conversation_disconnected_but_keeps_slot() {
        let closes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let state = Arc::new(idle_state(
            vec![("s", idle_slot(&closes, false, Some("id-a")))],
            dar_extension_sdk::ServiceRegistry::default(),
        ));
        write_fake_archive(&state.root, "id-a");
        state
            .reap_idle(1_000 + 300_000, std::time::Duration::from_secs(300))
            .await;
        let Json(index) = session_index(State(Arc::clone(&state)), Query(SessionQuery::default()))
            .await
            .unwrap();
        let entry = index.iter().find(|e| e.id == "id-a").unwrap();
        assert!(!entry.connected, "closed backend is not connected");
        assert_eq!(entry.slots, ["s"], "slot stays focusable after idle close");
    }

    #[test]
    fn sidebar_highlights_focused_slot_only_and_labels_connected() {
        let js = include_str!("renderer.js");
        assert!(js.contains("(s.slots || []).includes(focusedSlot) ? ' is-focused'"));
        assert!(js.contains("s.connected ?"));
        assert!(js.contains("Connected · "));
        assert!(!js.contains("Open · ") && !js.contains("is_live"));
        let css = include_str!("chat.css");
        assert!(css.contains(".chat-session.is-focused") && !css.contains("is-live"));
    }

    #[tokio::test]
    async fn new_endpoint_allocates_fresh_slot_when_restored_history_is_populated() {
        let root = test_root();
        let before = slot_state(root.clone(), HashMap::new());
        before
            .session("main")
            .await
            .unwrap()
            .publish_user("hi".into(), vec![])
            .unwrap();
        // Restart: a new state reloads the persisted history into `main`.
        let state = slot_state(root, HashMap::new());
        assert!(!state
            .session("main")
            .await
            .unwrap()
            .history
            .lock()
            .unwrap()
            .is_empty());
        let slot = slot_of(
            router(Arc::clone(&state))
                .oneshot(post("/main/new", Body::empty()))
                .await
                .unwrap(),
        )
        .await;
        assert_ne!(slot, "main");
        assert!(slot.starts_with('w'));
    }

    #[tokio::test]
    async fn new_endpoint_reuses_unused_slot() {
        let state = slot_state(test_root(), HashMap::new());
        state.session("main").await.unwrap();
        let slot = slot_of(
            router(state)
                .oneshot(post("/main/new", Body::empty()))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(slot, "main");
    }

    #[tokio::test]
    async fn reset_session_clears_transcript_and_persists_reset() {
        let s = session(Box::new(FakeSession {
            aborted: Arc::new(AtomicBool::new(false)),
            sends: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            abort_fails: false,
        }));
        s.publish_user("hi".into(), vec![]).unwrap();
        let user_seq = load_transcript(&s.transcript).unwrap().back().unwrap().seq;
        let state = slot_state(
            test_root(),
            HashMap::from([("test".into(), Arc::clone(&s))]),
        );
        state.reset_session("test").await.unwrap();
        let events = load_transcript(&s.transcript).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "reset");
        assert!(events[0].seq > user_seq);
    }

    const ARCHIVE_ID: &str = "11111111-2222-3333-4444-555555555555";

    fn write_archive(root: &std::path::Path) {
        let dir = root.join("data/chat/sessions");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(format!("2026-01-01T00-00-00Z_{ARCHIVE_ID}.jsonl")),
            format!("{{\"type\":\"session\",\"id\":\"{ARCHIVE_ID}\"}}\n{{\"type\":\"message_end\",\"message\":{{\"role\":\"user\",\"content\":\"remember this\"}}}}\n"),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn resume_replays_context_usage_from_pi_archive() {
        let root = test_root();
        let dir = root.join("data/chat/sessions");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(format!("2026-01-01T00-00-00Z_{ARCHIVE_ID}.jsonl")),
            format!("{{\"type\":\"session\",\"id\":\"{ARCHIVE_ID}\"}}\n{{\"type\":\"message\",\"message\":{{\"role\":\"assistant\",\"model\":\"claude-opus-5-5\",\"usage\":{{\"input\":4,\"output\":465,\"cacheRead\":0,\"cacheWrite\":28651}},\"content\":[{{\"type\":\"text\",\"text\":\"hi\"}}]}}}}\n"),
        )
        .unwrap();
        let state = slot_state(root, HashMap::new());
        let app = router(Arc::clone(&state));
        let body = Body::from(format!("{{\"id\":\"{ARCHIVE_ID}\"}}"));
        let slot = slot_of(app.oneshot(post("/main/resume", body)).await.unwrap()).await;
        assert_eq!(slot, ARCHIVE_ID);
        let opened = state.session(ARCHIVE_ID).await.unwrap();
        let history = opened.history.lock().unwrap();
        let usage = history.iter().find(|e| e.kind == "context_usage").unwrap();
        assert_eq!(usage.tokens_used, Some(29120));
        assert_eq!(usage.context_window, Some(200_000));
        assert!(usage.historical);
    }

    #[tokio::test]
    async fn resume_does_not_touch_busy_slot_and_is_idempotent() {
        let root = test_root();
        write_archive(&root);
        let closed = Arc::new(AtomicBool::new(false));
        let busy = session(Box::new(CloseFlagSession(Arc::clone(&closed))));
        busy.active_turns.store(1, Ordering::SeqCst);
        let generation = busy.generation.load(Ordering::SeqCst);
        let state = slot_state(root, HashMap::from([("main".into(), Arc::clone(&busy))]));
        let app = router(Arc::clone(&state));
        let resume = || {
            post(
                "/main/resume",
                Body::from(format!("{{\"id\":\"{ARCHIVE_ID}\"}}")),
            )
        };
        let slot = slot_of(app.clone().oneshot(resume()).await.unwrap()).await;
        assert_eq!(slot, ARCHIVE_ID);
        assert!(!closed.load(Ordering::SeqCst));
        assert!(busy.inner.lock().await.is_some());
        assert_eq!(busy.active_turns.load(Ordering::SeqCst), 1);
        assert_eq!(busy.generation.load(Ordering::SeqCst), generation);
        assert!(busy.history.lock().unwrap().is_empty());

        let opened = state.session(ARCHIVE_ID).await.unwrap();
        let replayed = opened.history.lock().unwrap().len();
        assert!(replayed > 0);
        assert!(opened.history.lock().unwrap().iter().all(|e| e.historical));

        // Second resume: slot already resolved to the id, no replay.
        let again = slot_of(app.oneshot(resume()).await.unwrap()).await;
        assert_eq!(again, ARCHIVE_ID);
        assert_eq!(opened.history.lock().unwrap().len(), replayed);
    }

    #[tokio::test]
    async fn resume_returns_existing_slot_that_resolved_to_id() {
        let root = test_root();
        write_archive(&root);
        let live = session(Box::new(RejectingSession));
        *live.resolved_live_id.lock().unwrap() = Some(ARCHIVE_ID.into());
        let state = slot_state(root, HashMap::from([("w1".into(), Arc::clone(&live))]));
        let slot = slot_of(
            router(Arc::clone(&state))
                .oneshot(post(
                    "/main/resume",
                    Body::from(format!("{{\"id\":\"{ARCHIVE_ID}\"}}")),
                ))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(slot, "w1");
        assert!(live.history.lock().unwrap().is_empty());
        assert_eq!(state.sessions.lock().await.len(), 1);
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
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
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
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
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
    fn notification_sound_uses_contained_file_or_default() {
        let root = test_root();
        fs::create_dir_all(root.join("sounds")).unwrap();
        fs::write(root.join("sounds/ping.mp3"), b"mp3").unwrap();
        let sound_for = |value: &str| {
            fs::write(
                root.join("agent.yaml"),
                format!("notification_sound: \"{value}\"\n"),
            )
            .unwrap();
            agent_notification_sound(&root)
        };
        assert!(sound_for("sounds/ping.mp3").is_some_and(|f| f.ends_with("sounds/ping.mp3")));
        assert_eq!(sound_for("missing.mp3"), None);
        assert_eq!(sound_for("../outside.mp3"), None);
        fs::write(root.join("agent.yaml"), "id: x\n").unwrap();
        assert_eq!(agent_notification_sound(&root), None);
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
            root: std::path::PathBuf::new(),
        };
        let html = tab(Avatar::Text("🦉".into())).render().unwrap();
        assert!(html.contains(r#"<span class="chat-avatar" aria-hidden="true">🦉</span>"#));
        assert!(html.contains(r#"<span class="chat-hero-avatar" aria-hidden="true">🦉</span>"#));
        let html = tab(Avatar::File(root.join("assets/me.png")))
            .render()
            .unwrap();
        assert!(html.contains(r#"<img data-avatar-src="/chat/avatar" alt="Owl">"#));
        assert!(html.contains(r#"data-icon-src="/chat/avatar""#));
        let html = tab(Avatar::Text("🦉".into())).render().unwrap();
        assert!(html.contains(r#"data-icon-text="🦉""#));
        let html = tab(Avatar::Url("https://x.test/a.png".into()))
            .render()
            .unwrap();
        assert!(html.contains(r#"data-icon-src="https://x.test/a.png""#));
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
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
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
            transition: Mutex::new(()),
            slot_registry: Arc::default(),
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
            *web.resolved_live_id.lock().unwrap() = Some("live".into());
            let state = AppState {
                config: Config {
                    backend: Some("fake".into()),
                    ..Config::default()
                },
                root: root.clone(),
                start: std::sync::OnceLock::from(start_ctx(services)),
                sessions: Mutex::new(HashMap::new()),
                meta: Mutex::new(HashMap::new()),
                transition: Mutex::new(()),
                slot_registry: Arc::default(),
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
    fn browser_avatar_icon_behaviors() {
        let root = env!("CARGO_MANIFEST_DIR");
        let status = std::process::Command::new("node")
            .args([
                format!("{root}/tests/avatar-icons.js"),
                format!("{root}/src/renderer.js"),
            ])
            .status()
            .expect("node is available for browser renderer tests");
        assert!(status.success());
    }

    #[test]
    fn browser_unread_and_notify_behaviors() {
        let root = env!("CARGO_MANIFEST_DIR");
        let status = std::process::Command::new("node")
            .args([
                format!("{root}/tests/unread-notify.js"),
                format!("{root}/src/renderer.js"),
            ])
            .status()
            .expect("node is available for browser renderer tests");
        assert!(status.success());
    }

    #[test]
    fn browser_slash_menu_behaviors() {
        let root = env!("CARGO_MANIFEST_DIR");
        let status = std::process::Command::new("node")
            .args([
                format!("{root}/tests/slash-menu.js"),
                format!("{root}/src/renderer.js"),
            ])
            .status()
            .expect("node is available for browser renderer tests");
        assert!(status.success());
    }

    #[test]
    fn browser_sidebar_search_behaviors() {
        let root = env!("CARGO_MANIFEST_DIR");
        let status = std::process::Command::new("node")
            .args([
                format!("{root}/tests/sidebar-search.js"),
                format!("{root}/src/renderer.js"),
            ])
            .status()
            .expect("node is available for browser renderer tests");
        assert!(status.success());
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
        let script = r#"const r=require(process.argv[1]);let b=[];for(const e of [{type:'thinking',text:'plan '},{type:'thinking',text:'it'},{type:'delta',text:'* **answer**\n```txt\n**code**\n```'},{type:'tool_call',id:'x',name:'shell',args:'{}'},{type:'tool_output',id:'x',text:'partial'},{type:'tool_output',id:'x',text:'failed',is_error:true,done:true},{type:'error',error:'warning'},{type:'aborted',error:'aborted'},{type:'user',text:'run `x --y` now'}])b=r.reduce(b,e);let h=r.html(b);if(b.length!==6||b[0].text!=='plan it'||(h.match(/data-tool-id=/g)||[]).length!==1||!h.includes('failed')||!h.includes('is-error is-done')||!h.includes('<ul><li><strong>answer</strong></li></ul>')||!h.includes('<pre><code data-language="txt">**code**\n</code></pre>')||!h.includes('warning')||!h.includes('Interrupted')||!h.includes('<code>x --y</code>')||r.usageText({tokens_used:12,context_window:100})!=='12 / 100 tokens · 12%'||r.usageText({tokens_used:12})!=='12 tokens')process.exit(1);"#;
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
                    app: None,
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
