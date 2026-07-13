//! Model-facing scheduler management tools (ALG-273).
//!
//! These wrap the *same* store / validation / runtime-state / execution paths
//! the HTTP API uses (`crate::store`, `crate::state`, `crate::service`,
//! `crate::schedule`), exposing them through the shared host [`ToolRegistry`] so
//! every tool-capable runner (Pi, Codex, OpenCode) and every cap-chat backend
//! can discover and call them. There is no second scheduler model: a tool call
//! and an HTTP call mutate the same `cron/jobs.json` and the same in-memory
//! [`SchedulerState`], and validate cron / timezone / `startAt` / job id with
//! the same code.
//!
//! Registration happens only on the scheduler's *enabled* path
//! (`crate::SchedulerExtension::register`), so the tools are discoverable only
//! when the scheduler extension is enabled. A disabled or absent scheduler
//! registers nothing, so the model simply does not see the tools (and any call
//! routed to them returns an `unknown_tool` error from the registry) rather than
//! inventing scheduler state.
//!
//! Autonomy / confirmation contract:
//!   - `list`, `get`, `status`, `tail` are read-only.
//!   - `create`, `update`, `enable`, `run_now` are autonomous (non-destructive):
//!     they preserve existing jobs.
//!   - `disable` and `delete` are destructive and require `confirm: true`;
//!     without it they reject with a structured `confirmation_required` error
//!     and make no change.
//!
//! Every outcome is structured (`ToolOutcome`): a validation failure, an unknown
//! job, or a missing confirmation is `is_error: true` data returned to the model
//! so the run continues. Create / update / status return next-fire information
//! so the model can explain outcomes and catch schedule mistakes.

use std::sync::Arc;

use anyhow::Result;
use chrono::{SecondsFormat, TimeZone, Utc};
use serde_json::{json, Value};

use host_api::ServiceRegistry;
use tool_registry::{ToolContent, ToolExecutor, ToolOutcome, ToolRegistryHandle, ToolSpec};

use crate::schedule::{compute_next_run_at_ms, format_schedule};
use crate::service::{run_job_now, RunNowOutcome, SchedulerConfig};
use crate::state::SchedulerState;
use crate::store::{generate_job_id, is_safe_job_id, save_jobs, Payload, Schedule, ScheduleJob};

/// Shared dependencies every scheduler tool executor needs. Mirrors the HTTP
/// `ApiState` so tools and HTTP share one contract.
#[derive(Clone)]
pub struct ToolDeps {
    pub state: Arc<SchedulerState>,
    pub root: std::path::PathBuf,
    pub config: SchedulerConfig,
    pub services: ServiceRegistry,
}

/// Register all scheduler management tools against the shared registry. Called
/// from the extension's `register()` pass on the enabled path only.
pub fn register_into(registry: &dyn ToolRegistryHandle, deps: ToolDeps) -> Result<()> {
    registry.register_tool(list_spec(), Arc::new(ListTool { deps: deps.clone() }))?;
    registry.register_tool(get_spec(), Arc::new(GetTool { deps: deps.clone() }))?;
    registry.register_tool(status_spec(), Arc::new(StatusTool { deps: deps.clone() }))?;
    registry.register_tool(tail_spec(), Arc::new(TailTool { deps: deps.clone() }))?;
    registry.register_tool(run_now_spec(), Arc::new(RunNowTool { deps: deps.clone() }))?;
    registry.register_tool(create_spec(), Arc::new(CreateTool { deps: deps.clone() }))?;
    registry.register_tool(update_spec(), Arc::new(UpdateTool { deps: deps.clone() }))?;
    registry.register_tool(
        enable_spec(),
        Arc::new(SetEnabledTool {
            deps: deps.clone(),
            enable: true,
        }),
    )?;
    registry.register_tool(
        disable_spec(),
        Arc::new(SetEnabledTool {
            deps: deps.clone(),
            enable: false,
        }),
    )?;
    registry.register_tool(delete_spec(), Arc::new(DeleteTool { deps }))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Render a UTC epoch-millis instant as an RFC3339 string, for model-readable
/// next/last-fire fields alongside the raw millis.
fn iso(ms: i64) -> Option<String> {
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Millis, true))
}

/// Merge a job's config with its runtime state (next/last run, status, error,
/// running-for) into a JSON object, adding RFC3339 mirrors and separate
/// computed next-fire diagnostics so the model can explain schedules without
/// converting timestamps itself.
fn job_view(deps: &ToolDeps, job: &ScheduleJob, now_ms: i64) -> Value {
    let rt = deps.state.runtime(&job.id);
    let running_for_ms = rt.running_since_ms.map(|since| (now_ms - since).max(0));
    let computed_next_run_at_ms = compute_next_run_at_ms(&job.schedule, now_ms).ok();
    let mut value = serde_json::to_value(job).unwrap_or_else(|_| json!({}));
    if let Value::Object(map) = &mut value {
        map.insert("nextRunAtMs".to_string(), json!(rt.next_run_at_ms));
        map.insert(
            "nextRunAt".to_string(),
            json!(rt.next_run_at_ms.and_then(iso)),
        );
        map.insert(
            "computedNextRunAtMs".to_string(),
            json!(computed_next_run_at_ms),
        );
        map.insert(
            "computedNextRunAt".to_string(),
            json!(computed_next_run_at_ms.and_then(iso)),
        );
        map.insert("lastRunAtMs".to_string(), json!(rt.last_run_at_ms));
        map.insert(
            "lastRunAt".to_string(),
            json!(rt.last_run_at_ms.and_then(iso)),
        );
        map.insert(
            "lastStatus".to_string(),
            json!(rt.last_status.map(|s| s.as_str())),
        );
        map.insert("lastError".to_string(), json!(rt.last_error));
        map.insert("runningForMs".to_string(), json!(running_for_ms));
        map.insert(
            "scheduleHuman".to_string(),
            json!(format_schedule(&job.schedule)),
        );
    }
    value
}

/// Serialize a JSON value to a compact string for a tool's `text` payload.
fn render(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string())
}

/// Look up a job id argument: required, non-empty, filesystem-safe. Returns the
/// structured error outcome on a bad argument.
fn require_job_id(args: &Value) -> Result<String, Box<ToolOutcome>> {
    let Some(id) = args.get("id").and_then(Value::as_str) else {
        return Err(Box::new(ToolOutcome::error_code(
            "invalid_args",
            "missing required 'id' string argument",
            None::<String>,
        )));
    };
    let id = id.trim();
    if id.is_empty() {
        return Err(Box::new(ToolOutcome::error_code(
            "invalid_args",
            "'id' must not be empty",
            None::<String>,
        )));
    }
    if !is_safe_job_id(id) {
        return Err(Box::new(ToolOutcome::error_code(
            "invalid_args",
            format!("unsafe job id {id:?}: ids must be a single path component (no '/', '\\', '..', or leading '.')"),
            None::<String>,
        )));
    }
    Ok(id.to_string())
}

/// Build a `Schedule` from a tool args `schedule` object and validate cron / tz
/// / startAt via the shared next-fire computation (same code the loop uses).
fn parse_and_validate_schedule(schedule: &Value) -> Result<Schedule, Box<ToolOutcome>> {
    let cron = schedule
        .get("cron")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if cron.trim().is_empty() {
        return Err(Box::new(ToolOutcome::error_code(
            "invalid_args",
            "schedule.cron is required (a raw 5-field cron expression)",
            None::<String>,
        )));
    }
    let tz = schedule
        .get("tz")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if tz.trim().is_empty() {
        return Err(Box::new(ToolOutcome::error_code(
            "invalid_args",
            "schedule.tz is required (an IANA timezone, e.g. \"Europe/Paris\")",
            None::<String>,
        )));
    }
    let start_at = schedule
        .get("startAt")
        .and_then(Value::as_str)
        .map(|s| s.to_string());
    let schedule = Schedule { cron, tz, start_at };
    match compute_next_run_at_ms(&schedule, Utc::now().timestamp_millis()) {
        Ok(_) => Ok(schedule),
        Err(err) => Err(Box::new(ToolOutcome::error_code(
            "invalid_schedule",
            format!("invalid schedule: {err:#}"),
            Some("Provide a valid raw cron expression and IANA timezone; startAt (if set) must be RFC3339."),
        ))),
    }
}

/// Build a `Payload` from a tool args `payload` object: a non-empty `message`.
fn parse_and_validate_payload(payload: &Value) -> Result<Payload, Box<ToolOutcome>> {
    let message = payload
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if message.trim().is_empty() {
        return Err(Box::new(ToolOutcome::error_code(
            "invalid_args",
            "payload.message is required (the prompt the job runs)",
            None::<String>,
        )));
    }
    Ok(Payload { message })
}

/// Persist `jobs` to disk and push into shared state (which wakes the timer
/// loop to re-arm). Returns a structured error outcome on a write failure.
fn persist(deps: &ToolDeps, jobs: Vec<ScheduleJob>) -> Result<(), Box<ToolOutcome>> {
    if let Err(e) = save_jobs(&deps.root, &jobs) {
        return Err(Box::new(ToolOutcome::error_code(
            "persist_error",
            format!("failed to persist cron/jobs.json: {e}"),
            None::<String>,
        )));
    }
    deps.state.set_jobs(jobs);
    Ok(())
}

/// Structured `not found` outcome for an unknown job id.
fn unknown_job(id: &str) -> ToolOutcome {
    ToolOutcome::error_code(
        "unknown_job",
        format!("no scheduler job with id {id:?}"),
        Some("Use scheduler_list_jobs to see existing job ids."),
    )
}

/// `confirm: true` gate for a destructive operation.
fn require_confirm(args: &Value, op: &str) -> Result<(), Box<ToolOutcome>> {
    if args.get("confirm").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(Box::new(ToolOutcome::error_code(
            "confirmation_required",
            format!("{op} is destructive and was not performed"),
            Some("Re-call with \"confirm\": true to proceed."),
        )))
    }
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

fn list_spec() -> ToolSpec {
    ToolSpec::new(
        "scheduler_list_jobs",
        "List all scheduler (cron) jobs for this agent with their schedule, \
         enabled flag, prompt, and runtime state (next fire, last run, last \
         status/error, whether running now). Read-only.",
        json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    )
    .reads()
}

struct ListTool {
    deps: ToolDeps,
}

#[async_trait::async_trait]
impl ToolExecutor for ListTool {
    async fn execute(&self, _args: Value) -> Result<ToolOutcome> {
        let now_ms = Utc::now().timestamp_millis();
        let jobs = self.deps.state.jobs();
        let out: Vec<Value> = jobs
            .iter()
            .map(|job| job_view(&self.deps, job, now_ms))
            .collect();
        Ok(ToolOutcome::ok(render(&json!({ "jobs": out }))))
    }
}

// ---------------------------------------------------------------------------
// get
// ---------------------------------------------------------------------------

fn get_spec() -> ToolSpec {
    ToolSpec::new(
        "scheduler_get_job",
        "Get one scheduler job by id, including its schedule, prompt, enabled \
         flag, and runtime state (next fire, last run, last status/error). \
         Read-only.",
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The job id." }
            },
            "required": ["id"],
            "additionalProperties": false,
        }),
    )
    .reads()
}

struct GetTool {
    deps: ToolDeps,
}

#[async_trait::async_trait]
impl ToolExecutor for GetTool {
    async fn execute(&self, args: Value) -> Result<ToolOutcome> {
        let id = match require_job_id(&args) {
            Ok(id) => id,
            Err(out) => return Ok(*out),
        };
        let now_ms = Utc::now().timestamp_millis();
        match self.deps.state.jobs().into_iter().find(|j| j.id == id) {
            Some(job) => Ok(ToolOutcome::ok(render(&job_view(&self.deps, &job, now_ms)))),
            None => Ok(unknown_job(&id)),
        }
    }
}

// ---------------------------------------------------------------------------
// status / next-fire diagnostics
// ---------------------------------------------------------------------------

fn status_spec() -> ToolSpec {
    ToolSpec::new(
        "scheduler_job_status",
        "Report next-fire diagnostics for a scheduler job: its schedule, the \
         computed next fire time (both epoch millis and RFC3339), whether it is \
         enabled and whether it is currently running, plus the last run \
         status/error. Use this to validate a cron expression before it \
         surprises you. Read-only.",
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The job id." }
            },
            "required": ["id"],
            "additionalProperties": false,
        }),
    )
    .reads()
}

struct StatusTool {
    deps: ToolDeps,
}

#[async_trait::async_trait]
impl ToolExecutor for StatusTool {
    async fn execute(&self, args: Value) -> Result<ToolOutcome> {
        let id = match require_job_id(&args) {
            Ok(id) => id,
            Err(out) => return Ok(*out),
        };
        let Some(job) = self.deps.state.jobs().into_iter().find(|j| j.id == id) else {
            return Ok(unknown_job(&id));
        };
        let now_ms = Utc::now().timestamp_millis();
        let rt = self.deps.state.runtime(&id);
        // Recompute the next fire from the schedule so the diagnostic is fresh
        // even for a disabled job (whose armed nextRunAt is cleared).
        let computed_next = compute_next_run_at_ms(&job.schedule, now_ms).ok();
        let body = json!({
            "id": job.id,
            "enabled": job.enabled,
            "running": rt.running_since_ms.is_some(),
            "schedule": job.schedule,
            "scheduleHuman": format_schedule(&job.schedule),
            "nextRunAtMs": computed_next,
            "nextRunAt": computed_next.and_then(iso),
            "armedNextRunAtMs": rt.next_run_at_ms,
            "lastRunAtMs": rt.last_run_at_ms,
            "lastRunAt": rt.last_run_at_ms.and_then(iso),
            "lastStatus": rt.last_status.map(|s| s.as_str()),
            "lastError": rt.last_error,
        });
        Ok(ToolOutcome::ok(render(&body)))
    }
}

// ---------------------------------------------------------------------------
// tail
// ---------------------------------------------------------------------------

fn tail_spec() -> ToolSpec {
    ToolSpec::new(
        "scheduler_tail_output",
        "Return the newest captured output file for a scheduler job (path + \
         content), for debugging recent runs. Returns a clear error when the \
         job has no output yet. Read-only.",
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The job id." }
            },
            "required": ["id"],
            "additionalProperties": false,
        }),
    )
    .reads()
}

struct TailTool {
    deps: ToolDeps,
}

#[async_trait::async_trait]
impl ToolExecutor for TailTool {
    async fn execute(&self, args: Value) -> Result<ToolOutcome> {
        let id = match require_job_id(&args) {
            Ok(id) => id,
            Err(out) => return Ok(*out),
        };
        if !self.deps.state.jobs().iter().any(|j| j.id == id) {
            return Ok(unknown_job(&id));
        }
        let dir = self.deps.root.join("cron").join("output").join(&id);
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
            return Ok(ToolOutcome::error_code(
                "no_output",
                format!("scheduler job {id:?} has no output yet"),
                Some("Run the job with scheduler_run_job_now, or wait for its next fire."),
            ));
        };
        let path = entry.path();
        match std::fs::read_to_string(&path) {
            Ok(content) => Ok(ToolOutcome::ok(render(&json!({
                "path": path.display().to_string(),
                "content": content,
            })))),
            Err(e) => Ok(ToolOutcome::error_code(
                "read_error",
                format!("failed reading output {}: {e}", path.display()),
                None::<String>,
            )),
        }
    }
}

/// Newest-output sort key: timestamp stem plus 4-digit collision suffix, so
/// `…-0001` beats the unsuffixed file for the same timestamp. Mirrors the HTTP
/// `tail` selection exactly.
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

// ---------------------------------------------------------------------------
// run-now
// ---------------------------------------------------------------------------

fn run_now_spec() -> ToolSpec {
    ToolSpec::new(
        "scheduler_run_job_now",
        "Run a scheduler job immediately, on demand, without disturbing its \
         schedule (the next fire is preserved). Uses the same execution and \
         overlap semantics as a scheduled fire: if a run is already in flight \
         the call is rejected. Non-destructive; no confirmation needed.",
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The job id to fire now." }
            },
            "required": ["id"],
            "additionalProperties": false,
        }),
    )
    .with_access(true, true)
}

struct RunNowTool {
    deps: ToolDeps,
}

#[async_trait::async_trait]
impl ToolExecutor for RunNowTool {
    async fn execute(&self, args: Value) -> Result<ToolOutcome> {
        let id = match require_job_id(&args) {
            Ok(id) => id,
            Err(out) => return Ok(*out),
        };
        match run_job_now(&self.deps.config, &self.deps.services, &self.deps.state, &id).await {
            RunNowOutcome::Unknown => Ok(unknown_job(&id)),
            RunNowOutcome::Conflict => Ok(ToolOutcome::error_code(
                "already_running",
                format!("scheduler job {id:?} is already running"),
                Some("Wait for the in-flight run to finish, then retry."),
            )),
            RunNowOutcome::Disabled => Ok(ToolOutcome::error_code(
                "job_disabled",
                format!("scheduler job {id:?} is disabled and was not run"),
                Some("Enable it with scheduler_enable_job first."),
            )),
            RunNowOutcome::Skipped => Ok(ToolOutcome::error_code(
                "skipped",
                format!("scheduler job {id:?} was skipped before firing (it was deleted or disabled mid-call)"),
                None::<String>,
            )),
            RunNowOutcome::Ran(result) => {
                let now_ms = Utc::now().timestamp_millis();
                let status = match result.status {
                    crate::output::RunStatus::Ok => "ok",
                    crate::output::RunStatus::Error => "error",
                };
                let job = self.deps.state.jobs().into_iter().find(|j| j.id == id);
                let job_value = job
                    .map(|j| job_view(&self.deps, &j, now_ms))
                    .unwrap_or(Value::Null);
                let body = json!({
                    "status": status,
                    "firedAt": result.fired_at.to_rfc3339(),
                    "finishedAt": result.finished_at.to_rfc3339(),
                    "outputPath": result.output_path.as_ref().map(|p| p.display().to_string()),
                    "error": result.error,
                    "job": job_value,
                });
                let text = render(&body);
                // An error run is data, not a host fault: surface it as an error
                // outcome so the model can explain the failure.
                match result.status {
                    crate::output::RunStatus::Ok => Ok(ToolOutcome::ok(text)),
                    crate::output::RunStatus::Error => Ok(ToolOutcome {
                        content: vec![ToolContent::Text { text: text.clone() }],
                        text,
                        is_error: true,
                        error: Some(tool_registry::ToolError {
                            code: "run_error".to_string(),
                            message: result
                                .error
                                .unwrap_or_else(|| "run failed".to_string()),
                            hint: None,
                        }),
                    }),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// create
// ---------------------------------------------------------------------------

fn create_spec() -> ToolSpec {
    ToolSpec::new(
        "scheduler_create_job",
        "Create a new scheduler job from a raw cron expression + IANA timezone \
         and a prompt message. The job is enabled by default. Returns the \
         created job including its computed next fire time so you can confirm \
         the schedule. Use raw cron (e.g. \"0 8 * * *\"); natural-language \
         schedules are not supported. Non-destructive; no confirmation needed.",
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Optional human-readable name." },
                "enabled": { "type": "boolean", "description": "Whether the job is enabled (default true)." },
                "schedule": {
                    "type": "object",
                    "properties": {
                        "cron": { "type": "string", "description": "Raw cron expression, e.g. \"0 8 * * *\"." },
                        "tz": { "type": "string", "description": "IANA timezone, e.g. \"Europe/Paris\"." },
                        "startAt": { "type": "string", "description": "Optional RFC3339 anchor; the schedule does not fire before this instant." }
                    },
                    "required": ["cron", "tz"],
                    "additionalProperties": false,
                },
                "payload": {
                    "type": "object",
                    "properties": {
                        "message": { "type": "string", "description": "The prompt the job runs each fire." }
                    },
                    "required": ["message"],
                    "additionalProperties": false,
                },
                "timeoutMs": { "type": "integer", "minimum": 1, "description": "Optional per-job run timeout in ms." }
            },
            "required": ["schedule", "payload"],
            "additionalProperties": false,
        }),
    )
    .with_access(true, true)
}

struct CreateTool {
    deps: ToolDeps,
}

#[async_trait::async_trait]
impl ToolExecutor for CreateTool {
    async fn execute(&self, args: Value) -> Result<ToolOutcome> {
        let Some(schedule_arg) = args.get("schedule") else {
            return Ok(ToolOutcome::error_code(
                "invalid_args",
                "'schedule' object is required",
                None::<String>,
            ));
        };
        let schedule = match parse_and_validate_schedule(schedule_arg) {
            Ok(s) => s,
            Err(out) => return Ok(*out),
        };
        let Some(payload_arg) = args.get("payload") else {
            return Ok(ToolOutcome::error_code(
                "invalid_args",
                "'payload' object is required",
                None::<String>,
            ));
        };
        let payload = match parse_and_validate_payload(payload_arg) {
            Ok(p) => p,
            Err(out) => return Ok(*out),
        };

        let mut jobs = self.deps.state.jobs();
        let id = generate_job_id(&jobs);
        let job = ScheduleJob {
            id,
            name: args
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            enabled: args.get("enabled").and_then(Value::as_bool).unwrap_or(true),
            schedule,
            payload,
            timeout_ms: args.get("timeoutMs").and_then(Value::as_u64),
        };
        jobs.push(job.clone());
        if let Err(out) = persist(&self.deps, jobs) {
            return Ok(*out);
        }
        let now_ms = Utc::now().timestamp_millis();
        Ok(ToolOutcome::ok(render(&job_view(&self.deps, &job, now_ms))))
    }
}

// ---------------------------------------------------------------------------
// update
// ---------------------------------------------------------------------------

fn update_spec() -> ToolSpec {
    ToolSpec::new(
        "scheduler_update_job",
        "Update an existing scheduler job's name, enabled flag, schedule \
         (raw cron + IANA timezone), prompt, or per-job timeout. Only the \
         provided fields change. Returns the updated job including its \
         recomputed next fire time. Non-destructive (does not delete the job).",
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The job id to update." },
                "name": { "type": "string" },
                "enabled": { "type": "boolean", "description": "Set enabled/disabled. Setting enabled=false is destructive and requires confirm=true." },
                "schedule": {
                    "type": "object",
                    "properties": {
                        "cron": { "type": "string" },
                        "tz": { "type": "string" },
                        "startAt": { "type": "string" }
                    },
                    "required": ["cron", "tz"],
                    "additionalProperties": false,
                },
                "payload": {
                    "type": "object",
                    "properties": { "message": { "type": "string" } },
                    "required": ["message"],
                    "additionalProperties": false,
                },
                "timeoutMs": { "type": ["integer", "null"], "minimum": 1, "description": "Per-job timeout in ms; null clears it (inherit the default)." },
                "confirm": { "type": "boolean", "description": "Required only when setting enabled=false." }
            },
            "required": ["id"],
            "additionalProperties": false,
        }),
    )
    .with_access(true, true)
}

struct UpdateTool {
    deps: ToolDeps,
}

#[async_trait::async_trait]
impl ToolExecutor for UpdateTool {
    async fn execute(&self, args: Value) -> Result<ToolOutcome> {
        let id = match require_job_id(&args) {
            Ok(id) => id,
            Err(out) => return Ok(*out),
        };
        let mut jobs = self.deps.state.jobs();
        let Some(idx) = jobs.iter().position(|j| j.id == id) else {
            return Ok(unknown_job(&id));
        };
        let mut job = jobs[idx].clone();
        if let Some(name) = args.get("name").and_then(Value::as_str) {
            job.name = name.to_string();
        }
        if let Some(enabled) = args.get("enabled").and_then(Value::as_bool) {
            if !enabled && job.enabled {
                if let Err(out) = require_confirm(&args, "disabling a scheduler job via update") {
                    return Ok(*out);
                }
            }
            job.enabled = enabled;
        }
        if let Some(schedule_arg) = args.get("schedule") {
            job.schedule = match parse_and_validate_schedule(schedule_arg) {
                Ok(s) => s,
                Err(out) => return Ok(*out),
            };
        }
        if let Some(payload_arg) = args.get("payload") {
            job.payload = match parse_and_validate_payload(payload_arg) {
                Ok(p) => p,
                Err(out) => return Ok(*out),
            };
        }
        // timeoutMs present-as-null clears the override; present-as-number sets
        // it; absent leaves it unchanged.
        if let Some(timeout) = args.get("timeoutMs") {
            job.timeout_ms = match timeout {
                Value::Null => None,
                v => v.as_u64(),
            };
        }
        jobs[idx] = job.clone();
        if let Err(out) = persist(&self.deps, jobs) {
            return Ok(*out);
        }
        let now_ms = Utc::now().timestamp_millis();
        Ok(ToolOutcome::ok(render(&job_view(&self.deps, &job, now_ms))))
    }
}

// ---------------------------------------------------------------------------
// enable / disable
// ---------------------------------------------------------------------------

fn enable_spec() -> ToolSpec {
    ToolSpec::new(
        "scheduler_enable_job",
        "Enable a scheduler job so it resumes firing on its schedule. Returns \
         the job with its recomputed next fire time. Non-destructive; no \
         confirmation needed.",
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The job id to enable." }
            },
            "required": ["id"],
            "additionalProperties": false,
        }),
    )
    .with_access(true, true)
}

fn disable_spec() -> ToolSpec {
    ToolSpec::new(
        "scheduler_disable_job",
        "Disable a scheduler job so it stops firing (its definition is kept). \
         DESTRUCTIVE: stops automation, so it requires \"confirm\": true; \
         without it the job is left unchanged.",
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The job id to disable." },
                "confirm": { "type": "boolean", "description": "Must be true to actually disable the job." }
            },
            "required": ["id"],
            "additionalProperties": false,
        }),
    )
    .with_access(true, true)
}

struct SetEnabledTool {
    deps: ToolDeps,
    enable: bool,
}

#[async_trait::async_trait]
impl ToolExecutor for SetEnabledTool {
    async fn execute(&self, args: Value) -> Result<ToolOutcome> {
        let id = match require_job_id(&args) {
            Ok(id) => id,
            Err(out) => return Ok(*out),
        };
        // Disable is destructive: require explicit confirmation, make no change
        // without it.
        if !self.enable {
            if let Err(out) = require_confirm(&args, "disabling a scheduler job") {
                return Ok(*out);
            }
        }
        let mut jobs = self.deps.state.jobs();
        let Some(idx) = jobs.iter().position(|j| j.id == id) else {
            return Ok(unknown_job(&id));
        };
        jobs[idx].enabled = self.enable;
        let job = jobs[idx].clone();
        if let Err(out) = persist(&self.deps, jobs) {
            return Ok(*out);
        }
        let now_ms = Utc::now().timestamp_millis();
        Ok(ToolOutcome::ok(render(&job_view(&self.deps, &job, now_ms))))
    }
}

// ---------------------------------------------------------------------------
// delete
// ---------------------------------------------------------------------------

fn delete_spec() -> ToolSpec {
    ToolSpec::new(
        "scheduler_delete_job",
        "Delete a scheduler job permanently. DESTRUCTIVE: removes the job \
         definition, so it requires \"confirm\": true; without it the job is \
         left unchanged.",
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The job id to delete." },
                "confirm": { "type": "boolean", "description": "Must be true to actually delete the job." }
            },
            "required": ["id"],
            "additionalProperties": false,
        }),
    )
    .with_access(true, true)
}

struct DeleteTool {
    deps: ToolDeps,
}

#[async_trait::async_trait]
impl ToolExecutor for DeleteTool {
    async fn execute(&self, args: Value) -> Result<ToolOutcome> {
        let id = match require_job_id(&args) {
            Ok(id) => id,
            Err(out) => return Ok(*out),
        };
        if let Err(out) = require_confirm(&args, "deleting a scheduler job") {
            return Ok(*out);
        }
        let mut jobs = self.deps.state.jobs();
        let before = jobs.len();
        jobs.retain(|j| j.id != id);
        if jobs.len() == before {
            return Ok(unknown_job(&id));
        }
        if let Err(out) = persist(&self.deps, jobs) {
            return Ok(*out);
        }
        Ok(ToolOutcome::ok(render(&json!({ "deleted": id }))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{jobs_path, load_jobs};
    use cap_runner::{ExitKind, KillReason, Runner, RunnerHandle, SpawnParams};
    use tool_registry::ToolRegistry;

    fn deps() -> (ToolDeps, tempfile::TempDir) {
        deps_with_runner(false)
    }

    /// In-test runner that exits normally (or abnormally) after a short sleep,
    /// so run-now exercises the real execution path.
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
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
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

    fn deps_with_runner(abnormal: bool) -> (ToolDeps, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(SchedulerState::new(vec![]));
        let mut services = ServiceRegistry::default();
        services
            .service::<dyn Runner>("fake", Arc::new(FakeRunner { abnormal }))
            .unwrap();
        let config = SchedulerConfig {
            root: dir.path().to_path_buf(),
            runner_kind: "fake".to_string(),
            runner_command: String::new(),
            runner_model: None,
            runner_provider: None,
            runner_thinking: None,
            system_context: None,
            max_run_timeout_ms: 3_600_000,
            poll_interval_ms: 2_000,
            job_timeout_ms: 60_000,
        };
        (
            ToolDeps {
                state,
                root: dir.path().to_path_buf(),
                config,
                services,
            },
            dir,
        )
    }

    async fn dispatch(deps: &ToolDeps, name: &str, args: Value) -> ToolOutcome {
        let reg = ToolRegistry::new();
        register_into(&reg, deps.clone()).unwrap();
        reg.dispatch(name, args).await
    }

    fn parse(out: &ToolOutcome) -> Value {
        serde_json::from_str(&out.text).unwrap()
    }

    async fn create_job(deps: &ToolDeps, cron: &str, tz: &str, msg: &str) -> String {
        let out = dispatch(
            deps,
            "scheduler_create_job",
            json!({
                "name": "T",
                "schedule": { "cron": cron, "tz": tz },
                "payload": { "message": msg }
            }),
        )
        .await;
        assert!(!out.is_error, "create failed: {}", out.text);
        parse(&out)["id"].as_str().unwrap().to_string()
    }

    // --- discoverability ---------------------------------------------------

    #[test]
    fn registers_the_full_tool_set() {
        let (deps, _dir) = deps();
        let reg = ToolRegistry::new();
        register_into(&reg, deps).unwrap();
        let mut names: Vec<String> = reg.list().into_iter().map(|s| s.name).collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "scheduler_create_job",
                "scheduler_delete_job",
                "scheduler_disable_job",
                "scheduler_enable_job",
                "scheduler_get_job",
                "scheduler_job_status",
                "scheduler_list_jobs",
                "scheduler_run_job_now",
                "scheduler_tail_output",
                "scheduler_update_job",
            ]
        );
    }

    #[test]
    fn read_tools_are_read_only_and_mutations_write() {
        let (deps, _dir) = deps();
        let reg = ToolRegistry::new();
        register_into(&reg, deps).unwrap();
        for spec in reg.list() {
            match spec.name.as_str() {
                "scheduler_list_jobs"
                | "scheduler_get_job"
                | "scheduler_job_status"
                | "scheduler_tail_output" => {
                    assert!(spec.access.read, "{} should read", spec.name);
                    assert!(!spec.access.write, "{} should not write", spec.name);
                }
                _ => assert!(spec.access.write, "{} should write", spec.name),
            }
        }
    }

    // --- create / list / get / status -------------------------------------

    #[tokio::test]
    async fn create_persists_and_lists() {
        let (deps, dir) = deps();
        let id = create_job(&deps, "0 8 * * *", "UTC", "hi").await;
        // Persisted to disk.
        assert!(jobs_path(dir.path()).exists());
        let loaded = load_jobs(dir.path(), |_| {});
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, id);

        let out = dispatch(&deps, "scheduler_list_jobs", json!({})).await;
        let v = parse(&out);
        assert_eq!(v["jobs"].as_array().unwrap().len(), 1);
        let job = &v["jobs"][0];
        assert_eq!(job["id"], id);
        // Runtime + diagnostics present.
        assert!(job.get("nextRunAtMs").is_some());
        assert!(
            job["nextRunAt"].is_null(),
            "unarmed runtime next fire follows HTTP semantics"
        );
        assert!(job["computedNextRunAtMs"].is_i64());
        assert!(job["computedNextRunAt"].is_string());
        assert!(job["scheduleHuman"].as_str().unwrap().contains("UTC"));
    }

    #[tokio::test]
    async fn create_bad_cron_is_invalid_schedule_error() {
        let (deps, _dir) = deps();
        let out = dispatch(
            &deps,
            "scheduler_create_job",
            json!({ "schedule": { "cron": "not a cron", "tz": "UTC" }, "payload": { "message": "hi" } }),
        )
        .await;
        assert!(out.is_error);
        assert_eq!(out.error.as_ref().unwrap().code, "invalid_schedule");
    }

    #[tokio::test]
    async fn create_bad_tz_is_invalid_schedule_error() {
        let (deps, _dir) = deps();
        let out = dispatch(
            &deps,
            "scheduler_create_job",
            json!({ "schedule": { "cron": "0 8 * * *", "tz": "Mars/Base" }, "payload": { "message": "hi" } }),
        )
        .await;
        assert!(out.is_error);
        assert_eq!(out.error.as_ref().unwrap().code, "invalid_schedule");
    }

    #[tokio::test]
    async fn create_empty_message_is_invalid_args() {
        let (deps, _dir) = deps();
        let out = dispatch(
            &deps,
            "scheduler_create_job",
            json!({ "schedule": { "cron": "0 8 * * *", "tz": "UTC" }, "payload": { "message": "   " } }),
        )
        .await;
        assert!(out.is_error);
        assert!(out.text.contains("payload.message is required"));
    }

    #[tokio::test]
    async fn get_unknown_is_error() {
        let (deps, _dir) = deps();
        let out = dispatch(&deps, "scheduler_get_job", json!({ "id": "nope" })).await;
        assert!(out.is_error);
        assert_eq!(out.error.as_ref().unwrap().code, "unknown_job");
    }

    #[tokio::test]
    async fn get_reflects_persisted_job() {
        let (deps, _dir) = deps();
        let id = create_job(&deps, "0 8 * * *", "UTC", "hi").await;
        let out = dispatch(&deps, "scheduler_get_job", json!({ "id": id.clone() })).await;
        let v = parse(&out);
        assert_eq!(v["id"], id);
        assert_eq!(v["payload"]["message"], "hi");
    }

    #[tokio::test]
    async fn status_reports_next_fire_for_enabled_and_disabled() {
        let (deps, _dir) = deps();
        let id = create_job(&deps, "0 8 * * *", "UTC", "hi").await;
        let out = dispatch(&deps, "scheduler_job_status", json!({ "id": id.clone() })).await;
        let v = parse(&out);
        assert_eq!(v["enabled"], true);
        assert_eq!(v["running"], false);
        assert!(v["nextRunAtMs"].is_i64());
        assert!(v["nextRunAt"].is_string());

        // Disable and confirm status still computes a next fire diagnostic.
        dispatch(
            &deps,
            "scheduler_disable_job",
            json!({ "id": id.clone(), "confirm": true }),
        )
        .await;
        let out = dispatch(&deps, "scheduler_job_status", json!({ "id": id })).await;
        let v = parse(&out);
        assert_eq!(v["enabled"], false);
        assert!(v["nextRunAtMs"].is_i64(), "next fire still computed");
    }

    #[tokio::test]
    async fn invalid_id_is_rejected() {
        let (deps, _dir) = deps();
        let out = dispatch(&deps, "scheduler_get_job", json!({ "id": "../escape" })).await;
        assert!(out.is_error);
        assert_eq!(out.error.as_ref().unwrap().code, "invalid_args");
        assert!(out.text.contains("unsafe job id"));
    }

    // --- update / enable / disable ----------------------------------------

    #[tokio::test]
    async fn update_patches_only_given_fields() {
        let (deps, _dir) = deps();
        let id = create_job(&deps, "0 8 * * *", "UTC", "hi").await;
        let out = dispatch(
            &deps,
            "scheduler_update_job",
            json!({ "id": id.clone(), "name": "Renamed", "timeoutMs": 5000 }),
        )
        .await;
        let v = parse(&out);
        assert_eq!(v["name"], "Renamed");
        assert_eq!(v["timeoutMs"], 5000);
        // Schedule + payload untouched.
        assert_eq!(v["payload"]["message"], "hi");
    }

    #[tokio::test]
    async fn update_bad_schedule_is_error_and_persists_nothing() {
        let (deps, dir) = deps();
        let id = create_job(&deps, "0 8 * * *", "UTC", "hi").await;
        let out = dispatch(
            &deps,
            "scheduler_update_job",
            json!({ "id": id, "schedule": { "cron": "0 8 * * *", "tz": "Not/AZone" } }),
        )
        .await;
        assert!(out.is_error);
        assert_eq!(out.error.as_ref().unwrap().code, "invalid_schedule");
        // Original schedule preserved on disk.
        let loaded = load_jobs(dir.path(), |_| {});
        assert_eq!(loaded[0].schedule.tz, "UTC");
    }

    #[tokio::test]
    async fn update_unknown_is_error() {
        let (deps, _dir) = deps();
        let out = dispatch(
            &deps,
            "scheduler_update_job",
            json!({ "id": "nope", "name": "x" }),
        )
        .await;
        assert!(out.is_error);
        assert_eq!(out.error.as_ref().unwrap().code, "unknown_job");
    }

    #[tokio::test]
    async fn update_enabled_false_requires_confirmation_and_no_change() {
        let (deps, dir) = deps();
        let id = create_job(&deps, "0 8 * * *", "UTC", "hi").await;
        let out = dispatch(
            &deps,
            "scheduler_update_job",
            json!({ "id": id.clone(), "enabled": false }),
        )
        .await;
        assert!(out.is_error);
        assert_eq!(out.error.as_ref().unwrap().code, "confirmation_required");
        let loaded = load_jobs(dir.path(), |_| {});
        assert!(
            loaded[0].enabled,
            "update must preserve enabled without confirm"
        );
    }

    #[tokio::test]
    async fn update_enabled_false_with_confirmation_disables() {
        let (deps, dir) = deps();
        let id = create_job(&deps, "0 8 * * *", "UTC", "hi").await;
        let out = dispatch(
            &deps,
            "scheduler_update_job",
            json!({ "id": id, "enabled": false, "confirm": true }),
        )
        .await;
        assert!(!out.is_error);
        assert_eq!(parse(&out)["enabled"], false);
        let loaded = load_jobs(dir.path(), |_| {});
        assert!(!loaded[0].enabled);
    }

    #[tokio::test]
    async fn enable_does_not_require_confirmation() {
        let (deps, _dir) = deps();
        let id = create_job(&deps, "0 8 * * *", "UTC", "hi").await;
        // Disable (with confirm) then enable (no confirm).
        dispatch(
            &deps,
            "scheduler_disable_job",
            json!({ "id": id.clone(), "confirm": true }),
        )
        .await;
        let out = dispatch(&deps, "scheduler_enable_job", json!({ "id": id.clone() })).await;
        assert!(!out.is_error, "enable needs no confirm: {}", out.text);
        assert_eq!(parse(&out)["enabled"], true);
    }

    #[tokio::test]
    async fn disable_without_confirm_is_rejected_and_no_change() {
        let (deps, dir) = deps();
        let id = create_job(&deps, "0 8 * * *", "UTC", "hi").await;
        let out = dispatch(&deps, "scheduler_disable_job", json!({ "id": id.clone() })).await;
        assert!(out.is_error);
        assert_eq!(out.error.as_ref().unwrap().code, "confirmation_required");
        // Still enabled on disk.
        let loaded = load_jobs(dir.path(), |_| {});
        assert!(loaded[0].enabled, "no change without confirm");
    }

    #[tokio::test]
    async fn disable_with_confirm_disables() {
        let (deps, dir) = deps();
        let id = create_job(&deps, "0 8 * * *", "UTC", "hi").await;
        let out = dispatch(
            &deps,
            "scheduler_disable_job",
            json!({ "id": id.clone(), "confirm": true }),
        )
        .await;
        assert!(!out.is_error);
        assert_eq!(parse(&out)["enabled"], false);
        let loaded = load_jobs(dir.path(), |_| {});
        assert!(!loaded[0].enabled);
    }

    // --- delete ------------------------------------------------------------

    #[tokio::test]
    async fn delete_without_confirm_is_rejected_and_no_change() {
        let (deps, dir) = deps();
        let id = create_job(&deps, "0 8 * * *", "UTC", "hi").await;
        let out = dispatch(&deps, "scheduler_delete_job", json!({ "id": id.clone() })).await;
        assert!(out.is_error);
        assert_eq!(out.error.as_ref().unwrap().code, "confirmation_required");
        let loaded = load_jobs(dir.path(), |_| {});
        assert_eq!(loaded.len(), 1, "job preserved without confirm");
    }

    #[tokio::test]
    async fn delete_with_confirm_removes() {
        let (deps, dir) = deps();
        let id = create_job(&deps, "0 8 * * *", "UTC", "hi").await;
        let out = dispatch(
            &deps,
            "scheduler_delete_job",
            json!({ "id": id.clone(), "confirm": true }),
        )
        .await;
        assert!(!out.is_error);
        assert_eq!(parse(&out)["deleted"], id);
        let loaded = load_jobs(dir.path(), |_| {});
        assert!(loaded.is_empty());
        // Deleting again is unknown_job.
        let out = dispatch(
            &deps,
            "scheduler_delete_job",
            json!({ "id": id, "confirm": true }),
        )
        .await;
        assert_eq!(out.error.as_ref().unwrap().code, "unknown_job");
    }

    // --- run-now -----------------------------------------------------------

    #[tokio::test]
    async fn run_now_does_not_require_confirmation_and_writes_output() {
        let (deps, dir) = deps_with_runner(false);
        // Yearly schedule so the timer won't independently fire during the test.
        let id = create_job(&deps, "0 0 1 1 *", "UTC", "hi").await;
        let out = dispatch(&deps, "scheduler_run_job_now", json!({ "id": id.clone() })).await;
        assert!(!out.is_error, "run-now should succeed: {}", out.text);
        let v = parse(&out);
        assert_eq!(v["status"], "ok");
        let path = v["outputPath"].as_str().unwrap();
        assert!(std::path::Path::new(path).exists());
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
    async fn run_now_unknown_is_error() {
        let (deps, _dir) = deps_with_runner(false);
        let out = dispatch(&deps, "scheduler_run_job_now", json!({ "id": "nope" })).await;
        assert!(out.is_error);
        assert_eq!(out.error.as_ref().unwrap().code, "unknown_job");
    }

    #[tokio::test]
    async fn run_now_already_running_is_conflict() {
        let (deps, _dir) = deps_with_runner(false);
        let id = create_job(&deps, "0 0 1 1 *", "UTC", "hi").await;
        // Claim the run so run-now hits the same overlap gate the loop uses.
        assert!(deps
            .state
            .try_claim_running(&id, Utc::now().timestamp_millis()));
        let out = dispatch(&deps, "scheduler_run_job_now", json!({ "id": id })).await;
        assert!(out.is_error);
        assert_eq!(out.error.as_ref().unwrap().code, "already_running");
    }

    #[tokio::test]
    async fn run_now_error_run_is_surfaced_as_error() {
        let (deps, _dir) = deps_with_runner(true);
        let id = create_job(&deps, "0 0 1 1 *", "UTC", "hi").await;
        let out = dispatch(&deps, "scheduler_run_job_now", json!({ "id": id })).await;
        assert!(out.is_error);
        assert_eq!(out.error.as_ref().unwrap().code, "run_error");
        assert_eq!(parse(&out)["status"], "error");
    }

    #[tokio::test]
    async fn run_now_preserves_next_fire() {
        let (deps, _dir) = deps_with_runner(false);
        let id = create_job(&deps, "0 0 1 1 *", "UTC", "hi").await;
        let next = Utc::now().timestamp_millis() + 3_600_000;
        deps.state.set_next_run(&id, Some(next));
        dispatch(&deps, "scheduler_run_job_now", json!({ "id": id.clone() })).await;
        assert_eq!(deps.state.runtime(&id).next_run_at_ms, Some(next));
    }

    // --- tail --------------------------------------------------------------

    #[tokio::test]
    async fn tail_no_output_is_clear_error() {
        let (deps, _dir) = deps();
        let id = create_job(&deps, "0 8 * * *", "UTC", "hi").await;
        let out = dispatch(&deps, "scheduler_tail_output", json!({ "id": id })).await;
        assert!(out.is_error);
        assert_eq!(out.error.as_ref().unwrap().code, "no_output");
    }

    #[tokio::test]
    async fn tail_returns_newest_output() {
        let (deps, dir) = deps();
        let id = create_job(&deps, "0 8 * * *", "UTC", "hi").await;
        let out_dir = dir.path().join("cron/output").join(&id);
        std::fs::create_dir_all(&out_dir).unwrap();
        std::fs::write(out_dir.join("2026-01-01_00-00-00.md"), "old").unwrap();
        std::fs::write(out_dir.join("2026-06-01_12-00-00.md"), "newest").unwrap();
        let out = dispatch(&deps, "scheduler_tail_output", json!({ "id": id })).await;
        assert!(!out.is_error);
        let v = parse(&out);
        assert_eq!(v["content"], "newest");
    }

    #[tokio::test]
    async fn tail_unknown_is_error() {
        let (deps, _dir) = deps();
        let out = dispatch(&deps, "scheduler_tail_output", json!({ "id": "nope" })).await;
        assert!(out.is_error);
        assert_eq!(out.error.as_ref().unwrap().code, "unknown_job");
    }
}
