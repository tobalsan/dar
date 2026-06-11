//! Dashboard extension mounted into the host HTTP server.
//!
//! Reads retained `RunSnapshot` values and sends control messages over the
//! host bus. It does not import the orchestrator implementation.

pub mod view;

use askama::Template;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use orchestrator_api::{ControlMsg, RunSnapshot, CONTROL_TOPIC, RUN_SNAPSHOT_TOPIC};
use std::sync::{Arc, OnceLock};
use view::DashboardTemplate;

#[derive(rust_embed::Embed)]
#[folder = "assets/"]
struct Assets;

#[derive(Default)]
pub struct DashboardExtension {
    bus: Arc<OnceLock<Arc<host_api::EventBus>>>,
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
            };
            let app = Router::new()
                .route("/", get(index))
                .route("/content", get(content))
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
            Ok(())
        })
    }
}

#[derive(Clone)]
struct BusApiState {
    bus: Arc<OnceLock<Arc<host_api::EventBus>>>,
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

async fn content(State(api): State<BusApiState>) -> Response {
    let Some(bus) = api.bus.get() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "dashboard bus unavailable").into_response();
    };
    let snapshot = bus
        .read_retained::<RunSnapshot>(RUN_SNAPSHOT_TOPIC)
        .unwrap_or_else(|_| RunSnapshot::empty());
    match view::ContentTemplate::from_snapshot(snapshot).render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            tracing::error!("dashboard render failed: {e}");
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
