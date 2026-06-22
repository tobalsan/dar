//! Per-agent job store. Loads `cron/jobs.json` under the agent root.
//!
//! Tolerance rules mirror aihub `store.ts`:
//! - missing file → empty (no warning)
//! - malformed file (bad JSON or schema) → one warning, treated as empty
//!
//! The on-disk shape omits `agentId` (single-agent binary: the agent is implied
//! by the root), matching the aihub disk shape.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Cron + IANA timezone + optional ISO `startAt` anchor.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Schedule {
    pub cron: String,
    pub tz: String,
    #[serde(rename = "startAt", default)]
    pub start_at: Option<String>,
}

/// Prompt payload. Only `message` is used by the walking skeleton; `sessionId`
/// is a documented parity gap (no session continuity yet).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Payload {
    pub message: String,
}

/// One scheduled job as loaded from disk.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ScheduleJob {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub schedule: Schedule,
    pub payload: Payload,
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct JobsFile {
    #[serde(default)]
    jobs: Vec<ScheduleJob>,
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

    // Job ids become filesystem path segments (`cron/output/<job_id>/`). Reject
    // any id that is not a single safe path segment so a crafted id cannot
    // escape the agent root.
    jobs.into_iter()
        .filter(|job| {
            if is_safe_job_id(&job.id) {
                true
            } else {
                warn(format!(
                    "[scheduler] Skipping job with unsafe id {:?} in {}",
                    job.id,
                    path.display()
                ));
                false
            }
        })
        .collect()
}

/// A job id must be a single, non-empty path component with no separators,
/// parent-traversal, or leading dot, so it is always contained under
/// `cron/output/`.
fn is_safe_job_id(id: &str) -> bool {
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
                        "payload": { "message": "Summarize overnight events." }
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
        // Defaults: missing name → empty, missing enabled → true.
        assert_eq!(jobs[1].name, "");
        assert!(jobs[1].enabled);
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
    fn skips_jobs_with_unsafe_ids() {
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
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, "good-job");
        assert_eq!(warnings.len(), 3);
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
