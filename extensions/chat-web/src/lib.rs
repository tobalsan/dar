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
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        Html, IntoResponse,
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

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    backend: Option<String>,
    command: Option<String>,
    // Retained for backwards-compatible config parsing; sessions are shared.
    sessions_dir: Option<String>,
    idle_minutes: Option<u64>,
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
            DashboardTabs::shared(&mut ctx.services)?.add(Arc::new(ChatTab))?;
            ctx.http.mount(host_api::HttpMount {
                namespace: "/chat".into(),
                router: router(state),
                routes: vec![
                    "/".into(),
                    "/{session}/stream".into(),
                    "/{session}/history".into(),
                    "/{session}/send".into(),
                    "/{session}/abort".into(),
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

struct ChatTab;
impl DashboardTab for ChatTab {
    fn id(&self) -> &str {
        TAB_ID
    }
    fn title(&self) -> &str {
        "Chat"
    }
    fn render(&self) -> Result<String> {
        Ok(format!(
            r#"<section class="chat-web"><div id="chat-transcript"></div><form id="chat-composer"><input id="chat-input" autocomplete="off" placeholder="Message"><button>Send</button><button type="button" id="chat-abort">Abort</button></form><script>{}</script></section>"#,
            include_str!("renderer.js")
        ))
    }
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
    tx: broadcast::Sender<WireEvent>,
    events: broadcast::Sender<ChatEvent>,
    generation: std::sync::atomic::AtomicU64,
    next_seq: std::sync::atomic::AtomicU64,
    active_turns: std::sync::atomic::AtomicUsize,
    abort_requested: std::sync::atomic::AtomicBool,
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
        .route("/{session}/history", get(history))
        .route("/{session}/send", post(send))
        .route("/{session}/abort", post(abort))
        .with_state(state)
}
async fn index() -> Html<&'static str> {
    Html("chat web is available from the Chat dashboard tab")
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
            tx,
            events,
            generation: std::sync::atomic::AtomicU64::new(0),
            next_seq: std::sync::atomic::AtomicU64::new(next_seq),
            active_turns: std::sync::atomic::AtomicUsize::new(0),
            abort_requested: std::sync::atomic::AtomicBool::new(false),
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
                let _ = sink.events.send(event.clone());
                sink.publish_if_current(generation, event);
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
            session.publish_user(display.clone());
            let _ = session.events.send(ChatEvent::User { text: display });
            session
                .active_turns
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            inner
                .as_mut()
                .expect("session opened above")
                .send_turn(prompt)
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
        Box::pin(async move {
            let session = self.session("main").await?;
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
                };
                let _ = session.tx.send(event);
            }
            let _ = session.events.send(ChatEvent::SessionReset);
            if let Some(backend) = backend {
                tokio::spawn(async move {
                    let _ = backend.close().await;
                });
            }
            Ok(())
        })
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
    fn publish_user(&self, text: String) {
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
        };
        append_transcript(&self.transcript, &event).expect("chat-web transcript append failed");
        self.history
            .lock()
            .expect("chat-web history mutex poisoned")
            .push_back(event.clone());
        let _ = self.tx.send(event);
    }
    fn publish_if_current(&self, generation: u64, event: ChatEvent) {
        if self.generation.load(std::sync::atomic::Ordering::SeqCst) == generation {
            self.publish(event);
        }
    }

    fn publish(&self, event: ChatEvent) {
        let (kind, text, error, id, name, args, is_error, done) = match event {
            ChatEvent::User { .. } | ChatEvent::SessionReset => return,
            ChatEvent::Delta {
                role: ChatRole::Assistant,
                text,
            } => ("delta", Some(text), None, None, None, None, None, None),
            ChatEvent::Delta {
                role: ChatRole::Thinking,
                text,
            } => ("thinking", Some(text), None, None, None, None, None, None),
            ChatEvent::ToolCall { id, name, args } => (
                "tool_call",
                None,
                None,
                Some(id),
                Some(name),
                Some(args),
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
            ),
            ChatEvent::Error(error) => ("error", None, Some(error), None, None, None, None, None),
            ChatEvent::TurnFinished { ok, error } => (
                if ok { "finished" } else { "aborted" },
                None,
                error,
                None,
                None,
                None,
                None,
                None,
            ),
            ChatEvent::SessionClosed { error } => {
                ("closed", None, error, None, None, None, None, None)
            }
            _ => return,
        };
        if matches!(kind, "finished" | "aborted" | "closed") {
            let Ok(turns) = self.active_turns.fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |turns| turns.checked_sub(1),
            ) else {
                return;
            };
            if turns == 1 {
                self.abort_requested
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                let _ = self.abort_signal.send(false);
            }
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
        };
        let mut history = self
            .history
            .lock()
            .expect("chat-web history mutex poisoned");
        append_transcript(&self.transcript, &event).expect("chat-web transcript append failed");
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
    if body.command_id.trim().is_empty() || body.message.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"accepted":false})),
        )
            .into_response();
    }
    match state.session(&id).await {
        Ok(s) => {
            if !s.command_ids.lock().await.insert(body.command_id.clone()) {
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
                        s.command_ids.lock().await.remove(&body.command_id);
                        return (
                            StatusCode::SERVICE_UNAVAILABLE,
                            Json(serde_json::json!({"accepted":false,"error":e.to_string()})),
                        )
                            .into_response();
                    }
                }
            }
            s.publish_user(body.message.clone());
            let _ = s.events.send(ChatEvent::User {
                text: body.message.clone(),
            });
            s.active_turns
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut aborted = s.abort_signal.subscribe();
            let message = body.message.clone();
            let accepted = tokio::select! {
                result = guard.as_mut().expect("session open").send_turn(message) => result,
                _ = aborted.changed() => Err(anyhow::anyhow!("turn aborted before acceptance")),
            };
            match accepted {
                Ok(()) => (
                    StatusCode::ACCEPTED,
                    Json(serde_json::json!({"accepted":true,"command_id":body.command_id})),
                )
                    .into_response(),
                Err(e) => {
                    if !s.abort_requested.load(std::sync::atomic::Ordering::SeqCst) {
                        s.active_turns
                            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                        s.command_ids.lock().await.remove(&body.command_id);
                    }
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
            tx,
            events,
            generation: std::sync::atomic::AtomicU64::new(0),
            next_seq: std::sync::atomic::AtomicU64::new(0),
            active_turns: std::sync::atomic::AtomicUsize::new(0),
            abort_requested: std::sync::atomic::AtomicBool::new(false),
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
                event.push_str(&String::from_utf8(frame.to_vec()).unwrap());
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
            replay.push_str(&String::from_utf8(frame.to_vec()).unwrap());
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
            reused.push_str(&String::from_utf8(frame.to_vec()).unwrap());
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
        session.publish_user("first".into());
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
                &String::from_utf8(
                    body.frame()
                        .await
                        .unwrap()
                        .unwrap()
                        .into_data()
                        .unwrap()
                        .to_vec(),
                )
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
            .publish_user("saved <message>".into());
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
        let html = ChatTab.render().unwrap();
        assert!(html.contains("id=\"chat-composer\""));
        assert!(!html.contains("\\\"chat-composer\\\""));
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

    #[test]
    fn browser_renderer_handles_the_representative_event_sequence() {
        let renderer = format!("{}/src/renderer.js", env!("CARGO_MANIFEST_DIR"));
        let script = r#"const r=require(process.argv[1]);let b=[];for(const e of [{type:'thinking',text:'plan '},{type:'thinking',text:'it'},{type:'delta',text:'* **answer**\n```txt\n**code**\n```'},{type:'tool_call',id:'x',name:'shell',args:'{}'},{type:'tool_output',id:'x',text:'partial'},{type:'tool_output',id:'x',text:'failed',is_error:true,done:true},{type:'error',error:'warning'},{type:'aborted',error:'aborted'}])b=r.reduce(b,e);let h=r.html(b);if(b.length!==5||b[0].text!=='plan it'||(h.match(/data-tool-id=/g)||[]).length!==1||!h.includes('failed')||!h.includes('is-error is-done')||!h.includes('<ul><li><strong>answer</strong></li></ul>')||!h.includes('<pre><code data-language="txt">**code**\n</code></pre>')||!h.includes('warning')||!h.includes('turn aborted'))process.exit(1);"#;
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
