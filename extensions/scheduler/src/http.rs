//! Scheduler HTTP API: single-agent job CRUD mounted under `/scheduler`.
//!
//! Unlike the aihub multi-agent API there are no `agentId` path segments \u2014 the
//! agent is implied by the host process. Mutations validate the body, persist
//! the new job set atomically to `cron/jobs.json` ([`crate::store::save_jobs`]),
//! and wake the timer loop via shared [`SchedulerState`] so a sooner schedule
//! takes effect immediately.
//!
//! Endpoints (namespace `/scheduler`):
//! - `GET    /scheduler/jobs`        \u2192 list jobs with runtime state
//! - `POST   /scheduler/jobs`        \u2192 create (201, server-generated id)
//! - `PATCH  /scheduler/jobs/{id}`   \u2192 update name/enabled/schedule/payload/timeout
//! - `DELETE /scheduler/jobs/{id}`   \u2192 delete (204)

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::schedule::compute_next_run_at_ms;
use crate::state::SchedulerState;
use crate::store::{is_safe_job_id, save_jobs, Payload, Schedule, ScheduleJob};

/// State shared with every handler.
#[derive(Clone)]
pub struct ApiState {
    pub state: Arc<SchedulerState>,
    pub root: std::path::PathBuf,
}

/// Build the scheduler router. Mounted at namespace `/scheduler`, so routes here
/// are relative (`/jobs`, `/jobs/{id}`).
pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/jobs", get(list_jobs).post(create_job))
        .route("/jobs/{id}", axum::routing::patch(update_job).delete(delete_job))
        .with_state(state)
}

/// Route paths claimed in the host registry (namespace-relative).
pub fn routes() -> Vec<String> {
    vec!["/jobs".to_string(), "/jobs/{id}".to_string()]
}

// ---- request bodies -------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ScheduleBody {
    cron: Option<String>,
    tz: Option<String>,
    #[serde(rename = "startAt")]
    start_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PayloadBody {
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CreateBody {
    name: Option<String>,
    enabled: Option<bool>,
    schedule: Option<ScheduleBody>,
    payload: Option<PayloadBody>,
    #[serde(rename = "timeoutMs")]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct UpdateBody {
    name: Option<String>,
    enabled: Option<bool>,
    schedule: Option<ScheduleBody>,
    payload: Option<PayloadBody>,
    #[serde(rename = "timeoutMs")]
    timeout_ms: Option<Option<u64>>,
}

// ---- handlers -------------------------------------------------------------

/// `GET /scheduler/jobs` \u2014 jobs merged with in-memory runtime state.
async fn list_jobs(State(api): State<ApiState>) -> Response {
    let now_ms = Utc::now().timestamp_millis();
    let jobs = api.state.jobs();
    let out: Vec<Value> = jobs
        .iter()
        .map(|job| job_with_runtime(&api, job, now_ms))
        .collect();
    json_ok(StatusCode::OK, json!({ "jobs": out }))
}

/// `POST /scheduler/jobs` \u2014 create with a server-generated id, default
/// `enabled: true`. Returns 201 with the new job (config + runtime).
async fn create_job(State(api): State<ApiState>, body: Result<Json<CreateBody>, axum::extract::rejection::JsonRejection>) -> Response {
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => return bad_request(&e.body_text()),
    };

    let schedule = match validate_schedule(body.schedule) {
        Ok(s) => s,
        Err(msg) => return bad_request(&msg),
    };
    let payload = match validate_payload(body.payload) {
        Ok(p) => p,
        Err(msg) => return bad_request(&msg),
    };

    let mut jobs = api.state.jobs();
    let id = generate_job_id(&jobs);
    let job = ScheduleJob {
        id,
        name: body.name.unwrap_or_default(),
        enabled: body.enabled.unwrap_or(true),
        schedule,
        payload,
        timeout_ms: body.timeout_ms,
    };
    jobs.push(job.clone());

    if let Err(e) = save_jobs(&api.root, &jobs) {
        return server_error(&format!("persisting jobs.json: {e}"));
    }
    api.state.set_jobs(jobs);

    let now_ms = Utc::now().timestamp_millis();
    json_ok(StatusCode::CREATED, job_with_runtime(&api, &job, now_ms))
}

/// `PATCH /scheduler/jobs/{id}` \u2014 patch name/enabled/schedule/payload/timeout.
/// Recomputing the next fire happens in the loop after the re-arm wake.
async fn update_job(
    State(api): State<ApiState>,
    Path(id): Path<String>,
    body: Result<Json<UpdateBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => return bad_request(&e.body_text()),
    };

    let mut jobs = api.state.jobs();
    let Some(idx) = jobs.iter().position(|j| j.id == id) else {
        return not_found(&id);
    };

    let mut job = jobs[idx].clone();
    if let Some(name) = body.name {
        job.name = name;
    }
    if let Some(enabled) = body.enabled {
        job.enabled = enabled;
    }
    if let Some(schedule) = body.schedule {
        job.schedule = match validate_schedule(Some(schedule)) {
            Ok(s) => s,
            Err(msg) => return bad_request(&msg),
        };
    }
    if let Some(payload) = body.payload {
        job.payload = match validate_payload(Some(payload)) {
            Ok(p) => p,
            Err(msg) => return bad_request(&msg),
        };
    }
    if let Some(timeout_ms) = body.timeout_ms {
        job.timeout_ms = timeout_ms;
    }

    jobs[idx] = job.clone();
    if let Err(e) = save_jobs(&api.root, &jobs) {
        return server_error(&format!("persisting jobs.json: {e}"));
    }
    api.state.set_jobs(jobs);

    let now_ms = Utc::now().timestamp_millis();
    json_ok(StatusCode::OK, job_with_runtime(&api, &job, now_ms))
}

/// `DELETE /scheduler/jobs/{id}` \u2014 remove the job. 204 on success, 404 unknown.
async fn delete_job(State(api): State<ApiState>, Path(id): Path<String>) -> Response {
    let mut jobs = api.state.jobs();
    let before = jobs.len();
    jobs.retain(|j| j.id != id);
    if jobs.len() == before {
        return not_found(&id);
    }
    if let Err(e) = save_jobs(&api.root, &jobs) {
        return server_error(&format!("persisting jobs.json: {e}"));
    }
    api.state.set_jobs(jobs);
    StatusCode::NO_CONTENT.into_response()
}

// ---- validation -----------------------------------------------------------

/// Validate a schedule body: cron parses, tz is a real IANA zone, startAt (if
/// present) is RFC3339. Reuses [`compute_next_run_at_ms`] so the same cron/tz
/// the loop will use is what we accept.
fn validate_schedule(body: Option<ScheduleBody>) -> Result<Schedule, String> {
    let body = body.ok_or("schedule is required")?;
    let cron = body.cron.unwrap_or_default();
    if cron.trim().is_empty() {
        return Err("schedule.cron is required".to_string());
    }
    let tz = body.tz.unwrap_or_default();
    if tz.trim().is_empty() {
        return Err("schedule.tz is required".to_string());
    }
    let schedule = Schedule {
        cron,
        tz,
        start_at: body.start_at,
    };
    // A successful next-fire computation validates cron, tz, and startAt at once.
    compute_next_run_at_ms(&schedule, Utc::now().timestamp_millis())
        .map_err(|e| format!("invalid schedule: {e:#}"))?;
    Ok(schedule)
}

/// Validate a payload body: a non-empty `message`.
fn validate_payload(body: Option<PayloadBody>) -> Result<Payload, String> {
    let body = body.ok_or("payload is required")?;
    let message = body.message.unwrap_or_default();
    if message.trim().is_empty() {
        return Err("payload.message is required".to_string());
    }
    Ok(Payload { message })
}

// ---- helpers --------------------------------------------------------------

/// Generate a unique, filesystem-safe job id not colliding with existing ones.
fn generate_job_id(existing: &[ScheduleJob]) -> String {
    let base = Utc::now().timestamp_millis();
    let mut n = base;
    loop {
        let candidate = format!("job-{n}");
        if is_safe_job_id(&candidate) && !existing.iter().any(|j| j.id == candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Merge a job's config with its in-memory runtime state into a JSON object.
fn job_with_runtime(api: &ApiState, job: &ScheduleJob, now_ms: i64) -> Value {
    let rt = api.state.runtime(&job.id);
    let running_for_ms = rt
        .running_since_ms
        .map(|since| (now_ms - since).max(0));
    let mut value = serde_json::to_value(job).unwrap_or_else(|_| json!({}));
    if let Value::Object(map) = &mut value {
        map.insert("nextRunAtMs".to_string(), json!(rt.next_run_at_ms));
        map.insert("lastRunAtMs".to_string(), json!(rt.last_run_at_ms));
        map.insert(
            "lastStatus".to_string(),
            json!(rt.last_status.map(|s| s.as_str())),
        );
        map.insert("lastError".to_string(), json!(rt.last_error));
        map.insert("runningForMs".to_string(), json!(running_for_ms));
    }
    value
}

fn json_ok(status: StatusCode, body: Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

fn bad_request(message: &str) -> Response {
    json_ok(StatusCode::BAD_REQUEST, json!({ "error": message }))
}

fn not_found(id: &str) -> Response {
    json_ok(
        StatusCode::NOT_FOUND,
        json!({ "error": format!("unknown job id {id:?}") }),
    )
}

fn server_error(message: &str) -> Response {
    json_ok(
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({ "error": message }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    fn api() -> (ApiState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(SchedulerState::new(vec![]));
        (
            ApiState {
                state,
                root: dir.path().to_path_buf(),
            },
            dir,
        )
    }

    async fn body_json(resp: Response) -> Value {
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn create_body(cron: &str, tz: &str, message: &str) -> Json<CreateBody> {
        Json(CreateBody {
            name: Some("Test".to_string()),
            enabled: None,
            schedule: Some(ScheduleBody {
                cron: Some(cron.to_string()),
                tz: Some(tz.to_string()),
                start_at: None,
            }),
            payload: Some(PayloadBody {
                message: Some(message.to_string()),
            }),
            timeout_ms: None,
        })
    }

    #[tokio::test]
    async fn create_returns_201_with_id_and_defaults_enabled() {
        let (api, _dir) = api();
        let resp = create_job(State(api.clone()), Ok(create_body("0 8 * * *", "UTC", "hi"))).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let v = body_json(resp).await;
        assert!(v["id"].as_str().unwrap().starts_with("job-"));
        assert_eq!(v["enabled"], true);
        assert_eq!(v["name"], "Test");
        // Persisted to disk.
        assert!(api.root.join("cron").join("jobs.json").exists());
        assert_eq!(api.state.jobs().len(), 1);
    }

    #[tokio::test]
    async fn create_bad_cron_is_400() {
        let (api, _dir) = api();
        let resp = create_job(State(api), Ok(create_body("not a cron", "UTC", "hi"))).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let v = body_json(resp).await;
        assert!(v["error"].as_str().unwrap().contains("invalid schedule"));
    }

    #[tokio::test]
    async fn create_missing_tz_is_400() {
        let (api, _dir) = api();
        let body = Json(CreateBody {
            name: None,
            enabled: None,
            schedule: Some(ScheduleBody {
                cron: Some("0 8 * * *".to_string()),
                tz: None,
                start_at: None,
            }),
            payload: Some(PayloadBody {
                message: Some("hi".to_string()),
            }),
            timeout_ms: None,
        });
        let resp = create_job(State(api), Ok(body)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let v = body_json(resp).await;
        assert!(v["error"].as_str().unwrap().contains("tz is required"));
    }

    #[tokio::test]
    async fn create_empty_message_is_400() {
        let (api, _dir) = api();
        let resp = create_job(State(api), Ok(create_body("0 8 * * *", "UTC", "   "))).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let v = body_json(resp).await;
        assert!(v["error"].as_str().unwrap().contains("message is required"));
    }

    #[tokio::test]
    async fn list_includes_runtime_fields() {
        let (api, _dir) = api();
        create_job(State(api.clone()), Ok(create_body("0 8 * * *", "UTC", "hi"))).await;
        let resp = list_jobs(State(api)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let job = &v["jobs"][0];
        for key in ["nextRunAtMs", "lastRunAtMs", "lastStatus", "runningForMs"] {
            assert!(job.get(key).is_some(), "missing runtime key {key}");
        }
    }

    #[tokio::test]
    async fn update_patches_fields() {
        let (api, _dir) = api();
        let created = body_json(
            create_job(State(api.clone()), Ok(create_body("0 8 * * *", "UTC", "hi"))).await,
        )
        .await;
        let id = created["id"].as_str().unwrap().to_string();
        let body = Json(UpdateBody {
            name: Some("Renamed".to_string()),
            enabled: Some(false),
            schedule: None,
            payload: None,
            timeout_ms: Some(Some(5000)),
        });
        let resp = update_job(State(api.clone()), Path(id.clone()), Ok(body)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["name"], "Renamed");
        assert_eq!(v["enabled"], false);
        assert_eq!(v["timeoutMs"], 5000);
    }

    #[tokio::test]
    async fn update_unknown_id_is_404() {
        let (api, _dir) = api();
        let body = Json(UpdateBody {
            name: Some("x".to_string()),
            enabled: None,
            schedule: None,
            payload: None,
            timeout_ms: None,
        });
        let resp = update_job(State(api), Path("nope".to_string()), Ok(body)).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn update_bad_schedule_is_400() {
        let (api, _dir) = api();
        let created = body_json(
            create_job(State(api.clone()), Ok(create_body("0 8 * * *", "UTC", "hi"))).await,
        )
        .await;
        let id = created["id"].as_str().unwrap().to_string();
        let body = Json(UpdateBody {
            name: None,
            enabled: None,
            schedule: Some(ScheduleBody {
                cron: Some("0 8 * * *".to_string()),
                tz: Some("Not/AZone".to_string()),
                start_at: None,
            }),
            payload: None,
            timeout_ms: None,
        });
        let resp = update_job(State(api), Path(id), Ok(body)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn delete_removes_and_returns_204() {
        let (api, _dir) = api();
        let created = body_json(
            create_job(State(api.clone()), Ok(create_body("0 8 * * *", "UTC", "hi"))).await,
        )
        .await;
        let id = created["id"].as_str().unwrap().to_string();
        let resp = delete_job(State(api.clone()), Path(id.clone())).await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert!(api.state.jobs().is_empty());
        // Idempotent-ish: second delete is 404.
        let resp = delete_job(State(api), Path(id)).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn delete_unknown_id_is_404() {
        let (api, _dir) = api();
        let resp = delete_job(State(api), Path("nope".to_string())).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn generate_job_id_is_unique() {
        let existing = vec![ScheduleJob {
            id: "job-1".to_string(),
            name: String::new(),
            enabled: true,
            schedule: Schedule {
                cron: "* * * * *".to_string(),
                tz: "UTC".to_string(),
                start_at: None,
            },
            payload: Payload {
                message: "x".to_string(),
            },
            timeout_ms: None,
        }];
        let id = generate_job_id(&existing);
        assert!(is_safe_job_id(&id));
        assert_ne!(id, "job-1");
    }
}
