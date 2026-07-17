//! Browser chat surface. It is deliberately an opt-in extension: without an
//! `extensions.chat-web` section it mounts neither routes nor dashboard tab.

use std::{
    collections::{HashMap, HashSet, VecDeque},
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
use tokio_stream::wrappers::BroadcastStream;

const TAB_ID: &str = "chat";

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    backend: Option<String>,
    command: Option<String>,
    sessions_dir: Option<String>,
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
            DashboardTabs::shared(&mut ctx.services)?.add(Arc::new(ChatTab))?;
            ctx.http.mount(host_api::HttpMount {
                namespace: "/chat".into(),
                router: router(state),
                routes: vec![
                    "/".into(),
                    "/{session}/stream".into(),
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
        Ok(r#"<section class="chat-web"><div id="chat-transcript"></div><form id="chat-composer"><input id="chat-input" autocomplete="off" placeholder="Message"><button>Send</button><button type="button" id="chat-abort">Abort</button></form><script>(function(){const id=sessionStorage.chatSession||(sessionStorage.chatSession=crypto.randomUUID());let es=new EventSource('/chat/'+id+'/stream');es.onmessage=e=>{let x=JSON.parse(e.data),d=document.getElementById('chat-transcript');d.insertAdjacentHTML('beforeend','<div>'+((x.text||x.error||x.type).replaceAll('&','&amp;').replaceAll('<','&lt;'))+'</div>')};document.getElementById('chat-composer').onsubmit=async e=>{e.preventDefault();let i=document.getElementById('chat-input');if(!i.value)return;await fetch('/chat/'+id+'/send',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({command_id:crypto.randomUUID(),message:i.value})});i.value=''};document.getElementById('chat-abort').onclick=()=>fetch('/chat/'+id+'/abort',{method:'POST'});})();</script></section>"#.into())
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
    generation: std::sync::atomic::AtomicU64,
    next_seq: std::sync::atomic::AtomicU64,
    active_turns: std::sync::atomic::AtomicUsize,
    abort_requested: std::sync::atomic::AtomicBool,
    abort_signal: watch::Sender<bool>,
    publish_lock: std::sync::Mutex<()>,
    command_ids: Mutex<HashSet<String>>,
    history: std::sync::Mutex<VecDeque<WireEvent>>,
    #[cfg(test)]
    pause_after_send: std::sync::Mutex<Option<Arc<PublishPause>>>,
    #[cfg(test)]
    pause_after_subscribe: std::sync::Mutex<Option<Arc<StreamPause>>>,
}
#[derive(Clone, Serialize)]
struct WireEvent {
    seq: u64,
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
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
        .route("/{session}/send", post(send))
        .route("/{session}/abort", post(abort))
        .with_state(state)
}
async fn index() -> Html<&'static str> {
    Html("chat web is available from the Chat dashboard tab")
}

impl AppState {
    async fn session(&self, id: &str) -> Result<Arc<Session>> {
        let mut sessions = self.sessions.lock().await;
        if let Some(s) = sessions.get(id).cloned() {
            return Ok(s);
        }
        let (tx, _) = broadcast::channel(256);
        let (abort_signal, _) = watch::channel(false);
        let session = Arc::new(Session {
            inner: Mutex::new(None),
            tx,
            generation: std::sync::atomic::AtomicU64::new(0),
            next_seq: std::sync::atomic::AtomicU64::new(0),
            active_turns: std::sync::atomic::AtomicUsize::new(0),
            abort_requested: std::sync::atomic::AtomicBool::new(false),
            abort_signal,
            publish_lock: std::sync::Mutex::new(()),
            command_ids: Mutex::new(HashSet::new()),
            history: std::sync::Mutex::new(VecDeque::new()),
            #[cfg(test)]
            pause_after_send: std::sync::Mutex::new(None),
            #[cfg(test)]
            pause_after_subscribe: std::sync::Mutex::new(None),
        });
        sessions.insert(id.to_owned(), Arc::clone(&session));
        Ok(session)
    }

    async fn open_session(&self, session: Arc<Session>) -> Result<Box<dyn ChatSession>> {
        let start = self.start.get().context("chat-web has not started")?;
        let backend_id = chat::resolve_agent_backend(start, self.config.backend.as_deref());
        let backend = start
            .host
            .services
            .get_named::<dyn ChatBackend>(&backend_id)
            .with_context(|| format!("chat backend {backend_id:?} is not registered"))?;
        let session_dir = self
            .config
            .sessions_dir
            .as_deref()
            .map(std::path::PathBuf::from)
            .map(|p| {
                if p.is_absolute() {
                    p
                } else {
                    self.root.join(p)
                }
            })
            .unwrap_or_else(|| self.root.join("data/chat-web/sessions"));
        std::fs::create_dir_all(&session_dir)?;
        let params = chat::agent_session_params(start, &session_dir)
            .command(self.config.command.as_deref().unwrap_or(""))
            .build();
        let generation = session
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let (event_tx, mut event_rx) = mpsc::channel(128);
        let sink = Arc::clone(&session);
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                sink.publish_if_current(generation, event);
            }
        });
        backend.open(params, event_tx).await
    }
}
impl Session {
    fn publish_if_current(&self, generation: u64, event: ChatEvent) {
        if self.generation.load(std::sync::atomic::Ordering::SeqCst) == generation {
            self.publish(event);
        }
    }

    fn publish(&self, event: ChatEvent) {
        let (kind, text, error) = match event {
            ChatEvent::Delta {
                role: ChatRole::Assistant,
                text,
            } => ("delta", Some(text), None),
            ChatEvent::Delta {
                role: ChatRole::Thinking,
                text,
            } => ("thinking", Some(text), None),
            ChatEvent::Error(error) => ("error", None, Some(error)),
            ChatEvent::TurnFinished { ok, error } => {
                (if ok { "finished" } else { "aborted" }, None, error)
            }
            ChatEvent::SessionClosed { error } => ("closed", None, error),
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
            kind,
            text,
            error,
        };
        let mut history = self
            .history
            .lock()
            .expect("chat-web history mutex poisoned");
        if history.len() == 256 {
            history.pop_front();
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
                let replay: Vec<_> = s
                    .history
                    .lock()
                    .expect("chat-web history mutex poisoned")
                    .iter()
                    .filter(|event| event.seq > last)
                    .cloned()
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
            let live = BroadcastStream::new(live).filter_map(move |item| async move {
                item.ok()
                    .filter(|event| event.seq > cutoff)
                    .map(Ok::<_, std::convert::Infallible>)
            });
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
            s.active_turns
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut aborted = s.abort_signal.subscribe();
            let accepted = tokio::select! {
                result = guard.as_mut().expect("session open").send_turn(body.message) => result,
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
        Arc::new(Session {
            inner: Mutex::new(Some(inner)),
            tx,
            generation: std::sync::atomic::AtomicU64::new(0),
            next_seq: std::sync::atomic::AtomicU64::new(0),
            active_turns: std::sync::atomic::AtomicUsize::new(0),
            abort_requested: std::sync::atomic::AtomicBool::new(false),
            abort_signal: watch::channel(false).0,
            publish_lock: std::sync::Mutex::new(()),
            command_ids: Mutex::new(HashSet::new()),
            history: std::sync::Mutex::new(VecDeque::new()),
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
            root: std::env::temp_dir(),
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
            root: std::env::temp_dir(),
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
            root: std::env::temp_dir(),
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
            root: std::env::temp_dir(),
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
            root: std::env::temp_dir(),
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
    async fn http_stream_fans_out_sequences_and_reuses_after_abort() {
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
            root: std::env::temp_dir(),
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
            app.clone().oneshot(send_request("one")).await.unwrap().status(),
            StatusCode::ACCEPTED
        );
        assert_eq!(
            app.clone().oneshot(send_request("one")).await.unwrap().status(),
            StatusCode::CONFLICT
        );

        let mut body_a = stream_a.into_body();
        let mut body_b = stream_b.into_body();
        for body in [&mut body_a, &mut body_b] {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(1), body.frame())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .into_data()
                .unwrap();
            let event = String::from_utf8(frame.to_vec()).unwrap();
            assert!(event.contains("id: 1"));
            assert!(event.contains("reply"));
        }

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
        assert!(terminal.contains("id: 2"));
        assert!(terminal.contains("aborted"));
        assert_eq!(
            app.oneshot(send_request("two")).await.unwrap().status(),
            StatusCode::ACCEPTED
        );
        let reused = tokio::time::timeout(std::time::Duration::from_secs(1), body_a.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap();
        let reused = String::from_utf8(reused.to_vec()).unwrap();
        assert!(reused.contains("id: 3"));
        assert!(reused.contains("reply"));
        assert_eq!(opens.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn http_stream_cannot_miss_an_event_during_subscription() {
        let state = Arc::new(AppState {
            config: Config::default(),
            root: std::env::temp_dir(),
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

    #[test]
    fn tab_fragment_has_a_usable_composer() {
        let html = ChatTab.render().unwrap();
        assert!(html.contains("id=\"chat-composer\""));
        assert!(!html.contains("\\\"chat-composer\\\""));
    }
}
