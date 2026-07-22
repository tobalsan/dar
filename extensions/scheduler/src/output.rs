//! Cron run output writer. Renders aihub-shape hybrid frontmatter + readable
//! markdown body and writes it to `cron/output/<job_id>/<timestamp>.md`.
//!
//! Parity gaps vs aihub `output.ts` (documented in README): no `session_id`,
//! no `model`, no `result_status`. Frontmatter keeps the core run fields
//! (job id, run type, fired/finished, status, duration, schedule).

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};

/// Run status written to frontmatter + body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunStatus {
    Ok,
    Error,
}

impl RunStatus {
    fn as_str(self) -> &'static str {
        match self {
            RunStatus::Ok => "ok",
            RunStatus::Error => "error",
        }
    }
}

/// Inputs for one rendered cron run output file.
pub struct CronRunOutput<'a> {
    pub root: &'a Path,
    pub job_id: &'a str,
    pub name: &'a str,
    pub prompt: &'a str,
    pub schedule: &'a str,
    pub fired_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub status: RunStatus,
    /// Assistant response text for an `ok` run.
    pub response: Option<String>,
    /// Error text for an `error` run.
    pub error: Option<String>,
}

impl CronRunOutput<'_> {
    fn duration_ms(&self) -> i64 {
        (self.finished_at - self.fired_at).num_milliseconds().max(0)
    }
}

/// Write the rendered output to `<root>/cron/output/<job_id>/<ts>.md` and
/// return the written path.
pub fn write_cron_run_output(input: &CronRunOutput<'_>) -> Result<PathBuf> {
    let dir = input.root.join("cron").join("output").join(input.job_id);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating cron output dir {}", dir.display()))?;
    let body = render_cron_run_output(input);
    let stem = file_timestamp(input.fired_at);
    for seq in 0..10_000u32 {
        let suffix = if seq == 0 {
            String::new()
        } else {
            format!("-{seq:04}")
        };
        let file_path = dir.join(format!("{stem}{suffix}.md"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&file_path)
        {
            Ok(mut file) => {
                file.write_all(body.as_bytes())
                    .with_context(|| format!("writing cron output {}", file_path.display()))?;
                return Ok(file_path);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("writing cron output {}", file_path.display()));
            }
        }
    }
    anyhow::bail!(
        "could not allocate cron output filename in {}",
        dir.display()
    )
}

/// Render the hybrid frontmatter + markdown body for one run.
pub fn render_cron_run_output(input: &CronRunOutput<'_>) -> String {
    let mut lines: Vec<String> = vec![
        "---".to_string(),
        format!("job_id: {}", yaml_string(input.job_id)),
        "run_type: cron".to_string(),
        format!("fired_at: {}", iso(input.fired_at)),
        format!("finished_at: {}", iso(input.finished_at)),
        format!("status: {}", input.status.as_str()),
        format!("duration_ms: {}", input.duration_ms()),
        format!("schedule: {}", yaml_string(input.schedule)),
        "---".to_string(),
        String::new(),
        format!("# Cron Job: {}", input.name),
        String::new(),
        format!("**Job ID:** {}", input.job_id),
        format!("**Run Time:** {}", display_timestamp(input.fired_at)),
        format!("**Schedule:** {}", input.schedule),
        format!(
            "**Status:** {}",
            match input.status {
                RunStatus::Ok
                    if input
                        .response
                        .as_deref()
                        .is_some_and(|text| text.trim() == "silent tick") =>
                    "ok (silent tick)",
                RunStatus::Ok
                    if input
                        .response
                        .as_deref()
                        .is_some_and(|text| text.starts_with("Gate output:\n")) =>
                    "woke agent",
                RunStatus::Ok => "ok",
                RunStatus::Error => input
                    .error
                    .as_deref()
                    .and_then(|error| error.lines().next())
                    .filter(|line| line.starts_with("script failed (exit "))
                    .unwrap_or("error"),
            }
        ),
        String::new(),
        "## Prompt".to_string(),
        String::new(),
        input.prompt.to_string(),
        String::new(),
    ];

    match input.status {
        RunStatus::Ok => {
            let body = input
                .response
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("[no response]");
            lines.push("## Response".to_string());
            lines.push(String::new());
            lines.push(body.to_string());
        }
        RunStatus::Error => {
            let body = input
                .error
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("[unknown error]");
            lines.push("## Error".to_string());
            lines.push(String::new());
            lines.push("```txt".to_string());
            lines.push(body.to_string());
            lines.push("```".to_string());
        }
    }
    lines.push(String::new());
    lines.join("\n")
}

fn iso(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// `YYYY-MM-DD_HH-mm-ss.sss` (UTC), sortable and collision-resistant.
fn file_timestamp(dt: DateTime<Utc>) -> String {
    dt.format("%Y-%m-%d_%H-%M-%S%.3f").to_string()
}

/// `YYYY-MM-DD HH:mm:ss` (UTC), for the readable body.
fn display_timestamp(dt: DateTime<Utc>) -> String {
    dt.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// JSON-encode a string so YAML frontmatter values are quoted exactly like
/// aihub (`JSON.stringify`).
fn yaml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| format!("{value:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn renders_hybrid_frontmatter_and_markdown_sections() {
        let content = render_cron_run_output(&CronRunOutput {
            root: Path::new("/tmp/agent"),
            job_id: "morning-digest",
            name: "Morning digest",
            prompt: "Summarize overnight events.",
            schedule: "0 8 * * * Europe/Paris",
            fired_at: at("2026-05-19T07:00:00Z"),
            finished_at: at("2026-05-19T07:00:14Z"),
            status: RunStatus::Ok,
            response: Some("Done.".to_string()),
            error: None,
        });

        assert!(content.starts_with("---\njob_id: \"morning-digest\""));
        assert!(content.contains("run_type: cron"));
        assert!(content.contains("fired_at: 2026-05-19T07:00:00.000Z"));
        assert!(content.contains("finished_at: 2026-05-19T07:00:14.000Z"));
        assert!(content.contains("status: ok"));
        assert!(content.contains("duration_ms: 14000"));
        assert!(content.contains("schedule: \"0 8 * * * Europe/Paris\""));
        assert!(content.contains("# Cron Job: Morning digest"));
        assert!(content.contains("**Schedule:** 0 8 * * * Europe/Paris"));
        assert!(content.contains("## Prompt\n\nSummarize overnight events."));
        assert!(content.contains("## Response\n\nDone."));
    }

    #[test]
    fn renders_error_runs() {
        let content = render_cron_run_output(&CronRunOutput {
            root: Path::new("/tmp/agent"),
            job_id: "job-1",
            name: "Job One",
            prompt: "Ping",
            schedule: "* * * * * UTC",
            fired_at: at("2026-05-19T07:00:00Z"),
            finished_at: at("2026-05-19T07:00:01Z"),
            status: RunStatus::Error,
            response: None,
            error: Some("boom".to_string()),
        });
        assert!(content.contains("status: error"));
        assert!(content.contains("## Error"));
        assert!(content.contains("boom"));
    }

    #[test]
    fn empty_response_falls_back_to_placeholder() {
        let content = render_cron_run_output(&CronRunOutput {
            root: Path::new("/tmp/agent"),
            job_id: "job-1",
            name: "Job One",
            prompt: "Ping",
            schedule: "* * * * * UTC",
            fired_at: at("2026-05-19T07:00:00Z"),
            finished_at: at("2026-05-19T07:00:01Z"),
            status: RunStatus::Ok,
            response: Some("   ".to_string()),
            error: None,
        });
        assert!(content.contains("## Response\n\n[no response]"));
    }

    #[test]
    fn writes_timestamped_output_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_cron_run_output(&CronRunOutput {
            root: dir.path(),
            job_id: "job-1",
            name: "Job One",
            prompt: "Ping",
            schedule: "* * * * * UTC",
            fired_at: Utc.with_ymd_and_hms(2026, 5, 19, 7, 0, 0).unwrap(),
            finished_at: Utc.with_ymd_and_hms(2026, 5, 19, 7, 0, 1).unwrap(),
            status: RunStatus::Error,
            response: None,
            error: Some("boom".to_string()),
        })
        .unwrap();

        assert_eq!(
            path,
            dir.path()
                .join("cron/output/job-1/2026-05-19_07-00-00.000.md")
        );
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("## Error"));
        assert!(content.contains("boom"));
    }

    #[test]
    fn writes_collision_safe_output_files() {
        let dir = tempfile::tempdir().unwrap();
        let fired_at = Utc.with_ymd_and_hms(2026, 5, 19, 7, 0, 0).unwrap();
        let input = |error: &str| CronRunOutput {
            root: dir.path(),
            job_id: "job-1",
            name: "Job One",
            prompt: "Ping",
            schedule: "* * * * * UTC",
            fired_at,
            finished_at: fired_at,
            status: RunStatus::Error,
            response: None,
            error: Some(error.to_string()),
        };
        let first = write_cron_run_output(&input("first")).unwrap();
        let second = write_cron_run_output(&input("second")).unwrap();
        assert_ne!(first, second);
        assert!(first.exists());
        assert!(second.exists());
        assert!(second
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("-0001"));
    }
}
