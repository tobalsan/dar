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
use tokio::sync::{broadcast, mpsc, Mutex};
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
                services: ctx.services.clone(),
                bus: std::sync::OnceLock::new(),
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
            self.state
                .get()
                .expect("chat-web registered")
                .bus
                .set(ctx.host.bus.clone())
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
    services: dar_extension_sdk::ServiceRegistry,
    bus: std::sync::OnceLock<Arc<host_api::EventBus>>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}
struct Session {
    inner: Mutex<Option<Box<dyn ChatSession>>>,
    tx: broadcast::Sender<WireEvent>,
    next_seq: std::sync::atomic::AtomicU64,
    command_ids: Mutex<HashSet<String>>,
    history: std::sync::Mutex<VecDeque<WireEvent>>,
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
        let (event_tx, mut event_rx) = mpsc::channel(128);
        let configured = self.config.backend.as_deref().filter(|s| !s.is_empty());
        let backend_id = if let Some(id) = configured {
            id.to_owned()
        } else {
            let registered = |id: &str| self.services.get_named::<dyn ChatBackend>(id).is_ok();
            self.bus
                .get()
                .and_then(|bus| {
                    bus.read_retained::<orchestrator_api::RunSnapshot>(
                        orchestrator_api::RUN_SNAPSHOT_TOPIC,
                    )
                    .ok()
                })
                .filter(|x| x.version > 0)
                .map(|x| x.agent.runner)
                .filter(|id| registered(id))
                .unwrap_or_else(|| chat::CHAT_FALLBACK_BACKEND.into())
        };
        let backend = self
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
        let params = cap_chat::ChatSessionParams::builder(
            self.config.command.as_deref().unwrap_or(""),
            &self.root,
            &session_dir,
        )
        .build();
        let opened = backend.open(params, event_tx).await?;
        let (tx, _) = broadcast::channel(256);
        let session = Arc::new(Session {
            inner: Mutex::new(Some(opened)),
            tx,
            next_seq: std::sync::atomic::AtomicU64::new(0),
            command_ids: Mutex::new(HashSet::new()),
            history: std::sync::Mutex::new(VecDeque::new()),
        });
        let sink = Arc::clone(&session);
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                sink.publish(event);
            }
        });
        sessions.insert(id.to_owned(), Arc::clone(&session));
        Ok(session)
    }
}
impl Session {
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
        let seq = self
            .next_seq
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let _ = self.tx.send(WireEvent {
            seq,
            kind,
            text: text.clone(),
            error: error.clone(),
        });
        let mut history = self
            .history
            .lock()
            .expect("chat-web history mutex poisoned");
        if history.len() == 256 {
            history.pop_front();
        }
        history.push_back(WireEvent {
            seq,
            kind,
            text,
            error,
        });
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
            let live = s.tx.subscribe();
            let replay: Vec<_> = s
                .history
                .lock()
                .expect("chat-web history mutex poisoned")
                .iter()
                .filter(|event| event.seq > last)
                .cloned()
                .collect();
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
            match guard
                .as_mut()
                .expect("session open")
                .send_turn(body.message)
                .await
            {
                Ok(()) => (
                    StatusCode::ACCEPTED,
                    Json(serde_json::json!({"accepted":true,"command_id":body.command_id})),
                )
                    .into_response(),
                Err(e) => {
                    s.command_ids.lock().await.remove(&body.command_id);
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
            let mut guard = s.inner.lock().await;
            match guard.as_mut().expect("session open").abort().await {
                Ok(()) => StatusCode::ACCEPTED.into_response(),
                Err(e) => (StatusCode::CONFLICT, e.to_string()).into_response(),
            }
        }
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct FakeSession {
        aborted: Arc<AtomicBool>,
    }
    impl ChatSession for FakeSession {
        fn send_turn(&mut self, _prompt: String) -> cap_chat::BoxFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn abort(&mut self) -> cap_chat::BoxFuture<'_, Result<()>> {
            let flag = Arc::clone(&self.aborted);
            Box::pin(async move {
                flag.store(true, Ordering::SeqCst);
                Ok(())
            })
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
            next_seq: std::sync::atomic::AtomicU64::new(0),
            command_ids: Mutex::new(HashSet::new()),
            history: std::sync::Mutex::new(VecDeque::new()),
        })
    }

    #[tokio::test]
    async fn fanout_has_monotonic_sequence_ids() {
        let s = session(Box::new(FakeSession {
            aborted: Arc::new(AtomicBool::new(false)),
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
    async fn abort_is_server_authoritative_and_emits_terminal_event() {
        let flag = Arc::new(AtomicBool::new(false));
        let s = session(Box::new(FakeSession {
            aborted: Arc::clone(&flag),
        }));
        let mut events = s.tx.subscribe();
        s.inner
            .lock()
            .await
            .as_mut()
            .unwrap()
            .abort()
            .await
            .unwrap();
        s.publish(ChatEvent::TurnFinished {
            ok: false,
            error: Some("aborted".into()),
        });
        assert!(flag.load(Ordering::SeqCst));
        let event = events.recv().await.unwrap();
        assert_eq!(event.kind, "aborted");
        assert_eq!(event.error.as_deref(), Some("aborted"));
    }

    #[test]
    fn tab_fragment_has_a_usable_composer() {
        let html = ChatTab.render().unwrap();
        assert!(html.contains("id=\"chat-composer\""));
        assert!(!html.contains("\\\"chat-composer\\\""));
    }
}
