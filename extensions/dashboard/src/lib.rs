//! Dashboard extension mounted into the host HTTP server.
//!
//! Reads retained `RunSnapshot` values and sends control messages over the
//! host bus. It does not import the orchestrator implementation.

pub mod view;

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::json;
use orchestrator_api::{ControlMsg, RunQuery, RunSnapshot, CONTROL_TOPIC, RUN_SNAPSHOT_TOPIC};
use std::sync::{Arc, OnceLock};
use view::{DashboardTemplate, RunDetailTemplate};

#[derive(rust_embed::Embed)]
#[folder = "assets/"]
struct Assets;

#[derive(Default)]
pub struct DashboardExtension {
    bus: Arc<OnceLock<Arc<host_api::EventBus>>>,
    runs: Arc<OnceLock<Arc<dyn RunQuery>>>,
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
            let state = BusApiState {
                bus: Arc::clone(&self.bus),
                runs: Arc::clone(&self.runs),
            };
            let app = Router::new()
                .route("/", get(index))
                .route("/content", get(content))
                .route("/runs/{run_id}", get(run_detail))
                .route("/runs/{run_id}/logs", get(run_logs))
                .route("/runs/{run_id}/interrupt", post(run_interrupt))
                .route("/runs/{run_id}/kill", post(run_kill))
                .route("/control/stop", post(control_stop))
                .route("/control/pause", post(control_pause))
                .route("/control/resume", post(control_resume))
                .route("/assets/{*path}", get(asset))
                .with_state(state);
            ctx.http.mount(host_api::HttpMount {
                namespace: "/".to_string(),
                router: app,
                routes: vec![
                    "/".to_string(),
                    "/content".to_string(),
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
            Ok(())
        })
    }
}

#[derive(Clone)]
struct BusApiState {
    bus: Arc<OnceLock<Arc<host_api::EventBus>>>,
    runs: Arc<OnceLock<Arc<dyn RunQuery>>>,
}

async fn index(State(api): State<BusApiState>) -> Response {
    let Some(bus) = api.bus.get() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "dashboard bus unavailable").into_response();
    };
    let snapshot = bus
        .read_retained::<RunSnapshot>(RUN_SNAPSHOT_TOPIC)
        .unwrap_or_else(|_| RunSnapshot::empty());
    match DashboardTemplate::page(snapshot).render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            tracing::error!("dashboard render failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "render error").into_response()
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
                let (row_type, text) = if let Ok(serde_json::Value::Object(map)) =
                    serde_json::from_str::<serde_json::Value>(&e.payload)
                {
                    if map.get("type").and_then(|v| v.as_str()) == Some("protocol_event") {
                        let rt = map.get("log_row").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let tx = map.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        (rt, tx)
                    } else {
                        (e.kind.clone(), e.payload.clone())
                    }
                } else {
                    (e.kind.clone(), e.payload.clone())
                };
                if row_type.is_empty() && text.is_empty() {
                    continue;
                }
                let tag = if row_type.is_empty() { view::he(&e.kind) } else { view::he(&row_type) };
                html.push_str(&format!(
                    "<div class=\"ev-row\" data-event-id=\"{}\"><span class=\"ev-meta\"><span class=\"ev-tag\">{}</span><span class=\"ev-ts\">{}</span></span><span class=\"ev-text\">{}</span></div>",
                    e.event_id,
                    tag,
                    view::he(&view::fmt_event_ts(&e.ts)),
                    view::he(&text),
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
                        text: map.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        detail: map.get("detail").and_then(|v| v.as_str()).unwrap_or("").to_string(),
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
            let (row_type, text) = if let Ok(serde_json::Value::Object(map)) =
                serde_json::from_str::<serde_json::Value>(&e.payload)
            {
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
                    (rt, tx)
                } else {
                    (e.kind.clone(), e.payload.clone())
                }
            } else {
                (e.kind.clone(), e.payload.clone())
            };
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

async fn run_interrupt(
    State(api): State<BusApiState>,
    Path(run_id): Path<String>,
) -> Response {
    let Some(bus) = api.bus.get() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"ok":false,"message":"bus unavailable"}).to_string(),
        )
            .into_response();
    };
    let (reply_tx, reply_rx) = orchestrator_api::reply_channel();
    let msg = ControlMsg::Interrupt {
        run_id,
        reply: reply_tx,
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

async fn run_kill(
    State(api): State<BusApiState>,
    Path(run_id): Path<String>,
) -> Response {
    let Some(bus) = api.bus.get() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"ok":false,"message":"bus unavailable"}).to_string(),
        )
            .into_response();
    };
    let (reply_tx, reply_rx) = orchestrator_api::reply_channel();
    let msg = ControlMsg::Kill {
        run_id,
        reply: reply_tx,
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

    fn make_state() -> BusApiState {
        BusApiState {
            bus: Arc::new(OnceLock::new()),
            runs: Arc::new(OnceLock::new()),
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
    async fn htmx_poll_returns_503_when_runs_absent() {
        let state = make_state();
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("hx-request", "true".parse().unwrap());
        let resp = run_logs(
            axum::extract::State(state),
            axum::extract::Path("r1".to_string()),
            axum::extract::Query(LogsQuery { since: Some(0), limit: None, view: None }),
            headers,
        ).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
