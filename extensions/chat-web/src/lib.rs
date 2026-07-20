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
    extract::{DefaultBodyLimit, Multipart, Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware,
    response::{
        sse::{Event, KeepAlive, Sse},
        Html, IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use cap_dashboard_tab::{DashboardTab, DashboardTabs};
use dar_extension_sdk::{
    chat::{self, ChatBackend, ChatEvent, ChatRole, ChatSession},
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
            });
            self.state
                .set(Arc::clone(&state))
                .map_err(|_| anyhow::anyhow!("chat-web registered twice"))?;
            migrate_tui_sessions(ctx.paths.root())?;
            state.session("main").await?;
            let coordinator: Arc<dyn chat::ChatCoordinator> = state.clone();
            ctx.services.service::<dyn chat::ChatCoordinator>(
                chat::CHAT_COORDINATOR_SERVICE,
                coordinator,
            )?;
            DashboardTabs::shared(&mut ctx.services)?.add(Arc::new(ChatTab {
                agent_name: agent_display_name(ctx.paths.root()),
            }))?;
            ctx.http.mount(host_api::HttpMount {
                namespace: "/chat".into(),
                router: router(state),
                routes: vec![
                    "/".into(),
                    "/{session}/stream".into(),
                    "/{session}/history".into(),
                    "/{session}/send".into(),
                    "/{session}/upload".into(),
                    "/{session}/attachment/{command}/{name}".into(),
                    "/{session}/abort".into(),
                    "/{session}/compact".into(),
                    "/{session}/new".into(),
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
}

struct ChatTab {
    agent_name: String,
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
            r#"<style>{}</style><section class="chat-web" id="chat-root" data-agent-name="{}"><div class="chat-transcript" id="chat-transcript" role="log" aria-live="polite" aria-label="Conversation"></div><form class="chat-dock" id="chat-composer" autocomplete="off" onsubmit="event.preventDefault()"><div class="chat-chips" id="chat-chips" aria-label="Pending attachments"></div><div class="chat-row"><button type="button" id="chat-attach" class="chat-icon" aria-label="Attach files" title="Attach files"><svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M21.44 11.05l-9.19 9.19a6 6 0 0 1-8.49-8.49l9.19-9.19a4 4 0 0 1 5.66 5.66l-9.2 9.19a2 2 0 0 1-2.83-2.83l8.49-8.48"/></svg></button><input type="file" id="chat-attachments" multiple hidden aria-hidden="true" tabindex="-1"><textarea class="chat-input" id="chat-input" rows="1" placeholder="Message the agent" aria-label="Message" enterkeyhint="send"></textarea><button type="button" id="chat-abort" class="chat-icon chat-icon-stop" aria-label="Stop the running turn" title="Stop" hidden><svg width="16" height="16" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><rect x="6" y="6" width="12" height="12" rx="2"/></svg></button><button type="submit" id="chat-send" class="chat-icon chat-icon-send" aria-label="Send message" title="Send" disabled><svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M12 19V5"/><path d="M5 12l7-7 7 7"/></svg></button></div><div class="chat-bar"><span class="chat-spacer"></span><span class="chat-meter" id="chat-token-meter" aria-live="polite"></span></div></form></section><script>{}</script>"#,
            include_str!("chat.css"),
            escape_html_attr(&self.agent_name),
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
#[derive(Clone, Serialize, Deserialize)]
struct WireEvent {
    seq: u64,
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
        .route("/{session}/stream", get(stream))
        .route("/{session}/send", post(send))
        .route("/{session}/upload", post(upload))
        .route("/{session}/attachment/{command}/{name}", get(attachment))
        .route("/{session}/abort", post(abort))
        .route("/{session}/compact", post(compact))
        .route("/{session}/new", post(new_session_route))
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
        let resume_session_id = (!session
            .suppress_resume
            .swap(false, std::sync::atomic::Ordering::SeqCst))
        .then(|| newest_session(&session_dir))
        .flatten()
        .filter(|session| !self.session_is_idle(session))
        .map(|session| session.id);
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
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                let _acceptance = sink.acceptance_lock.lock().await;
                if sink.publish_if_current(generation, event.clone()) {
                    let _ = sink.events.send(event);
                }
            }
        });
        backend.open(params, event_tx).await
    }

    fn session_is_idle(&self, session: &PersistedSession) -> bool {
        let idle_minutes = self.config.idle_minutes.unwrap_or(360);
        session.modified.elapsed().is_ok_and(|idle| {
            idle > std::time::Duration::from_secs(idle_minutes.saturating_mul(60))
        })
    }

    async fn reset_session(&self, id: &str) -> Result<()> {
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
        if let Some(backend) = backend {
            tokio::spawn(async move {
                let _ = backend.close().await;
            });
        }
        Ok(())
    }
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

fn newest_session(dir: &std::path::Path) -> Option<PersistedSession> {
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
                let id = header.get("id")?.as_str()?.to_owned();
                Some(PersistedSession {
                    id,
                    modified: entry.metadata().ok()?.modified().ok()?,
                })
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
        let (kind, text, error, id, name, args, is_error, done, tokens_used, context_window) =
            match event {
                ChatEvent::User { .. } | ChatEvent::SessionReset => return true,
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
                ),
                ChatEvent::SessionClosed { error } => (
                    "closed", None, error, None, None, None, None, None, None, None,
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
            let stream = replay.chain(live).map(sse_event);
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
            let accepted = tokio::select! {
                result = s.accept_turn(
                    guard.as_mut().expect("session open").as_mut(),
                    prompt,
                    display,
                    attachments,
                ) => result,
                _ = aborted.changed() => Err(anyhow::anyhow!("turn aborted before acceptance")),
            };
            match accepted {
                Ok(()) => (
                    StatusCode::ACCEPTED,
                    Json(serde_json::json!({"accepted":true,"command_id":command_id})),
                )
                    .into_response(),
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

    struct FakeBackend {
        opens: Arc<std::sync::atomic::AtomicUsize>,
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
        let (_, shutdown) = watch::channel(false);
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
        let mut persisted = newest_session(&sessions).unwrap();
        assert_eq!(persisted.id, "resume-me");
        persisted.modified = std::time::SystemTime::UNIX_EPOCH;
        let state = AppState {
            config: Config {
                idle_minutes: Some(0),
                ..Config::default()
            },
            root,
            start: std::sync::OnceLock::new(),
            sessions: Mutex::new(HashMap::new()),
        };
        assert!(state.session_is_idle(&persisted));
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
    async fn rejected_http_send_has_no_user_event_or_active_turn() {
        let s = session(Box::new(RejectingSession));
        let state = Arc::new(AppState {
            config: Config::default(),
            root: test_root(),
            start: std::sync::OnceLock::new(),
            sessions: Mutex::new(HashMap::from([("test".into(), Arc::clone(&s))])),
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
        };
        let html = tab.render().unwrap();
        assert!(html.contains("id=\"chat-composer\""));
        // Belt-and-braces: even if the JS singleton fails to attach, the inline
        // handler blocks a native submit / full-page reload.
        assert!(html.contains("onsubmit=\"event.preventDefault()\""));
        assert!(html.contains("data-agent-name="));
        assert!(html.contains("id=\"chat-input\"") && html.contains("<textarea"));
        assert!(html.contains("id=\"chat-attachments\"") && html.contains("hidden"));
        assert!(html.contains("id=\"chat-attach\""));
        assert!(html.contains("id=\"chat-send\""));
        // Guards against the renderer losing the fleet-proxy prefix read.
        assert!(html.contains("window.__dashPrefix"));
        let abort_pos = html.find("id=\"chat-abort\"").unwrap();
        let abort_tag_end = abort_pos + html[abort_pos..].find('>').unwrap();
        assert!(html[abort_pos..abort_tag_end].contains("hidden"));
        assert!(!html.contains("chat-compact"));
        assert!(!html.contains(">Compact<"));
        assert!(html.contains("@media (max-width: 520px)"));
        assert!(html.contains("overflow-wrap: anywhere"));
        assert!(!html.contains("\\\"chat-composer\\\""));
        assert!(html.contains("Context cleared, started a new session."));
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
        let script = r#"const handlers={},style={setProperty(){}};const element=(id)=>({id,style:{...style},dataset:{},value:'',disabled:false,hidden:false,innerHTML:'',scrollHeight:10,scrollTop:0,clientHeight:10,getBoundingClientRect:()=>({top:0})});const elements=Object.fromEntries(['chat-root','chat-transcript','chat-input','chat-chips','chat-send','chat-abort','chat-token-meter'].map(id=>[id,element(id)]));elements['chat-root'].dataset.agentName='Agent';global.document={getElementById:id=>elements[id]||null,addEventListener:(type,fn)=>handlers[type]=fn};global.window={innerHeight:1000,addEventListener(){}};global.EventSource=function(){};global.crypto={randomUUID:()=> 'command-id'};global.fetch=async()=>({ok:false,status:409,text:async()=>JSON.stringify({error:'backend rejected turn'})});require(process.argv[1]);const app=window.__chatWeb,file={name:'notes.txt'};app.pending=[file];elements['chat-input'].value='keep this';handlers.input({target:elements['chat-input']});handlers.submit({target:{id:'chat-composer'},preventDefault(){}});setTimeout(()=>{if(app.draft!=='keep this'||elements['chat-input'].value!=='keep this'||app.pending[0]!==file||app.turns!==0||elements['chat-send'].disabled||!elements['chat-transcript'].innerHTML.includes('backend rejected turn'))process.exit(1)},0);"#;
        let status = std::process::Command::new("node")
            .args(["-e", script, &renderer])
            .status()
            .expect("node is available for browser renderer tests");
        assert!(status.success());
    }

    #[test]
    fn browser_renderer_handles_the_representative_event_sequence() {
        let renderer = format!("{}/src/renderer.js", env!("CARGO_MANIFEST_DIR"));
        let script = r#"const r=require(process.argv[1]);let b=[];for(const e of [{type:'thinking',text:'plan '},{type:'thinking',text:'it'},{type:'delta',text:'* **answer**\n```txt\n**code**\n```'},{type:'tool_call',id:'x',name:'shell',args:'{}'},{type:'tool_output',id:'x',text:'partial'},{type:'tool_output',id:'x',text:'failed',is_error:true,done:true},{type:'error',error:'warning'},{type:'aborted',error:'aborted'},{type:'user',text:'run `x --y` now'}])b=r.reduce(b,e);let h=r.html(b);if(b.length!==6||b[0].text!=='plan it'||(h.match(/data-tool-id=/g)||[]).length!==1||!h.includes('failed')||!h.includes('is-error is-done')||!h.includes('<ul><li><strong>answer</strong></li></ul>')||!h.includes('<pre><code data-language="txt">**code**\n</code></pre>')||!h.includes('warning')||!h.includes('turn aborted')||!h.includes('<code>x --y</code>')||r.usageText({tokens_used:12,context_window:100})!=='12 / 100 tokens'||r.usageText({tokens_used:12})!=='12 tokens')process.exit(1);"#;
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
