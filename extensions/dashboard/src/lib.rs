//! Dashboard extension mounted into the host HTTP server.
//!
//! Reads retained `RunSnapshot` values and sends control messages over the
//! host bus. It does not import the orchestrator implementation.

pub mod view;

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use cap_dashboard_tab::{escape_html, DashboardTabs};
use orchestrator_api::{ControlMsg, RunQuery, RunSnapshot, CONTROL_TOPIC, RUN_SNAPSHOT_TOPIC};
use serde::Deserialize;
use serde_json::json;
use std::sync::{Arc, OnceLock};
use view::{DashboardTemplate, RunDetailTemplate, TabNav};

#[derive(rust_embed::Embed)]
#[folder = "assets/"]
struct Assets;

#[derive(Default)]
pub struct DashboardExtension {
    bus: Arc<OnceLock<Arc<host_api::EventBus>>>,
    runs: Arc<OnceLock<Arc<dyn RunQuery>>>,
    tabs: Arc<OnceLock<Arc<DashboardTabs>>>,
}

impl host_api::Extension for DashboardExtension {
    fn id(&self) -> &'static str {
        "dashboard"
    }

    fn register<'a>(
        &'a self,
        ctx: &'a mut host_api::RegisterCtx,
    ) -> host_api::BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            // Ensure the shared tab registry exists so any extension that
            // registers a tab (in any order) shares one collector. Zero
            // registered tabs leaves the dashboard looking exactly as before.
            let _ = DashboardTabs::shared(&mut ctx.services);
            let state = BusApiState {
                bus: Arc::clone(&self.bus),
                runs: Arc::clone(&self.runs),
                tabs: Arc::clone(&self.tabs),
            };
            let app = Router::new()
                .route("/", get(index))
                .route("/content", get(content))
                .route("/tabs/{tab_id}", get(tab_fragment))
                .route("/runs/{run_id}", get(run_detail))
                .route("/runs/{run_id}/logs", get(run_logs))
                .route("/runs/{run_id}/interrupt", post(run_interrupt))
                .route("/runs/{run_id}/kill", post(run_kill))
                .route("/control/stop", post(control_stop))
                .route("/control/pause", post(control_pause))
                .route("/control/resume", post(control_resume))
                .route("/assets/{*path}", get(asset))
                .layer(axum::middleware::map_response(mark_prefix_aware))
                .with_state(state);
            ctx.http.mount(host_api::HttpMount {
                namespace: "/".to_string(),
                router: app,
                routes: vec![
                    "/".to_string(),
                    "/content".to_string(),
                    "/tabs/{tab_id}".to_string(),
                    "/runs/{run_id}".to_string(),
                    "/runs/{run_id}/logs".to_string(),
                    "/runs/{run_id}/interrupt".to_string(),
                    "/runs/{run_id}/kill".to_string(),
                    "/control/stop".to_string(),
                    "/control/pause".to_string(),
                    "/control/resume".to_string(),
                    "/assets/{*path}".to_string(),
                ],
                claim_root: true,
            })?;
            Ok(())
        })
    }

    fn start<'a>(&'a self, ctx: host_api::StartCtx) -> host_api::BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let _ = self.bus.set(ctx.host.bus.clone());
            // Run-detail drawer is optional: if no orchestrator registered a
            // RunQuery service, the /runs route degrades to 503.
            if let Ok(runs) = ctx.host.services.get_named::<dyn RunQuery>("orchestrator") {
                let _ = self.runs.set(runs);
            }
            // Snapshot the registered dashboard tabs (frozen by start time).
            let _ = self
                .tabs
                .set(DashboardTabs::from_services(&ctx.host.services));
            // Presence: announce this dashboard into the registry so a single
            // `dar dash` aggregator can discover and link to it. The host
            // surfaces the *actual* bound addr, which is what makes the
            // OS-assigned (`:0`) ephemeral port usable. Failure to register is
            // non-fatal: the agent's own dashboard still works standalone.
            if let Some(addr) = ctx.host.http_addr() {
                match register_presence(&ctx, addr) {
                    Ok((registry, entry)) => {
                        let cleanup = PresenceGuard { registry, entry };
                        let mut shutdown = ctx.shutdown.clone();
                        tokio::spawn(async move {
                            shutdown.cancelled().await;
                            cleanup.unlink();
                        });
                    }
                    Err(e) => tracing::warn!("dashboard presence registration failed: {e:#}"),
                }
            } else {
                tracing::warn!("dashboard: no bound HTTP addr; skipping presence registration");
            }
            Ok(())
        })
    }
}

/// Reachable address advertised in the presence file. The host binds on
/// `0.0.0.0` for direction A, but a literal `0.0.0.0` host is not dialable; the
/// fleet aggregator proxies browser traffic through `/agent/<port>/`, so it
/// dials loopback using this port; keep a placeholder host for direct access.
/// Uses `SocketAddr`'s own `Display` so IPv6 hosts are bracketed (`[::]:port`)
/// and the stored addr stays a well-formed `host:port`.
fn advertised_addr(bound: std::net::SocketAddr) -> String {
    bound.to_string()
}

#[derive(serde::Deserialize)]
struct AgentIdOnly {
    id: String,
}

/// Read just the `id` field from `<root>/agent.yaml`.
fn read_agent_id(root: &std::path::Path) -> anyhow::Result<String> {
    use anyhow::Context as _;
    let path = root.join("agent.yaml");
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let parsed: AgentIdOnly = serde_yaml::from_str(&raw)
        .with_context(|| format!("parsing id from {}", path.display()))?;
    Ok(parsed.id)
}

/// Registry directory from `extensions.dashboard.registry_dir`, else default.
fn registry_dir(ctx: &host_api::StartCtx) -> std::path::PathBuf {
    ctx.config
        .get("dashboard")
        .and_then(|v| v.get("registry_dir"))
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(dar_presence::default_registry_dir)
}

fn register_presence(
    ctx: &host_api::StartCtx,
    addr: std::net::SocketAddr,
) -> anyhow::Result<(dar_presence::Registry, dar_presence::PresenceEntry)> {
    let root = ctx.paths.root();
    let id = read_agent_id(root)?;
    let folder = root.to_string_lossy().to_string();
    let workflow = presence_workflow(&ctx.paths);
    let entry = dar_presence::PresenceEntry {
        id,
        folder,
        workflow,
        addr: advertised_addr(addr),
        pid: std::process::id(),
        // Millisecond boot identity: execv keeps PID, and seconds are too
        // coarse to prove an immediate in-place restart.
        started_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0),
    };
    let registry = dar_presence::Registry::new(registry_dir(ctx));
    registry.write(&entry)?;
    Ok((registry, entry))
}

fn presence_workflow(paths: &host_api::HostPaths) -> Option<String> {
    let workflow = paths.workflow_root().join("WORKFLOW.md");
    workflow
        .is_file()
        .then(|| workflow.to_string_lossy().into_owned())
}

struct PresenceGuard {
    registry: dar_presence::Registry,
    entry: dar_presence::PresenceEntry,
}

impl PresenceGuard {
    fn unlink(&self) {
        if let Err(e) = self.registry.remove(
            &self.entry.id,
            &self.entry.folder,
            self.entry.workflow.as_deref(),
        ) {
            tracing::warn!("dashboard presence unlink failed: {e:#}");
        }
    }
}

#[derive(Clone)]
struct BusApiState {
    bus: Arc<OnceLock<Arc<host_api::EventBus>>>,
    runs: Arc<OnceLock<Arc<dyn RunQuery>>>,
    tabs: Arc<OnceLock<Arc<DashboardTabs>>>,
}

fn tab_nav(api: &BusApiState) -> Vec<TabNav> {
    api.tabs
        .get()
        .map(|registry| {
            registry
                .snapshot()
                .iter()
                .map(|t| TabNav {
                    id: t.id().to_string(),
                    title: t.title().to_string(),
                    self_refreshing: t.self_refreshing(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// First registered tab claiming `passive_default()`, pre-rendered. `None`
/// when there is no such tab or its `render()` fails — either way the caller
/// falls back to the Runs view.
fn passive_default_tab(api: &BusApiState) -> Option<(String, String)> {
    let registry = api.tabs.get()?;
    let tab = registry
        .snapshot()
        .into_iter()
        .find(|t| t.passive_default())?;
    let fragment = tab.render().ok()?;
    Some((tab.id().to_string(), fragment))
}

/// Sanitize an inbound `x-forwarded-prefix` before it is echoed into the
/// shell's `<script src>` and `window.__dashPrefix`. Accepts only
/// `/agent/<port>`-shaped values: must start with `/`, contain only
/// `[A-Za-z0-9/_.-]` (rules out quotes/angle-brackets breaking out of the
/// `<script>` context), no `..` (path traversal) and no `//` (protocol-
/// relative confusion). A single trailing slash is stripped. Anything else,
/// including a missing header, sanitizes to `""` (direct-access behavior).
fn sanitize_prefix(raw: &str) -> String {
    let ok = raw.starts_with('/')
        && !raw.contains("//")
        && !raw.contains("..")
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '.' | '-'));
    if !ok {
        return String::new();
    }
    raw.strip_suffix('/').unwrap_or(raw).to_string()
}

/// Router-level `map_response` layer: marks every response from this
/// extension's first-party HTML as managing its own `X-Forwarded-Prefix`
/// awareness, so `dar dash`'s proxy skips its compatibility HTML rewriter.
async fn mark_prefix_aware(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert("x-prefix-aware", HeaderValue::from_static("1"));
    response
}

async fn index(State(api): State<BusApiState>, headers: axum::http::HeaderMap) -> Response {
    let Some(bus) = api.bus.get() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "dashboard bus unavailable").into_response();
    };
    let snapshot = bus
        .read_retained::<RunSnapshot>(RUN_SNAPSHOT_TOPIC)
        .unwrap_or_else(|_| RunSnapshot::empty());
    let tabs = tab_nav(&api);
    let prefix = headers
        .get("x-forwarded-prefix")
        .and_then(|v| v.to_str().ok())
        .map(sanitize_prefix)
        .unwrap_or_default();
    // Passive agents (no orchestration loop) have nothing to show on the Runs
    // view; let the first tab that claims passive_default() be the initial
    // active tab instead. The active loop always publishes a non-empty tracker
    // kind; an empty tracker is the genuine passive signal (passive_snapshot()).
    let passive = snapshot.agent.tracker.is_empty();
    let page = match passive.then(|| passive_default_tab(&api)).flatten() {
        Some((id, fragment)) => {
            DashboardTemplate::page_with_active_tab(snapshot, tabs, id, fragment, prefix)
        }
        None => DashboardTemplate::page_with_tabs(snapshot, tabs, prefix),
    };
    match page.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            tracing::error!("dashboard render failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "render error").into_response()
        }
    }
}

/// Dispatch `GET /tabs/{tab_id}` to the registered provider and splice its HTML
/// fragment into `#content` (innerHTML-swap). 404 when no tab matches.
async fn tab_fragment(State(api): State<BusApiState>, Path(tab_id): Path<String>) -> Response {
    let Some(registry) = api.tabs.get() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "dashboard tabs unavailable",
        )
            .into_response();
    };
    let Some(tab) = registry.find(&tab_id) else {
        return (StatusCode::NOT_FOUND, "tab not found").into_response();
    };
    match tab.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            tracing::error!("dashboard tab {tab_id} render failed: {e:#}");
            (StatusCode::INTERNAL_SERVER_ERROR, "tab render error").into_response()
        }
    }
}

#[derive(Deserialize)]
struct ContentQuery {
    page: Option<usize>,
}

async fn content(State(api): State<BusApiState>, Query(q): Query<ContentQuery>) -> Response {
    let Some(bus) = api.bus.get() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "dashboard bus unavailable").into_response();
    };
    let snapshot = bus
        .read_retained::<RunSnapshot>(RUN_SNAPSHOT_TOPIC)
        .unwrap_or_else(|_| RunSnapshot::empty());
    let page = q.page.unwrap_or(1).max(1);
    match view::ContentTemplate::from_snapshot_page(snapshot, page).render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            tracing::error!("dashboard render failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "render error").into_response()
        }
    }
}

async fn run_detail(State(api): State<BusApiState>, Path(run_id): Path<String>) -> Response {
    let Some(runs) = api.runs.get() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "run detail unavailable").into_response();
    };
    let Some(run) = runs.run(&run_id) else {
        return (StatusCode::NOT_FOUND, "run not found").into_response();
    };
    let events = runs.events_for_run(&run_id, 0, i64::MAX as usize);
    match RunDetailTemplate::build(run, events).render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            tracing::error!("run detail render failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "render error").into_response()
        }
    }
}

async fn control_stop(State(api): State<BusApiState>) -> StatusCode {
    send_control(&api, ControlMsg::Stop)
}

async fn control_pause(State(api): State<BusApiState>) -> StatusCode {
    send_control(&api, ControlMsg::Pause)
}

async fn control_resume(State(api): State<BusApiState>) -> StatusCode {
    send_control(&api, ControlMsg::Resume)
}

fn send_control(_api: &BusApiState, msg: ControlMsg) -> StatusCode {
    let Some(bus) = _api.bus.get() else {
        return StatusCode::SERVICE_UNAVAILABLE;
    };
    match bus.publish(CONTROL_TOPIC, msg) {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

/// Parse an event payload into `(row_type, text)`.  For a `protocol_event`
/// envelope the inner `log_row` and `text` fields are returned; for any other
/// payload the event kind and raw payload string are returned unchanged.
fn protocol_event_parts(payload: &str, kind: &str) -> (String, String) {
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(payload) {
        if map.get("type").and_then(|v| v.as_str()) == Some("protocol_event") {
            let rt = map
                .get("log_row")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let tx = map
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            return (rt, tx);
        }
    }
    (kind.to_string(), payload.to_string())
}

#[derive(Deserialize)]
struct LogsQuery {
    since: Option<i64>,
    limit: Option<usize>,
    view: Option<String>,
}

async fn run_logs(
    State(api): State<BusApiState>,
    Path(run_id): Path<String>,
    Query(q): Query<LogsQuery>,
    headers: axum::http::HeaderMap,
) -> Response {
    let Some(runs) = api.runs.get() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "run detail unavailable").into_response();
    };
    let since = q.since.unwrap_or(0);
    if headers.contains_key("hx-request") {
        if q.view.as_deref() == Some("events") {
            // Events tab incremental poll: append only new ev-row divs.
            let events = runs.events_for_run(&run_id, since, i64::MAX as usize);
            let mut html = String::new();
            for e in &events {
                let (row_type, text) = protocol_event_parts(&e.payload, &e.kind);
                if row_type.is_empty() && text.is_empty() {
                    continue;
                }
                let tag = if row_type.is_empty() {
                    escape_html(&e.kind)
                } else {
                    escape_html(&row_type)
                };
                html.push_str(&format!(
                    "<div class=\"ev-row\" data-event-id=\"{}\"><span class=\"ev-meta\"><span class=\"ev-tag\">{}</span><span class=\"ev-ts\">{}</span></span><span class=\"ev-text\">{}</span></div>",
                    e.event_id,
                    tag,
                    escape_html(&view::fmt_event_ts(&e.ts)),
                    escape_html(&text),
                ));
            }
            // Empty body = no-op for hx-swap="beforeend".
            return Html(html).into_response();
        }
        // Logs tab incremental poll: append only new log rows after `since`.
        let events = runs.events_for_run(&run_id, since, i64::MAX as usize);
        let mut html = String::new();
        for e in &events {
            if let Ok(serde_json::Value::Object(map)) =
                serde_json::from_str::<serde_json::Value>(&e.payload)
            {
                if map.get("type").and_then(|v| v.as_str()) == Some("protocol_event") {
                    let rt = map.get("log_row").and_then(|v| v.as_str()).unwrap_or("");
                    if rt.is_empty() {
                        continue;
                    }
                    let el = view::EventLine {
                        event_id: e.event_id,
                        ts: view::fmt_event_ts(&e.ts),
                        kind: e.kind.clone(),
                        row_type: rt.to_string(),
                        text: map
                            .get("text")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        detail: map
                            .get("detail")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        rendered: String::new(),
                    };
                    html.push_str(&view::render_log_row(&el));
                }
            }
        }
        // Empty body = no-op for hx-swap="beforeend".
        return Html(html).into_response();
    }
    let limit = q.limit.unwrap_or(100).min(500);
    let events = runs.events_for_run(&run_id, since, limit);
    let next_cursor = events.last().map(|e| e.event_id);
    let rows: Vec<serde_json::Value> = events
        .into_iter()
        .map(|e| {
            let (row_type, text) = protocol_event_parts(&e.payload, &e.kind);
            json!({
                "event_id": e.event_id,
                "kind": e.kind,
                "ts": e.ts,
                "payload": e.payload,
                "row_type": row_type,
                "text": text,
            })
        })
        .collect();
    let body = json!({"events": rows, "next_cursor": next_cursor});
    (
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

/// Shared plumbing for run-scoped control messages that carry a reply channel.
/// Checks the bus, publishes `msg`, awaits the reply with a 5 s timeout, and
/// maps the three outcomes to HTTP responses.  The caller creates the reply
/// channel and embeds `reply_tx` into `msg` before calling this helper.
async fn send_run_control(
    api: &BusApiState,
    msg: ControlMsg,
    reply_rx: tokio::sync::oneshot::Receiver<orchestrator_api::ControlReply>,
) -> Response {
    let Some(bus) = api.bus.get() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"ok":false,"message":"bus unavailable"}).to_string(),
        )
            .into_response();
    };
    if bus.publish(CONTROL_TOPIC, msg).is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"ok":false,"message":"bus unavailable"}).to_string(),
        )
            .into_response();
    }
    match tokio::time::timeout(std::time::Duration::from_secs(5), reply_rx).await {
        Ok(Ok(reply)) => {
            if reply.ok {
                (
                    [(header::CONTENT_TYPE, "application/json")],
                    json!({"ok":true,"message":reply.message}).to_string(),
                )
                    .into_response()
            } else {
                (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    json!({"ok":false,"message":reply.message}).to_string(),
                )
                    .into_response()
            }
        }
        _ => (
            StatusCode::GATEWAY_TIMEOUT,
            json!({"ok":false,"message":"timeout"}).to_string(),
        )
            .into_response(),
    }
}

async fn run_interrupt(State(api): State<BusApiState>, Path(run_id): Path<String>) -> Response {
    let (reply_tx, reply_rx) = orchestrator_api::reply_channel();
    let msg = ControlMsg::Interrupt {
        run_id,
        reply: reply_tx,
    };
    send_run_control(&api, msg, reply_rx).await
}

async fn run_kill(State(api): State<BusApiState>, Path(run_id): Path<String>) -> Response {
    let (reply_tx, reply_rx) = orchestrator_api::reply_channel();
    let msg = ControlMsg::Kill {
        run_id,
        reply: reply_tx,
    };
    send_run_control(&api, msg, reply_rx).await
}

async fn asset(Path(path): Path<String>) -> Response {
    match Assets::get(&path) {
        Some(content) => {
            let mime = mime_guess::from_path(&path).first_or_octet_stream();
            (
                [(header::CONTENT_TYPE, mime.as_ref().to_string())],
                content.data.into_owned(),
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passive_agent_has_no_presence_workflow() {
        let root = tempfile::tempdir().unwrap();
        let paths = host_api::HostPaths::new(root.path()).unwrap();

        assert_eq!(presence_workflow(&paths), None);
    }

    #[test]
    fn presence_reports_existing_workflow() {
        let root = tempfile::tempdir().unwrap();
        let workflow = root.path().join("WORKFLOW.md");
        std::fs::write(&workflow, "").unwrap();
        let paths = host_api::HostPaths::new(root.path()).unwrap();
        let expected = paths
            .workflow_root()
            .join("WORKFLOW.md")
            .to_string_lossy()
            .into_owned();

        assert_eq!(presence_workflow(&paths), Some(expected));
    }

    fn make_state() -> BusApiState {
        BusApiState {
            bus: Arc::new(OnceLock::new()),
            runs: Arc::new(OnceLock::new()),
            tabs: Arc::new(OnceLock::new()),
        }
    }

    #[tokio::test]
    async fn interrupt_returns_503_when_bus_absent() {
        let state = make_state();
        let resp = run_interrupt(
            axum::extract::State(state),
            axum::extract::Path("ALG-1-123".to_string()),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn kill_returns_503_when_bus_absent() {
        let state = make_state();
        let resp = run_kill(
            axum::extract::State(state),
            axum::extract::Path("ALG-1-123".to_string()),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn tab_fragment_returns_503_when_registry_absent() {
        let state = make_state();
        let resp = tab_fragment(
            axum::extract::State(state),
            axum::extract::Path("demo".to_string()),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn tab_fragment_404_for_unknown_tab() {
        let state = make_state();
        let _ = state.tabs.set(Arc::new(DashboardTabs::default()));
        let resp = tab_fragment(
            axum::extract::State(state),
            axum::extract::Path("nope".to_string()),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn tab_fragment_renders_registered_provider() {
        struct Demo;
        impl cap_dashboard_tab::DashboardTab for Demo {
            fn id(&self) -> &str {
                "demo"
            }
            fn title(&self) -> &str {
                "Demo"
            }
            fn render(&self) -> anyhow::Result<String> {
                Ok("<p id=\"demo-body\">hi</p>".to_string())
            }
        }
        let registry = Arc::new(DashboardTabs::default());
        registry.add(Arc::new(Demo)).unwrap();
        let state = make_state();
        let _ = state.tabs.set(registry);
        let resp = tab_fragment(
            axum::extract::State(state),
            axum::extract::Path("demo".to_string()),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("demo-body"), "fragment body spliced: {html}");
    }

    struct SelfRefreshingTab;
    impl cap_dashboard_tab::DashboardTab for SelfRefreshingTab {
        fn id(&self) -> &str {
            "live"
        }
        fn title(&self) -> &str {
            "Live"
        }
        fn render(&self) -> anyhow::Result<String> {
            Ok(String::new())
        }
        fn self_refreshing(&self) -> bool {
            true
        }
    }

    #[test]
    fn tab_nav_carries_self_refreshing_through() {
        let registry = Arc::new(DashboardTabs::default());
        registry.add(Arc::new(SelfRefreshingTab)).unwrap();
        let state = make_state();
        let _ = state.tabs.set(registry);
        let nav = tab_nav(&state);
        assert_eq!(nav.len(), 1);
        assert!(nav[0].self_refreshing, "self_refreshing passed through");
    }

    struct ClaimTab;
    impl cap_dashboard_tab::DashboardTab for ClaimTab {
        fn id(&self) -> &str {
            "claim"
        }
        fn title(&self) -> &str {
            "Claim"
        }
        fn render(&self) -> anyhow::Result<String> {
            Ok("<p id=\"claim-body\">hi</p>".to_string())
        }
        fn passive_default(&self) -> bool {
            true
        }
    }

    fn snapshot_with_tracker(tracker: &str) -> RunSnapshot {
        let mut s = RunSnapshot::empty();
        s.agent.tracker = tracker.to_string();
        s
    }

    #[tokio::test]
    async fn index_passive_snapshot_activates_claiming_tab() {
        // No retained snapshot registered -> RunSnapshot::empty() (tracker
        // empty), which is the passive case.
        let state = make_state();
        let _ = state.bus.set(Arc::new(host_api::EventBus::new()));
        let registry = Arc::new(DashboardTabs::default());
        registry.add(Arc::new(ClaimTab)).unwrap();
        let _ = state.tabs.set(registry);
        let resp = index(axum::extract::State(state), axum::http::HeaderMap::new()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            html.contains("claim-body"),
            "claiming tab's fragment is the initial #content body: {html}"
        );
        let claim_pos = html.find("/tabs/claim").expect("claim button present");
        let snippet = &html[claim_pos.saturating_sub(60)..claim_pos];
        assert!(
            snippet.contains("dash-tab active"),
            "claiming tab's button is active: {snippet}"
        );
    }

    #[tokio::test]
    async fn index_non_passive_snapshot_keeps_runs_active_despite_claimant() {
        let state = make_state();
        let mut bus = host_api::EventBus::new();
        bus.register_retained(RUN_SNAPSHOT_TOPIC, snapshot_with_tracker("files"))
            .unwrap();
        let _ = state.bus.set(Arc::new(bus));
        let registry = Arc::new(DashboardTabs::default());
        registry.add(Arc::new(ClaimTab)).unwrap();
        let _ = state.tabs.set(registry);
        let resp = index(axum::extract::State(state), axum::http::HeaderMap::new()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            !html.contains("claim-body"),
            "non-passive agent does not use the claimant's fragment as initial body: {html}"
        );
        let runs_pos = html.find("/content").expect("runs button present");
        let snippet = &html[runs_pos.saturating_sub(60)..runs_pos];
        assert!(
            snippet.contains("dash-tab active"),
            "Runs button stays active for a non-passive agent: {snippet}"
        );
    }

    #[tokio::test]
    async fn htmx_poll_returns_503_when_runs_absent() {
        let state = make_state();
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("hx-request", "true".parse().unwrap());
        let resp = run_logs(
            axum::extract::State(state),
            axum::extract::Path("r1".to_string()),
            axum::extract::Query(LogsQuery {
                since: Some(0),
                limit: None,
                view: None,
            }),
            headers,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn sanitize_prefix_accepts_agent_port_shape() {
        assert_eq!(sanitize_prefix("/agent/50123"), "/agent/50123");
        assert_eq!(
            sanitize_prefix("/agent/50123/"),
            "/agent/50123",
            "single trailing slash stripped"
        );
    }

    #[test]
    fn sanitize_prefix_rejects_hostile_values() {
        assert_eq!(sanitize_prefix(""), "");
        assert_eq!(sanitize_prefix("agent/50123"), "", "must start with /");
        assert_eq!(
            sanitize_prefix("/\"><script>"),
            "",
            "script-breakout chars rejected"
        );
        assert_eq!(sanitize_prefix("/a/../b"), "", "path traversal rejected");
        assert_eq!(
            sanitize_prefix("//evil.example"),
            "",
            "protocol-relative rejected"
        );
    }

    fn header_map(pairs: &[(&str, &str)]) -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        for (k, v) in pairs {
            headers.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        headers
    }

    #[tokio::test]
    async fn index_with_forwarded_prefix_renders_prefixed_asset_and_dash_prefix() {
        let state = make_state();
        let _ = state.bus.set(Arc::new(host_api::EventBus::new()));
        let resp = index(
            axum::extract::State(state),
            header_map(&[("x-forwarded-prefix", "/agent/50123")]),
        )
        .await;
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            html.contains(r#"src="/agent/50123/assets/htmx.min.js""#),
            "htmx asset src prefixed: {html}"
        );
        assert!(
            html.contains(r#"window.__dashPrefix = "/agent/50123";"#),
            "dashPrefix set: {html}"
        );
    }

    #[tokio::test]
    async fn index_without_forwarded_prefix_renders_direct_access() {
        let state = make_state();
        let _ = state.bus.set(Arc::new(host_api::EventBus::new()));
        let resp = index(axum::extract::State(state), axum::http::HeaderMap::new()).await;
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            html.contains(r#"src="/assets/htmx.min.js""#),
            "unprefixed htmx asset src: {html}"
        );
        assert!(
            html.contains(r#"window.__dashPrefix = "";"#),
            "empty dashPrefix: {html}"
        );
    }

    #[tokio::test]
    async fn index_sanitizes_hostile_forwarded_prefix() {
        let state = make_state();
        let _ = state.bus.set(Arc::new(host_api::EventBus::new()));
        let resp = index(
            axum::extract::State(state),
            header_map(&[("x-forwarded-prefix", "/a/../b")]),
        )
        .await;
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            html.contains(r#"src="/assets/htmx.min.js""#),
            "hostile prefix sanitized to empty: {html}"
        );
    }

    #[tokio::test]
    async fn content_response_carries_prefix_aware_header() {
        use tower::ServiceExt;
        let state = make_state();
        let _ = state.bus.set(Arc::new(host_api::EventBus::new()));
        let app = Router::new()
            .route("/content", get(content))
            .layer(axum::middleware::map_response(mark_prefix_aware))
            .with_state(state);
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/content")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.headers().get("x-prefix-aware").map(|v| v.as_bytes()),
            Some(&b"1"[..]),
            "first-party HTML opts out of the proxy's rewriter"
        );
    }
}
