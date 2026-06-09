//! axum 0.8 dashboard server.
//!
//! Serves a single self-polling HTMX page (`GET /`), the embedded htmx asset
//! (`GET /assets/{*path}`), and three control endpoints that forward
//! `ControlMsg` to the orchestrator. The dashboard never touches issue state;
//! the only mutations it performs are sending stop/pause/resume messages.

pub mod view;

use std::net::IpAddr;

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use std::collections::HashMap;
use tokio::sync::watch;

use crate::state::{AppState, ControlMsg};
use crate::store::{EventRow, Store};
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
        .route("/api/runs/{run_id}/logs", get(api_run_logs))
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
    let size: usize = params
        .get("size")
        .and_then(|s| s.parse().ok())
        .unwrap_or(50)
        .min(250);
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
    let since: i64 = params
        .get("since")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let limit: usize = params
        .get("limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(100)
        .min(500);
    match state.store.list_events_since(&identifier, since, limit) {
        Ok(rows) => Json(
            rows.into_iter()
                .map(UiLogRow::from)
                .collect::<Vec<UiLogRow>>(),
        )
        .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Debug, serde::Serialize)]
struct EventsPage {
    events: Vec<UiLogRow>,
    next_cursor: Option<i64>,
}

#[derive(Debug, serde::Serialize)]
struct UiLogRow {
    event_id: i64,
    run_id: Option<String>,
    issue_identifier: String,
    kind: String,
    payload: String,
    ts: String,
    row_type: String,
    text: String,
}

impl From<EventRow> for UiLogRow {
    fn from(row: EventRow) -> Self {
        let (row_type, text) = normalized_log_fields(&row);
        Self {
            event_id: row.event_id,
            run_id: row.run_id,
            issue_identifier: row.issue_identifier,
            kind: row.kind,
            payload: row.payload,
            ts: row.ts,
            row_type,
            text,
        }
    }
}

fn normalized_log_fields(row: &EventRow) -> (String, String) {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&row.payload) {
        let row_type = value
            .get("log_row")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| normalize_log_row(&row.kind, &row.payload))
            .to_string();
        let text = value
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or(&row.payload);
        (row_type, strip_ansi(text))
    } else {
        let text = strip_ansi(&row.payload);
        (normalize_log_row(&row.kind, &text).to_string(), text)
    }
}

fn normalize_log_row(kind: &str, text: &str) -> &'static str {
    let lower = text.to_ascii_lowercase();
    if kind == "stderr" || lower.contains("error") || lower.contains("\"type\":\"error\"") {
        "error"
    } else if lower.contains("thinking") || lower.contains("thought") {
        "thinking"
    } else if lower.contains("tool_call") || lower.contains("tool use") {
        "tool_call"
    } else if lower.contains("tool_output") || lower.contains("tool result") {
        "tool_output"
    } else if lower.contains("\"role\":\"user\"") {
        "user"
    } else {
        "assistant"
    }
}

fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn events_page_for_run(
    store: &Store,
    run_id: &str,
    since: i64,
    limit: usize,
) -> anyhow::Result<EventsPage> {
    let rows = store.list_events_for_run(run_id, since, limit)?;
    let next_cursor = rows.last().map(|row| row.event_id);
    let events = rows.into_iter().map(UiLogRow::from).collect();
    Ok(EventsPage {
        events,
        next_cursor,
    })
}

/// `GET /api/runs/{run_id}/logs?since=N&limit=N` — events for one run since
/// an `event_id` cursor. Returns `{ events, next_cursor }`.
async fn api_run_logs(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let since: i64 = params
        .get("since")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let limit: usize = params
        .get("limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(100)
        .min(500);
    match events_page_for_run(&state.store, &run_id, since, limit) {
        Ok(page) => Json(page).into_response(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use axum::body;
    use chrono::Utc;
    use serde_json::Value;
    use tempfile::tempdir;
    use tokio::sync::mpsc;

    use crate::state::AgentInfo;
    use crate::store::{NewEvent, NewRun};

    #[test]
    fn run_logs_page_returns_events_and_cursor() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("store.db")).unwrap();
        let now = Utc::now();
        let run_id = crate::store::new_run_id("DASH-1", &now);

        store
            .insert_run(&NewRun {
                run_id: &run_id,
                issue_id: "dash-1",
                issue_identifier: "DASH-1",
                workspace: "/tmp/ws",
                profile_json: None,
                workflow_path: None,
                workflow_sha: None,
                pid: 100,
                worker_id: None,
                started_at: now,
            })
            .unwrap();

        for payload in ["one", "\u{1b}[31mtool_call\u{1b}[0m: run", "three"] {
            store
                .insert_event(&NewEvent {
                    run_id: Some(&run_id),
                    issue_identifier: "DASH-1",
                    kind: "stdout",
                    payload,
                    ts: now,
                })
                .unwrap();
        }

        let page = events_page_for_run(&store, &run_id, 0, 2).unwrap();
        assert_eq!(page.events.len(), 2);
        assert_eq!(page.next_cursor, Some(page.events[1].event_id));
        assert_eq!(page.events[1].row_type, "tool_call");
        assert_eq!(page.events[1].text, "tool_call: run");

        let next = events_page_for_run(&store, &run_id, page.next_cursor.unwrap(), 2).unwrap();
        assert_eq!(next.events.len(), 1);
        assert_eq!(next.events[0].payload, "three");
    }

    #[tokio::test]
    async fn api_run_logs_handler_returns_events_and_cursor() {
        let dir = tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("store.db")).unwrap());
        let now = Utc::now();
        let run_id = crate::store::new_run_id("DASH-2", &now);

        store
            .insert_run(&NewRun {
                run_id: &run_id,
                issue_id: "dash-2",
                issue_identifier: "DASH-2",
                workspace: "/tmp/ws",
                profile_json: None,
                workflow_path: None,
                workflow_sha: None,
                pid: 101,
                worker_id: None,
                started_at: now,
            })
            .unwrap();

        let mut event_ids = Vec::new();
        for payload in ["one", "two", "three", "four"] {
            let event_id = store
                .insert_event(&NewEvent {
                    run_id: Some(&run_id),
                    issue_identifier: "DASH-2",
                    kind: "stdout",
                    payload,
                    ts: now,
                })
                .unwrap();
            event_ids.push(event_id);
        }

        let state = test_state(Arc::clone(&store));
        let response = api_run_logs(
            State(state.clone()),
            Path(run_id.clone()),
            Query(HashMap::from([
                ("since".to_string(), event_ids[1].to_string()),
                ("limit".to_string(), "2".to_string()),
            ])),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();

        let events = json["events"].as_array().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["payload"], "three");
        assert_eq!(events[0]["row_type"], "assistant");
        assert_eq!(events[0]["text"], "three");
        assert_eq!(events[1]["payload"], "four");
        assert_eq!(json["next_cursor"], event_ids[3]);

        let missing = api_run_logs(
            State(state),
            Path("missing-run".to_string()),
            Query(HashMap::from([
                ("since".to_string(), "0".to_string()),
                ("limit".to_string(), "2".to_string()),
            ])),
        )
        .await;

        assert_eq!(missing.status(), StatusCode::OK);
        let body = body::to_bytes(missing.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["events"].as_array().unwrap().len(), 0);
        assert!(json["next_cursor"].is_null());
    }

    fn test_state(store: Arc<Store>) -> AppState {
        let (control_tx, _control_rx) = mpsc::unbounded_channel();
        AppState::new(
            AgentInfo {
                id: "test-agent".to_string(),
                folder: "/tmp/agent".to_string(),
                tracker: "file".to_string(),
                runner: "claude".to_string(),
            },
            control_tx,
            store,
            Vec::new(),
        )
    }
}
