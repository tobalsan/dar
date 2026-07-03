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

use host_api::ServiceRegistry;

use crate::schedule::compute_next_run_at_ms;
use crate::service::{run_job_now, RunNowOutcome, SchedulerConfig};
use crate::state::SchedulerState;
use crate::store::{generate_job_id, save_jobs, Payload, Schedule, ScheduleJob};

/// State shared with every handler.
#[derive(Clone)]
pub struct ApiState {
    pub state: Arc<SchedulerState>,
    pub root: std::path::PathBuf,
    /// Static runner config + typed services needed to fire a job on `run-now`.
    /// Built in `register` alongside the timer loop's config.
    pub config: SchedulerConfig,
    pub services: ServiceRegistry,
}

/// Build the scheduler router. Mounted at namespace `/scheduler`, so routes here
/// are relative (`/jobs`, `/jobs/{id}`).
pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/jobs", get(list_jobs).post(create_job))
        .route(
            "/jobs/{id}",
            axum::routing::patch(update_job).delete(delete_job),
        )
        .route("/jobs/{id}/run-now", axum::routing::post(run_now))
        .route("/jobs/{id}/tail", get(tail))
        .with_state(state)
}

/// Route paths claimed in the host registry (namespace-relative).
pub fn routes() -> Vec<String> {
    vec![
        "/jobs".to_string(),
        "/jobs/{id}".to_string(),
        "/jobs/{id}/run-now".to_string(),
        "/jobs/{id}/tail".to_string(),
    ]
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
async fn create_job(
    State(api): State<ApiState>,
    body: Result<Json<CreateBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
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

    persist_and_respond(&api, jobs, &job, StatusCode::CREATED)
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

    persist_and_respond(&api, jobs, &job, StatusCode::OK)
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

/// `POST /scheduler/jobs/{id}/run-now` — fire a job immediately without
/// disturbing its schedule. Maps the run outcome to HTTP (aihub parity):
/// - `ok` result       → `200` with the run result + output path
/// - `skipped` result  → `202` with the run result
/// - `error`/`inactive` result → `500` with the run result
/// - already running   → `409`
/// - unknown job        → `404`
///
/// The schedule's next fire is preserved across the manual run, except when a
/// scheduled fire was overlap-skipped during it (see [`run_job_now`]).
async fn run_now(State(api): State<ApiState>, Path(id): Path<String>) -> Response {
    match run_job_now(&api.config, &api.services, &api.state, &id).await {
        RunNowOutcome::Unknown => not_found(&id),
        RunNowOutcome::Conflict => json_ok(
            StatusCode::CONFLICT,
            json!({ "error": format!("job {id:?} is already running") }),
        ),
        // A disabled job is not fired: an "inactive" result, surfaced as 500.
        RunNowOutcome::Disabled => json_ok(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({
                "status": "inactive",
                "error": format!("job {id:?} is disabled"),
                "job": Value::Null,
            }),
        ),
        // The fire was skipped (job vanished/disabled mid-claim) → 202.
        RunNowOutcome::Skipped => json_ok(
            StatusCode::ACCEPTED,
            json!({
                "status": "skipped",
                "error": format!("job {id:?} was skipped before firing"),
                "job": Value::Null,
            }),
        ),
        RunNowOutcome::Ran(result) => {
            let now_ms = Utc::now().timestamp_millis();
            let body = run_result_json(&api, &id, &result, now_ms);
            let status = match result.status {
                crate::output::RunStatus::Ok => StatusCode::OK,
                crate::output::RunStatus::Error => StatusCode::INTERNAL_SERVER_ERROR,
            };
            json_ok(status, body)
        }
    }
}

/// Build the JSON body for a completed run-now: run status, timestamps, output
/// path, error (if any), plus the job merged with its post-run runtime state.
fn run_result_json(
    api: &ApiState,
    job_id: &str,
    result: &crate::service::ExecuteResult,
    now_ms: i64,
) -> Value {
    let status = match result.status {
        crate::output::RunStatus::Ok => "ok",
        crate::output::RunStatus::Error => "error",
    };
    let job = api.state.jobs().into_iter().find(|j| j.id == job_id);
    let job_value = job
        .map(|j| job_with_runtime(api, &j, now_ms))
        .unwrap_or_else(|| json!(null));
    json!({
        "status": status,
        "firedAt": result.fired_at.to_rfc3339(),
        "finishedAt": result.finished_at.to_rfc3339(),
        "outputPath": result.output_path.as_ref().map(|p| p.display().to_string()),
        "error": result.error,
        "job": job_value,
    })
}

/// `GET /scheduler/jobs/{id}/tail` — return the newest output file for a job
/// (path + content). The newest file is selected by timestamp stem plus
/// collision suffix (`-0001` beats unsuffixed for same timestamp). Unknown job
/// → `404`; job with no outputs → `404`.
async fn tail(State(api): State<ApiState>, Path(id): Path<String>) -> Response {
    if !api.state.jobs().iter().any(|j| j.id == id) {
        return not_found(&id);
    }
    let dir = api.root.join("cron").join("output").join(&id);
    let newest = std::fs::read_dir(&dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            output_sort_key(&name).map(|key| (key, e))
        })
        .max_by_key(|(key, _)| key.clone())
        .map(|(_, e)| e);
    let Some(entry) = newest else {
        return json_ok(
            StatusCode::NOT_FOUND,
            json!({ "error": format!("no outputs for job {id:?}") }),
        );
    };
    let path = entry.path();
    match std::fs::read_to_string(&path) {
        Ok(content) => json_ok(
            StatusCode::OK,
            json!({ "path": path.display().to_string(), "content": content }),
        ),
        Err(e) => server_error(&format!("reading output {}: {e}", path.display())),
    }
}

fn output_sort_key(name: &str) -> Option<(String, u32)> {
    let stem = name.strip_suffix(".md")?;
    let (ts, seq) = match stem.rsplit_once('-') {
        Some((prefix, suffix))
            if suffix.len() == 4 && suffix.chars().all(|c| c.is_ascii_digit()) =>
        {
            (prefix, suffix.parse().ok()?)
        }
        _ => (stem, 0),
    };
    Some((ts.to_string(), seq))
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

/// Merge a job's config with its in-memory runtime state into a JSON object.
fn job_with_runtime(api: &ApiState, job: &ScheduleJob, now_ms: i64) -> Value {
    let rt = api.state.runtime(&job.id);
    let running_for_ms = rt.running_since_ms.map(|since| (now_ms - since).max(0));
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

/// Persist `jobs` to disk, update shared state, and return the HTTP response
/// for `job` at `status` (201 for create, 200 for update).
fn persist_and_respond(
    api: &ApiState,
    jobs: Vec<ScheduleJob>,
    job: &ScheduleJob,
    status: StatusCode,
) -> Response {
    if let Err(e) = save_jobs(&api.root, &jobs) {
        return server_error(&format!("persisting jobs.json: {e}"));
    }
    api.state.set_jobs(jobs);
    let now_ms = Utc::now().timestamp_millis();
    json_ok(status, job_with_runtime(api, job, now_ms))
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
                config: test_config(dir.path().to_path_buf()),
                services: ServiceRegistry::default(),
            },
            dir,
        )
    }

    fn test_config(root: std::path::PathBuf) -> SchedulerConfig {
        SchedulerConfig {
            root,
            runner_kind: "fake".to_string(),
            runner_command: String::new(),
            runner_model: None,
            runner_provider: None,
            max_run_timeout_ms: 3_600_000,
            poll_interval_ms: 2_000,
            job_timeout_ms: 60_000,
        }
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
        let resp = create_job(
            State(api.clone()),
            Ok(create_body("0 8 * * *", "UTC", "hi")),
        )
        .await;
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
        create_job(
            State(api.clone()),
            Ok(create_body("0 8 * * *", "UTC", "hi")),
        )
        .await;
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
            create_job(
                State(api.clone()),
                Ok(create_body("0 8 * * *", "UTC", "hi")),
            )
            .await,
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
            create_job(
                State(api.clone()),
                Ok(create_body("0 8 * * *", "UTC", "hi")),
            )
            .await,
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
            create_job(
                State(api.clone()),
                Ok(create_body("0 8 * * *", "UTC", "hi")),
            )
            .await,
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
        assert!(crate::store::is_safe_job_id(&id));
        assert_ne!(id, "job-1");
    }

    // ---- run-now & tail ---------------------------------------------------

    use cap_runner::{ExitKind, KillReason, Runner, RunnerHandle, SpawnParams};
    use std::sync::atomic::{AtomicBool, Ordering};

    /// In-test runner that exits normally after a short sleep, optionally with an
    /// abnormal exit to drive an error run.
    struct FakeRunner {
        abnormal: bool,
    }

    impl Runner for FakeRunner {
        fn spawn<'a>(
            &self,
            _params: SpawnParams<'a>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<RunnerHandle>> + Send + 'a>,
        > {
            let abnormal = self.abnormal;
            Box::pin(async move {
                let (kill_tx, _kill_rx) = tokio::sync::oneshot::channel::<KillReason>();
                let done = tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    if abnormal {
                        ExitKind::Abnormal(Some(1))
                    } else {
                        ExitKind::Normal
                    }
                });
                Ok(RunnerHandle::new(0, kill_tx, done))
            })
        }
    }

    fn api_with_runner(abnormal: bool) -> (ApiState, tempfile::TempDir) {
        let (mut api, dir) = api();
        let mut services = ServiceRegistry::default();
        services
            .service::<dyn Runner>("fake", Arc::new(FakeRunner { abnormal }))
            .unwrap();
        api.services = services;
        (api, dir)
    }

    async fn create_one(api: &ApiState) -> String {
        let created = body_json(
            create_job(
                State(api.clone()),
                Ok(create_body("0 0 1 1 *", "UTC", "hi")),
            )
            .await,
        )
        .await;
        created["id"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn run_now_unknown_job_is_404() {
        let (api, _dir) = api();
        let resp = run_now(State(api), Path("nope".to_string())).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn run_now_already_running_is_409() {
        let (api, _dir) = api();
        let id = create_one(&api).await;
        assert!(api
            .state
            .try_claim_running(&id, Utc::now().timestamp_millis()));
        let resp = run_now(State(api), Path(id)).await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn run_now_disabled_job_is_500_inactive() {
        let (api, _dir) = api();
        let id = create_one(&api).await;
        let body = Json(UpdateBody {
            name: None,
            enabled: Some(false),
            schedule: None,
            payload: None,
            timeout_ms: None,
        });
        update_job(State(api.clone()), Path(id.clone()), Ok(body)).await;
        let resp = run_now(State(api), Path(id)).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let v = body_json(resp).await;
        assert_eq!(v["status"], "inactive");
    }

    #[tokio::test]
    async fn run_now_ok_returns_200_and_writes_output() {
        let (api, dir) = api_with_runner(false);
        let id = create_one(&api).await;
        let resp = run_now(State(api.clone()), Path(id.clone())).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["status"], "ok");
        let path = v["outputPath"].as_str().unwrap();
        assert!(std::path::Path::new(path).exists(), "output written");
        // Output landed under cron/output/<id>/.
        assert!(dir
            .path()
            .join("cron/output")
            .join(&id)
            .read_dir()
            .unwrap()
            .next()
            .is_some());
    }

    #[tokio::test]
    async fn run_now_error_result_is_500() {
        let (api, _dir) = api_with_runner(true);
        let id = create_one(&api).await;
        let resp = run_now(State(api), Path(id)).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let v = body_json(resp).await;
        assert_eq!(v["status"], "error");
        assert!(v["error"].is_string());
    }

    #[tokio::test]
    async fn run_now_preserves_next_fire() {
        let (api, _dir) = api_with_runner(false);
        let id = create_one(&api).await;
        // Arm a next fire as the loop would.
        let next = Utc::now().timestamp_millis() + 3_600_000;
        api.state.set_next_run(&id, Some(next));
        run_now(State(api.clone()), Path(id.clone())).await;
        assert_eq!(
            api.state.runtime(&id).next_run_at_ms,
            Some(next),
            "pre-run next fire restored"
        );
    }

    #[tokio::test]
    async fn run_now_keeps_recomputed_next_fire_when_scheduled_fire_skipped() {
        let (api, _dir) = api_with_runner(false);
        let id = create_one(&api).await;
        let pre = Utc::now().timestamp_millis() + 3_600_000;
        api.state.set_next_run(&id, Some(pre));
        // Simulate the loop overlap-skipping a scheduled fire and recomputing the
        // next fire during the manual run: a runner whose spawn marks the skip.
        let marker = Arc::new(AtomicBool::new(false));
        let recomputed = Utc::now().timestamp_millis() + 7_200_000;
        struct SkippingRunner {
            state: Arc<SchedulerState>,
            id: String,
            recomputed: i64,
            done: Arc<AtomicBool>,
        }
        impl Runner for SkippingRunner {
            fn spawn<'a>(
                &self,
                _params: SpawnParams<'a>,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = anyhow::Result<RunnerHandle>> + Send + 'a>,
            > {
                // Emulate the loop's overlap-skip bookkeeping landing mid-run.
                self.state
                    .mark_skipped(&self.id, Utc::now().timestamp_millis());
                self.state.set_next_run(&self.id, Some(self.recomputed));
                self.done.store(true, Ordering::SeqCst);
                Box::pin(async move {
                    let (kill_tx, _rx) = tokio::sync::oneshot::channel::<KillReason>();
                    let done = tokio::spawn(async move { ExitKind::Normal });
                    Ok(RunnerHandle::new(0, kill_tx, done))
                })
            }
        }
        let mut services = ServiceRegistry::default();
        services
            .service::<dyn Runner>(
                "fake",
                Arc::new(SkippingRunner {
                    state: Arc::clone(&api.state),
                    id: id.clone(),
                    recomputed,
                    done: Arc::clone(&marker),
                }),
            )
            .unwrap();
        let mut api = api;
        api.services = services;

        run_now(State(api.clone()), Path(id.clone())).await;
        assert!(marker.load(Ordering::SeqCst), "skip path exercised");
        assert_eq!(
            api.state.runtime(&id).next_run_at_ms,
            Some(recomputed),
            "recomputed next fire kept when a scheduled fire was skipped"
        );
    }

    #[tokio::test]
    async fn tail_unknown_job_is_404() {
        let (api, _dir) = api();
        let resp = tail(State(api), Path("nope".to_string())).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn tail_no_outputs_is_404() {
        let (api, _dir) = api();
        let id = create_one(&api).await;
        let resp = tail(State(api), Path(id)).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn tail_returns_newest_output() {
        let (api, dir) = api();
        let id = create_one(&api).await;
        let out_dir = dir.path().join("cron/output").join(&id);
        std::fs::create_dir_all(&out_dir).unwrap();
        std::fs::write(out_dir.join("2026-01-01_00-00-00.md"), "old").unwrap();
        std::fs::write(out_dir.join("2026-06-01_12-00-00.md"), "newest").unwrap();
        std::fs::write(out_dir.join("2026-03-01_00-00-00.md"), "mid").unwrap();
        let resp = tail(State(api), Path(id)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["content"], "newest");
        assert!(v["path"]
            .as_str()
            .unwrap()
            .ends_with("2026-06-01_12-00-00.md"));
    }

    #[tokio::test]
    async fn tail_returns_newest_collision_output() {
        let (api, dir) = api();
        let id = create_one(&api).await;
        let out_dir = dir.path().join("cron/output").join(&id);
        std::fs::create_dir_all(&out_dir).unwrap();
        std::fs::write(out_dir.join("2026-06-01_12-00-00.000.md"), "first").unwrap();
        std::fs::write(out_dir.join("2026-06-01_12-00-00.000-0001.md"), "second").unwrap();
        let resp = tail(State(api), Path(id)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["content"], "second");
        assert!(v["path"]
            .as_str()
            .unwrap()
            .ends_with("2026-06-01_12-00-00.000-0001.md"));
    }
}
