//! axum 0.8 dashboard server.
//!
//! Serves a single self-polling HTMX page (`GET /`), the embedded htmx asset
//! (`GET /assets/{*path}`), and three control endpoints that forward
//! `ControlMsg` to the orchestrator. The dashboard never touches issue state;
//! the only mutations it performs are sending stop/pause/resume messages.

pub mod view;

use std::net::IpAddr;

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use askama::Template;
use std::collections::HashMap;
use tokio::sync::watch;

use crate::state::{AppState, ControlMsg};
use view::DashboardTemplate;

/// Embedded static assets (only `htmx.min.js` in v0).
#[derive(rust_embed::Embed)]
#[folder = "assets/"]
struct Assets;

/// Build the router and serve until `shutdown` flips to `true`.
pub async fn serve(
    state: AppState,
    bind: IpAddr,
    port: u16,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/", get(index))
        .route("/control/stop", post(control_stop))
        .route("/control/pause", post(control_pause))
        .route("/control/resume", post(control_resume))
        .route("/assets/{*path}", get(asset))
        // Logs/Events API: paged run list and per-issue event cursor.
        .route("/api/runs", get(api_runs))
        .route("/api/events/{identifier}", get(api_events))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind((bind, port))
        .await
        .map_err(|e| anyhow::anyhow!("binding dashboard on {bind}:{port}: {e}"))?;

    axum::serve(listener, app.into_make_service())
        .with_graceful_shutdown(async move {
            // Resolve when shutdown is signalled (or the sender drops).
            while shutdown.changed().await.is_ok() {
                if *shutdown.borrow() {
                    break;
                }
            }
        })
        .await
        .map_err(|e| anyhow::anyhow!("dashboard server error: {e}"))?;

    Ok(())
}

/// `GET /` — render the dashboard page from the current state snapshot.
async fn index(State(state): State<AppState>) -> Response {
    let tmpl = DashboardTemplate::from_state(&state).await;
    match tmpl.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            tracing::error!("dashboard render failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "render error").into_response()
        }
    }
}

/// `POST /control/stop` — kill the active child, set run state `Cancelled`.
async fn control_stop(State(state): State<AppState>) -> StatusCode {
    send_control(&state, ControlMsg::Stop)
}

/// `POST /control/pause` — stop picking up new issues.
async fn control_pause(State(state): State<AppState>) -> StatusCode {
    send_control(&state, ControlMsg::Pause)
}

/// `POST /control/resume` — resume polling.
async fn control_resume(State(state): State<AppState>) -> StatusCode {
    send_control(&state, ControlMsg::Resume)
}

/// Forward a control message to the orchestrator; 204 on success.
fn send_control(state: &AppState, msg: ControlMsg) -> StatusCode {
    match state.control_tx.send(msg) {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

/// `GET /api/runs?page=N&size=N` — paged run list from SQLite (newest-first).
async fn api_runs(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let page: usize = params.get("page").and_then(|s| s.parse().ok()).unwrap_or(0);
    let size: usize = params.get("size").and_then(|s| s.parse().ok()).unwrap_or(50).min(250);
    match state.store.list_runs_paged(page, size) {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// `GET /api/events/{identifier}?since=N&limit=N` — events for an issue since
/// an `event_id` cursor (pass `since=0` for all). Returns JSON array of event
/// rows, ascending by `event_id`; the last row's `event_id` is the next cursor.
async fn api_events(
    State(state): State<AppState>,
    Path(identifier): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let since: i64 = params.get("since").and_then(|s| s.parse().ok()).unwrap_or(0);
    let limit: usize = params.get("limit").and_then(|s| s.parse().ok()).unwrap_or(100).min(500);
    match state.store.list_events_since(&identifier, since, limit) {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// `GET /assets/{*path}` — serve an embedded static asset.
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
