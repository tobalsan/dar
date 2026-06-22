//! `example-jobs` — an EXAMPLE extension that exposes validated, JSON-backed
//! mutation tools through the host tool registry.
//!
//! ## What this demonstrates (and what it is not)
//!
//! This is a **demonstration of ALG-253's host tool registry + MCP bridge**: it
//! shows how an extension can expose *validated, host-executed mutation of
//! project state* — `jobs_create` / `jobs_edit` / `jobs_list` — instead of the
//! agent making ad hoc edits to a file. The host owns the read-modify-write,
//! enforces an input schema, and returns a structured outcome.
//!
//! It is **NOT** the cron scheduler extension (ALG-218), and these tools are
//! **not** how that scheduler talks to agents (ALG-218 deliberately exposes no
//! agent tools; its agents edit `cron/jobs.json` directly). The jobs file here
//! is a generic stand-in used only to exercise the registry's validated
//! mutation path end to end.
//!
//! ## Backing store
//!
//! State lives in a single JSON file shaped as `{ "jobs": [ <job>, ... ] }`.
//! Each job is `{ id, name, schedule, command, enabled }`. The path comes from
//! `extensions.example-jobs.path` in `agent.yaml` (defaulting to `jobs.json`),
//! so the codex/MCP bridge config-parity path is exercised the same way the
//! `example-tool` fixture exercises it.
//!
//! ## Failure model
//!
//! - Invalid input (missing/blank required fields, wrong types, unknown job id)
//!   is a **structured failure** (`ToolOutcome::error`), not a host fault — the
//!   agent's run continues.
//! - A malformed backing file is reported as a clear structured failure and is
//!   **never overwritten**, so a hand-corrupted file is not silently clobbered.
//! - Writes are atomic (temp file + rename) so a crash mid-write cannot leave a
//!   half-written, corrupt jobs file.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use host_api::{Extension, RegisterCtx};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tool_registry::{
    ToolExecutor, ToolOutcome, ToolRegistryHandle, ToolSpec, TOOL_REGISTRY_SERVICE,
};

/// Default backing-file path, relative to the agent root, when
/// `extensions.example-jobs.path` is not set.
const DEFAULT_JOBS_PATH: &str = "jobs.json";

pub fn extension() -> Box<dyn Extension> {
    Box::new(ExampleJobsExtension)
}

pub struct ExampleJobsExtension;

/// Per-extension config read from `extensions.example-jobs` in `agent.yaml`.
#[derive(Debug, Clone, Deserialize)]
pub struct ExampleJobsConfig {
    /// Path to the backing jobs JSON file.
    #[serde(default = "default_jobs_path")]
    pub path: PathBuf,
}

impl Default for ExampleJobsConfig {
    fn default() -> Self {
        Self {
            path: default_jobs_path(),
        }
    }
}

fn default_jobs_path() -> PathBuf {
    PathBuf::from(DEFAULT_JOBS_PATH)
}

impl Extension for ExampleJobsExtension {
    fn id(&self) -> &'static str {
        "example-jobs"
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let config = match ctx.config.get(self.id()) {
                Some(value) => serde_json::from_value::<ExampleJobsConfig>(value.clone())
                    .context("parsing extensions.example-jobs config")?,
                None => ExampleJobsConfig::default(),
            };
            let registry = ctx
                .services
                .get_named::<dyn ToolRegistryHandle>(TOOL_REGISTRY_SERVICE)
                .context("example-jobs requires the tool-registry-host extension")?;
            register_into(registry.as_ref(), config.path)
        })
    }
}

/// Register the three jobs tools against `registry`, all backed by the JSON file
/// at `path`. Shared by the extension `register()` pass and the tests.
pub fn register_into(registry: &dyn ToolRegistryHandle, path: PathBuf) -> Result<()> {
    let store = Arc::new(JobStore::new(path));

    registry.register_tool(
        ToolSpec::new(
            "jobs_create",
            "Create a new job in the JSON-backed jobs file. Fails (structured) \
             if required fields are missing/blank or the id already exists.",
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Unique job id." },
                    "name": { "type": "string", "description": "Human-readable job name." },
                    "schedule": { "type": "string", "description": "Cron-style schedule expression." },
                    "command": { "type": "string", "description": "Command the job runs." },
                    "enabled": { "type": "boolean", "description": "Whether the job is enabled. Defaults to true." }
                },
                "required": ["id", "name", "schedule", "command"],
                "additionalProperties": false,
            }),
        )
        .with_access(false, true),
        Arc::new(JobsCreate {
            store: Arc::clone(&store),
        }),
    )?;

    registry.register_tool(
        ToolSpec::new(
            "jobs_edit",
            "Edit an existing job by id. Only the provided fields change. Fails \
             (structured) if the id is unknown or a provided field is invalid.",
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Id of the job to edit." },
                    "name": { "type": "string", "description": "New name." },
                    "schedule": { "type": "string", "description": "New schedule expression." },
                    "command": { "type": "string", "description": "New command." },
                    "enabled": { "type": "boolean", "description": "New enabled flag." }
                },
                "required": ["id"],
                "additionalProperties": false,
            }),
        )
        .with_access(true, true),
        Arc::new(JobsEdit {
            store: Arc::clone(&store),
        }),
    )?;

    registry.register_tool(
        ToolSpec::new(
            "jobs_list",
            "List all jobs from the JSON-backed jobs file. Returns an empty list \
             if the file does not exist yet.",
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }),
        )
        .reads(),
        Arc::new(JobsList {
            store: Arc::clone(&store),
        }),
    )?;

    Ok(())
}

/// One job record in the backing file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub name: String,
    pub schedule: String,
    pub command: String,
    pub enabled: bool,
}

/// On-disk document shape: `{ "jobs": [ ... ] }`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct JobsDoc {
    #[serde(default)]
    jobs: Vec<Job>,
}

/// Path-backed jobs store. The host owns all reads/writes; the agent never
/// touches the file directly. Reads tolerate a missing file (empty doc) but
/// surface a malformed file as a clear error without overwriting it.
///
/// Each call does a full read-modify-write; there is no in-process lock, so
/// concurrent mutations could lose an update (last-rename-wins). That is
/// acceptable for this single-agent demo, and the atomic temp-file+rename
/// still rules out a *corrupt* (half-written) file even under a racing write.
pub struct JobStore {
    path: PathBuf,
}

impl JobStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Load the document. A missing file is an empty document; a malformed file
    /// is an `Err` so callers can fail loudly without clobbering it.
    fn load(&self) -> Result<JobsDoc, StoreError> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(JobsDoc::default());
            }
            Err(err) => {
                return Err(StoreError::Io(format!(
                    "could not read jobs file {}: {err}",
                    self.path.display()
                )));
            }
        };
        if raw.trim().is_empty() {
            return Ok(JobsDoc::default());
        }
        serde_json::from_str::<JobsDoc>(&raw).map_err(|err| {
            StoreError::Malformed(format!(
                "jobs file {} is malformed and was left untouched: {err}",
                self.path.display()
            ))
        })
    }

    /// Atomically persist the document: write to a sibling temp file then
    /// rename over the target, so a crash mid-write cannot corrupt the file.
    fn save(&self, doc: &JobsDoc) -> Result<(), StoreError> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|err| {
                    StoreError::Io(format!(
                        "could not create directory {}: {err}",
                        parent.display()
                    ))
                })?;
            }
        }
        let mut serialized = serde_json::to_string_pretty(doc)
            .map_err(|err| StoreError::Io(format!("could not serialize jobs: {err}")))?;
        serialized.push('\n');

        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serialized.as_bytes()).map_err(|err| {
            StoreError::Io(format!(
                "could not write temp jobs file {}: {err}",
                tmp.display()
            ))
        })?;
        std::fs::rename(&tmp, &self.path).map_err(|err| {
            StoreError::Io(format!(
                "could not replace jobs file {}: {err}",
                self.path.display()
            ))
        })?;
        Ok(())
    }
}

/// Internal store error, rendered to a structured `ToolOutcome::error`.
enum StoreError {
    Io(String),
    Malformed(String),
}

impl StoreError {
    fn message(&self) -> &str {
        match self {
            StoreError::Io(m) | StoreError::Malformed(m) => m,
        }
    }
}

/// Pull a required, non-blank string field from the args object, or return a
/// structured-failure message describing what was wrong.
fn require_str(args: &Value, field: &str) -> Result<String, String> {
    match args.get(field) {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.clone()),
        Some(Value::String(_)) => Err(format!("'{field}' must not be blank")),
        Some(_) => Err(format!("'{field}' must be a string")),
        None => Err(format!("'{field}' is required")),
    }
}

/// Pull an optional string field; `Ok(None)` if absent, error if present but not
/// a (non-blank) string.
fn optional_str(args: &Value, field: &str) -> Result<Option<String>, String> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.clone())),
        Some(Value::String(_)) => Err(format!("'{field}' must not be blank")),
        Some(_) => Err(format!("'{field}' must be a string")),
    }
}

/// Pull an optional boolean field; error if present but not a boolean.
fn optional_bool(args: &Value, field: &str) -> Result<Option<bool>, String> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(format!("'{field}' must be a boolean")),
    }
}

struct JobsCreate {
    store: Arc<JobStore>,
}

#[async_trait::async_trait]
impl ToolExecutor for JobsCreate {
    async fn execute(&self, args: Value) -> Result<ToolOutcome> {
        let job = match build_new_job(&args) {
            Ok(job) => job,
            Err(msg) => return Ok(ToolOutcome::error(msg)),
        };

        let mut doc = match self.store.load() {
            Ok(doc) => doc,
            Err(err) => return Ok(ToolOutcome::error(err.message())),
        };

        if doc.jobs.iter().any(|j| j.id == job.id) {
            return Ok(ToolOutcome::error(format!(
                "a job with id '{}' already exists",
                job.id
            )));
        }

        doc.jobs.push(job.clone());
        if let Err(err) = self.store.save(&doc) {
            return Ok(ToolOutcome::error(err.message()));
        }
        Ok(ToolOutcome::ok(format!(
            "created job '{}' ({})",
            job.id, job.name
        )))
    }
}

fn build_new_job(args: &Value) -> Result<Job, String> {
    if !args.is_object() {
        return Err("arguments must be an object".to_string());
    }
    Ok(Job {
        id: require_str(args, "id")?,
        name: require_str(args, "name")?,
        schedule: require_str(args, "schedule")?,
        command: require_str(args, "command")?,
        enabled: optional_bool(args, "enabled")?.unwrap_or(true),
    })
}

struct JobsEdit {
    store: Arc<JobStore>,
}

#[async_trait::async_trait]
impl ToolExecutor for JobsEdit {
    async fn execute(&self, args: Value) -> Result<ToolOutcome> {
        if !args.is_object() {
            return Ok(ToolOutcome::error("arguments must be an object"));
        }
        let id = match require_str(&args, "id") {
            Ok(id) => id,
            Err(msg) => return Ok(ToolOutcome::error(msg)),
        };

        // Validate optional fields before touching the file.
        let new_name = match optional_str(&args, "name") {
            Ok(v) => v,
            Err(msg) => return Ok(ToolOutcome::error(msg)),
        };
        let new_schedule = match optional_str(&args, "schedule") {
            Ok(v) => v,
            Err(msg) => return Ok(ToolOutcome::error(msg)),
        };
        let new_command = match optional_str(&args, "command") {
            Ok(v) => v,
            Err(msg) => return Ok(ToolOutcome::error(msg)),
        };
        let new_enabled = match optional_bool(&args, "enabled") {
            Ok(v) => v,
            Err(msg) => return Ok(ToolOutcome::error(msg)),
        };

        if new_name.is_none()
            && new_schedule.is_none()
            && new_command.is_none()
            && new_enabled.is_none()
        {
            return Ok(ToolOutcome::error(
                "no fields to edit: provide at least one of name, schedule, command, enabled",
            ));
        }

        let mut doc = match self.store.load() {
            Ok(doc) => doc,
            Err(err) => return Ok(ToolOutcome::error(err.message())),
        };

        let Some(job) = doc.jobs.iter_mut().find(|j| j.id == id) else {
            return Ok(ToolOutcome::error(format!("no job with id '{id}'")));
        };

        if let Some(name) = new_name {
            job.name = name;
        }
        if let Some(schedule) = new_schedule {
            job.schedule = schedule;
        }
        if let Some(command) = new_command {
            job.command = command;
        }
        if let Some(enabled) = new_enabled {
            job.enabled = enabled;
        }

        if let Err(err) = self.store.save(&doc) {
            return Ok(ToolOutcome::error(err.message()));
        }
        Ok(ToolOutcome::ok(format!("edited job '{id}'")))
    }
}

struct JobsList {
    store: Arc<JobStore>,
}

#[async_trait::async_trait]
impl ToolExecutor for JobsList {
    async fn execute(&self, _args: Value) -> Result<ToolOutcome> {
        let doc = match self.store.load() {
            Ok(doc) => doc,
            Err(err) => return Ok(ToolOutcome::error(err.message())),
        };
        let payload =
            serde_json::to_string_pretty(&json!({ "jobs": doc.jobs })).map_err(|err| {
                anyhow::anyhow!("could not serialize jobs list: {err}")
            })?;
        Ok(ToolOutcome::ok(payload))
    }
}

/// Convenience: render the on-disk jobs for assertions/inspection.
pub fn read_jobs(path: &Path) -> Result<Vec<Job>> {
    let store = JobStore::new(path.to_path_buf());
    store
        .load()
        .map(|doc| doc.jobs)
        .map_err(|err| anyhow::anyhow!("{}", err.message()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn store(path: &Path) -> JobStore {
        JobStore::new(path.to_path_buf())
    }

    #[tokio::test]
    async fn create_writes_a_job() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        let create = JobsCreate {
            store: Arc::new(store(&path)),
        };

        let out = create
            .execute(json!({
                "id": "nightly",
                "name": "Nightly backup",
                "schedule": "0 3 * * *",
                "command": "backup.sh",
            }))
            .await
            .unwrap();
        assert!(!out.is_error, "create should succeed: {out:?}");

        let jobs = read_jobs(&path).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, "nightly");
        assert!(jobs[0].enabled, "enabled defaults to true");
    }

    #[tokio::test]
    async fn create_rejects_duplicate_id() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        let create = JobsCreate {
            store: Arc::new(store(&path)),
        };
        let args = json!({
            "id": "dup", "name": "A", "schedule": "* * * * *", "command": "c",
        });
        create.execute(args.clone()).await.unwrap();
        let out = create.execute(args).await.unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("already exists"));
        // Only the first job persisted.
        assert_eq!(read_jobs(&path).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn create_validation_failures_are_structured() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        let create = JobsCreate {
            store: Arc::new(store(&path)),
        };

        // Missing required field.
        let out = create
            .execute(json!({ "id": "x", "name": "n", "schedule": "s" }))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("'command' is required"));

        // Blank required field.
        let out = create
            .execute(json!({ "id": "  ", "name": "n", "schedule": "s", "command": "c" }))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("'id' must not be blank"));

        // Wrong type.
        let out = create
            .execute(json!({ "id": "x", "name": "n", "schedule": "s", "command": "c", "enabled": "yes" }))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("'enabled' must be a boolean"));

        // Nothing was written on any failure.
        assert!(read_jobs(&path).unwrap().is_empty());
    }

    #[tokio::test]
    async fn edit_updates_only_provided_fields() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        let shared = Arc::new(store(&path));
        let create = JobsCreate {
            store: Arc::clone(&shared),
        };
        let edit = JobsEdit {
            store: Arc::clone(&shared),
        };

        create
            .execute(json!({
                "id": "j1", "name": "Old", "schedule": "0 0 * * *", "command": "old.sh",
            }))
            .await
            .unwrap();

        let out = edit
            .execute(json!({ "id": "j1", "command": "new.sh", "enabled": false }))
            .await
            .unwrap();
        assert!(!out.is_error, "edit should succeed: {out:?}");

        let jobs = read_jobs(&path).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "Old", "untouched field preserved");
        assert_eq!(jobs[0].command, "new.sh");
        assert!(!jobs[0].enabled);
    }

    #[tokio::test]
    async fn edit_unknown_id_is_structured_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        let edit = JobsEdit {
            store: Arc::new(store(&path)),
        };
        let out = edit
            .execute(json!({ "id": "ghost", "name": "x" }))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("no job with id 'ghost'"));
    }

    #[tokio::test]
    async fn edit_requires_at_least_one_field() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        let edit = JobsEdit {
            store: Arc::new(store(&path)),
        };
        let out = edit.execute(json!({ "id": "j1" })).await.unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("no fields to edit"));
    }

    #[tokio::test]
    async fn list_returns_jobs_and_empty_when_absent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        let shared = Arc::new(store(&path));
        let list = JobsList {
            store: Arc::clone(&shared),
        };

        // Missing file -> empty list, no error, no file created.
        let out = list.execute(json!({})).await.unwrap();
        assert!(!out.is_error);
        let parsed: Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(parsed["jobs"].as_array().unwrap().len(), 0);
        assert!(!path.exists(), "list must not create the file");

        // After a create, list reflects it.
        JobsCreate {
            store: Arc::clone(&shared),
        }
        .execute(json!({
            "id": "a", "name": "A", "schedule": "* * * * *", "command": "c",
        }))
        .await
        .unwrap();

        let out = list.execute(json!({})).await.unwrap();
        let parsed: Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(parsed["jobs"][0]["id"], "a");
    }

    #[tokio::test]
    async fn malformed_file_is_handled_gracefully_without_corruption() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        let garbage = "{ this is not valid json ]";
        std::fs::write(&path, garbage).unwrap();

        let shared = Arc::new(store(&path));

        // list surfaces a clear error.
        let out = JobsList {
            store: Arc::clone(&shared),
        }
        .execute(json!({}))
        .await
        .unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("malformed"));

        // create fails too — and must NOT overwrite the existing (bad) file.
        let out = JobsCreate {
            store: Arc::clone(&shared),
        }
        .execute(json!({
            "id": "x", "name": "n", "schedule": "s", "command": "c",
        }))
        .await
        .unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("malformed"));

        // The original bytes are intact — no corruption / clobbering.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), garbage);
    }

    #[tokio::test]
    async fn registers_all_three_tools() {
        use tool_registry::ToolRegistry;
        let dir = tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        let registry = ToolRegistry::new();
        register_into(&registry, path).unwrap();
        let names: Vec<_> = registry.list().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["jobs_create", "jobs_edit", "jobs_list"]);
    }

    #[tokio::test]
    async fn dispatch_through_registry_round_trips() {
        use tool_registry::ToolRegistry;
        let dir = tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        let registry = ToolRegistry::new();
        register_into(&registry, path.clone()).unwrap();

        let out = registry
            .dispatch(
                "jobs_create",
                json!({ "id": "k", "name": "K", "schedule": "* * * * *", "command": "c" }),
            )
            .await;
        assert!(!out.is_error, "{out:?}");

        let out = registry.dispatch("jobs_list", json!({})).await;
        let parsed: Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(parsed["jobs"][0]["id"], "k");
    }
}
