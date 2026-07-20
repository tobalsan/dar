//! Per-agent job store. Loads `cron/jobs.json` under the agent root.
//!
//! Tolerance rules mirror aihub `store.ts`:
//! - missing file → empty (no warning)
//! - malformed file (bad JSON or schema) → one warning, treated as empty
//!
//! The on-disk shape omits `agentId` (single-agent binary: the agent is implied
//! by the root), matching the aihub disk shape.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::schedule::compute_next_run_at_ms;

/// On-disk file version, mirroring the aihub disk shape.
const JOBS_FILE_VERSION: u32 = 1;

/// Cron + IANA timezone + optional ISO `startAt` anchor.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Schedule {
    pub cron: String,
    pub tz: String,
    #[serde(rename = "startAt", skip_serializing_if = "Option::is_none", default)]
    pub start_at: Option<String>,
}

/// Prompt payload. Only `message` is used by the walking skeleton; `sessionId`
/// is a documented parity gap (no session continuity yet).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Payload {
    pub message: String,
}

/// One scheduled job. This is the on-disk shape: it holds *only* configuration.
/// Runtime state (next/last run, status, running-for) is never serialized here —
/// it lives in-memory ([`crate::state`]) and is merged into list responses.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ScheduleJob {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub schedule: Schedule,
    pub payload: Payload,
    /// Per-job timeout override (ms). When set, takes precedence over the
    /// extension-level `jobTimeoutMs` and the 10-minute default. `null`/absent
    /// means "inherit the extension/global default". Omitted from disk when
    /// `None` so the persisted shape stays config-only.
    #[serde(rename = "timeoutMs", skip_serializing_if = "Option::is_none", default)]
    pub timeout_ms: Option<u64>,
}

fn default_enabled() -> bool {
    true
}

impl Schedule {
    pub(crate) fn validate(&self, now_ms: i64) -> Result<(), String> {
        if self.cron.trim().is_empty() {
            return Err("cron must not be empty".to_string());
        }
        compute_next_run_at_ms(self, now_ms)
            .map(|_| ())
            .map_err(|err| format!("bad schedule: {err:#}"))
    }
}

impl Payload {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.message.trim().is_empty() {
            return Err("payload.message must not be empty".to_string());
        }
        Ok(())
    }
}

impl ScheduleJob {
    pub(crate) fn validate(&self, now_ms: i64) -> Result<(), String> {
        if !is_safe_job_id(&self.id) {
            return Err(format!("unsafe job id {:?}", self.id));
        }
        self.schedule.validate(now_ms)?;
        self.payload.validate()?;
        if self.timeout_ms == Some(0) {
            return Err("timeoutMs must be greater than 0".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct JobsFile {
    #[serde(default = "default_version")]
    version: u32,
    #[serde(default)]
    jobs: Vec<ScheduleJob>,
}

fn default_version() -> u32 {
    JOBS_FILE_VERSION
}

/// `<root>/cron/jobs.json`.
pub fn jobs_path(root: &Path) -> PathBuf {
    root.join("cron").join("jobs.json")
}

/// Load enabled-and-disabled jobs from `<root>/cron/jobs.json`.
///
/// `warn` is invoked at most once, only for a malformed file. A missing file is
/// silently treated as empty.
pub fn load_jobs(root: &Path, mut warn: impl FnMut(String)) -> Vec<ScheduleJob> {
    let path = jobs_path(root);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            warn(format!(
                "[scheduler] Failed to read {}; treating as empty: {err}",
                path.display()
            ));
            return Vec::new();
        }
    };

    let jobs = match serde_json::from_str::<JobsFile>(&raw) {
        Ok(file) => file.jobs,
        Err(err) => {
            warn(format!(
                "[scheduler] Invalid {}; treating as empty: {err}",
                path.display()
            ));
            return Vec::new();
        }
    };

    let now_ms = chrono::Utc::now().timestamp_millis();
    if let Some((job, err)) = jobs
        .iter()
        .find_map(|job| job.validate(now_ms).err().map(|err| (job, err)))
    {
        warn(format!(
            "[scheduler] Invalid {}; treating as empty: job {:?}: {err}",
            path.display(),
            job.id
        ));
        return Vec::new();
    }

    jobs
}

/// Atomically persist `jobs` to `<root>/cron/jobs.json` using a temp file +
/// rename so a concurrent reader (or a crash mid-write) never observes a
/// partially written file. Only configuration is written — runtime state lives
/// in memory and never reaches disk. The `cron/` directory is created if absent.
pub fn save_jobs(root: &Path, jobs: &[ScheduleJob]) -> std::io::Result<()> {
    let path = jobs_path(root);
    let dir = path.parent().expect("jobs_path always has a parent");
    std::fs::create_dir_all(dir)?;

    let file = JobsFile {
        version: JOBS_FILE_VERSION,
        jobs: jobs.to_vec(),
    };
    let mut body = serde_json::to_string_pretty(&file)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    body.push('\n');

    // Write to a unique temp file in the same directory (same filesystem, so
    // the rename is atomic) then rename over the target. The name combines the
    // pid with a process-wide counter so two concurrent saves never share a
    // temp path and clobber each other's bytes.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".jobs.json.{}.{seq}.tmp", std::process::id()));
    std::fs::write(&tmp, body.as_bytes())?;
    match std::fs::rename(&tmp, &path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = std::fs::remove_file(&tmp);
            Err(err)
        }
    }
}

/// Generate a unique, filesystem-safe job id not colliding with any existing
/// one. Shared by the HTTP create handler and the `scheduler_create_job` tool so
/// both mint ids the same way.
pub fn generate_job_id(existing: &[ScheduleJob]) -> String {
    let base = chrono::Utc::now().timestamp_millis();
    let mut n = base;
    loop {
        let candidate = format!("job-{n}");
        if is_safe_job_id(&candidate) && !existing.iter().any(|j| j.id == candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// A job id must be a single, non-empty path component with no separators,
/// parent-traversal, or leading dot, so it is always contained under
/// `cron/output/`.
pub fn is_safe_job_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('.')
        && !id.contains('/')
        && !id.contains('\\')
        && Path::new(id).components().count() == 1
        && Path::new(id)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn collect_warnings(root: &Path) -> (Vec<ScheduleJob>, Vec<String>) {
        let warnings = RefCell::new(Vec::new());
        let jobs = load_jobs(root, |m| warnings.borrow_mut().push(m));
        (jobs, warnings.into_inner())
    }

    #[test]
    fn missing_file_is_empty_without_warning() {
        let dir = tempfile::tempdir().unwrap();
        let (jobs, warnings) = collect_warnings(dir.path());
        assert!(jobs.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn malformed_file_warns_once_and_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("cron")).unwrap();
        std::fs::write(jobs_path(dir.path()), "not json").unwrap();
        let (jobs, warnings) = collect_warnings(dir.path());
        assert!(jobs.is_empty());
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn loads_jobs_and_synthesizes_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("cron")).unwrap();
        std::fs::write(
            jobs_path(dir.path()),
            r#"{
                "version": 1,
                "jobs": [
                    {
                        "id": "morning-digest",
                        "name": "Morning digest",
                        "enabled": true,
                        "schedule": { "cron": "0 8 * * *", "tz": "Europe/Paris", "startAt": "2026-05-19T07:00:00.000Z" },
                        "payload": { "message": "Summarize overnight events." },
                        "timeoutMs": 120000
                    },
                    {
                        "id": "no-name",
                        "schedule": { "cron": "*/5 * * * *", "tz": "UTC" },
                        "payload": { "message": "tick" }
                    }
                ]
            }"#,
        )
        .unwrap();
        let (jobs, warnings) = collect_warnings(dir.path());
        assert!(warnings.is_empty());
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].id, "morning-digest");
        assert_eq!(
            jobs[0].schedule.start_at.as_deref(),
            Some("2026-05-19T07:00:00.000Z")
        );
        assert_eq!(jobs[0].payload.message, "Summarize overnight events.");
        assert_eq!(jobs[0].timeout_ms, Some(120000));
        // Defaults: missing name → empty, missing enabled → true, missing
        // timeoutMs → None (inherit the extension/global default).
        assert_eq!(jobs[1].name, "");
        assert!(jobs[1].enabled);
        assert_eq!(jobs[1].timeout_ms, None);
    }

    #[test]
    fn empty_jobs_array_is_empty_without_warning() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("cron")).unwrap();
        std::fs::write(jobs_path(dir.path()), r#"{ "version": 1, "jobs": [] }"#).unwrap();
        let (jobs, warnings) = collect_warnings(dir.path());
        assert!(jobs.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn invalid_job_makes_whole_file_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("cron")).unwrap();
        std::fs::write(
            jobs_path(dir.path()),
            r#"{
                "jobs": [
                    { "id": "../escape", "schedule": { "cron": "* * * * *", "tz": "UTC" }, "payload": { "message": "x" } },
                    { "id": "a/b", "schedule": { "cron": "* * * * *", "tz": "UTC" }, "payload": { "message": "x" } },
                    { "id": "", "schedule": { "cron": "* * * * *", "tz": "UTC" }, "payload": { "message": "x" } },
                    { "id": "good-job", "schedule": { "cron": "* * * * *", "tz": "UTC" }, "payload": { "message": "x" } }
                ]
            }"#,
        )
        .unwrap();
        let (jobs, warnings) = collect_warnings(dir.path());
        assert!(jobs.is_empty());
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn semantic_invalid_jobs_make_whole_file_empty_with_one_warning() {
        let cases = [
            (
                "bad-cron",
                r#"{ "id": "bad-cron", "schedule": { "cron": "not cron", "tz": "UTC" }, "payload": { "message": "x" } }"#,
            ),
            (
                "bad-timezone",
                r#"{ "id": "bad-timezone", "schedule": { "cron": "* * * * *", "tz": "Mars/Base" }, "payload": { "message": "x" } }"#,
            ),
            (
                "bad-start-at",
                r#"{ "id": "bad-start-at", "schedule": { "cron": "* * * * *", "tz": "UTC", "startAt": "tomorrow" }, "payload": { "message": "x" } }"#,
            ),
            (
                "empty-message",
                r#"{ "id": "empty-message", "schedule": { "cron": "* * * * *", "tz": "UTC" }, "payload": { "message": "" } }"#,
            ),
            (
                "zero-timeout",
                r#"{ "id": "zero-timeout", "schedule": { "cron": "* * * * *", "tz": "UTC" }, "payload": { "message": "x" }, "timeoutMs": 0 }"#,
            ),
            (
                "mixed-valid-invalid",
                r#"{ "id": "valid", "schedule": { "cron": "* * * * *", "tz": "UTC" }, "payload": { "message": "x" } }, { "id": "invalid", "schedule": { "cron": "bad", "tz": "UTC" }, "payload": { "message": "x" } }"#,
            ),
        ];
        for (name, jobs_json) in cases {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dir.path().join("cron")).unwrap();
            std::fs::write(
                jobs_path(dir.path()),
                format!(r#"{{ "jobs": [ {jobs_json} ] }}"#),
            )
            .unwrap();
            let (jobs, warnings) = collect_warnings(dir.path());
            assert!(jobs.is_empty(), "{name}");
            assert_eq!(warnings.len(), 1, "{name}");
        }
    }

    #[test]
    fn is_safe_job_id_rules() {
        assert!(is_safe_job_id("morning-digest"));
        assert!(is_safe_job_id("job_1"));
        assert!(!is_safe_job_id(""));
        assert!(!is_safe_job_id(".."));
        assert!(!is_safe_job_id("../x"));
        assert!(!is_safe_job_id("a/b"));
        assert!(!is_safe_job_id("a\\b"));
        assert!(!is_safe_job_id("/abs"));
        assert!(!is_safe_job_id(".hidden"));
    }

    fn sample_job(id: &str) -> ScheduleJob {
        ScheduleJob {
            id: id.to_string(),
            name: "Sample".to_string(),
            enabled: true,
            schedule: Schedule {
                cron: "0 8 * * *".to_string(),
                tz: "Europe/Paris".to_string(),
                start_at: Some("2026-05-19T07:00:00.000Z".to_string()),
            },
            payload: Payload {
                message: "do the thing".to_string(),
            },
            timeout_ms: Some(60_000),
        }
    }

    #[test]
    fn timeout_validation_accepts_inheritance_and_positive_values() {
        let mut job = sample_job("timeout");
        assert!(job.validate(chrono::Utc::now().timestamp_millis()).is_ok());
        job.timeout_ms = None;
        assert!(job.validate(chrono::Utc::now().timestamp_millis()).is_ok());
        job.timeout_ms = Some(0);
        assert_eq!(
            job.validate(chrono::Utc::now().timestamp_millis())
                .unwrap_err(),
            "timeoutMs must be greater than 0"
        );
    }

    #[test]
    fn save_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let jobs = vec![sample_job("a"), {
            let mut j = sample_job("b");
            j.schedule.start_at = None;
            j.timeout_ms = None;
            j.enabled = false;
            j
        }];
        save_jobs(dir.path(), &jobs).unwrap();
        let (loaded, warnings) = collect_warnings(dir.path());
        assert!(warnings.is_empty());
        assert_eq!(loaded, jobs);
    }

    #[test]
    fn save_omits_runtime_state_and_writes_version() {
        let dir = tempfile::tempdir().unwrap();
        save_jobs(dir.path(), &[sample_job("a")]).unwrap();
        let raw = std::fs::read_to_string(jobs_path(dir.path())).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // Disk shape: version + jobs, each job is config-only.
        assert_eq!(value["version"], 1);
        let job = &value["jobs"][0];
        let keys: Vec<&str> = job
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        for key in &keys {
            assert!(
                matches!(
                    *key,
                    "id" | "name" | "enabled" | "schedule" | "payload" | "timeoutMs"
                ),
                "unexpected disk key {key:?}"
            );
        }
        // No runtime-state fields ever persisted.
        for forbidden in [
            "nextRunAt",
            "lastRunAt",
            "lastStatus",
            "status",
            "runningForMs",
            "agentId",
        ] {
            assert!(
                job.get(forbidden).is_none(),
                "{forbidden} must not be on disk"
            );
        }
    }

    #[test]
    fn save_omits_absent_optional_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut job = sample_job("a");
        job.schedule.start_at = None;
        job.timeout_ms = None;
        save_jobs(dir.path(), &[job]).unwrap();
        let raw = std::fs::read_to_string(jobs_path(dir.path())).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let j = &value["jobs"][0];
        assert!(j.get("timeoutMs").is_none());
        assert!(j["schedule"].get("startAt").is_none());
    }

    #[test]
    fn save_is_atomic_no_temp_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        save_jobs(dir.path(), &[sample_job("a")]).unwrap();
        // After a successful save only jobs.json exists in cron/ — no stray temp.
        let entries: Vec<String> = std::fs::read_dir(dir.path().join("cron"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["jobs.json".to_string()]);
    }

    #[test]
    fn save_creates_cron_dir() {
        let dir = tempfile::tempdir().unwrap();
        // cron/ does not exist yet.
        assert!(!dir.path().join("cron").exists());
        save_jobs(dir.path(), &[sample_job("a")]).unwrap();
        assert!(jobs_path(dir.path()).exists());
    }

    #[test]
    fn save_overwrites_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        save_jobs(dir.path(), &[sample_job("a"), sample_job("b")]).unwrap();
        save_jobs(dir.path(), &[sample_job("c")]).unwrap();
        let (loaded, _) = collect_warnings(dir.path());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "c");
    }

    #[test]
    fn missing_required_field_is_malformed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("cron")).unwrap();
        // job missing `payload` → schema error → warn + empty.
        std::fs::write(
            jobs_path(dir.path()),
            r#"{ "jobs": [ { "id": "x", "schedule": { "cron": "* * * * *", "tz": "UTC" } } ] }"#,
        )
        .unwrap();
        let (jobs, warnings) = collect_warnings(dir.path());
        assert!(jobs.is_empty());
        assert_eq!(warnings.len(), 1);
    }
}
