//! Read-only "Cron" dashboard tab (ALG-225).
//!
//! Contributes one [`DashboardTab`] to the web dashboard via the
//! `cap-dashboard-tab` contract (ALG-220). It renders an HTML *fragment* the
//! dashboard splices into its `#content` shell, showing each job's schedule +
//! timezone, enabled flag, next/last run times, last status (with the error
//! message when the last run failed), running-for, and the recent output files
//! per job.
//!
//! The tab is **read-only**: it renders state and links to output files but has
//! no mutation controls. Mutation stays with the scheduler HTTP API
//! ([`crate::http`]) and direct edits to `cron/jobs.json`.
//!
//! It reads the same shared [`SchedulerState`] the timer loop and HTTP API use,
//! so the view always reflects live runtime state. Refresh is the dashboard's
//! existing self-poll: when the Cron tab is active the dashboard re-fetches its
//! fragment into `#content` on the shared cadence (the same poller the
//! orchestrator run view uses), so no inner poll is needed here.
//!
//! Cron activity is surfaced *here*, at the scheduler's own level — scheduled
//! runs never reach the orchestrator's `RunSnapshot` or its run list (the
//! scheduler fires the runner service directly with a `NullSink` capture store
//! and never publishes to the orchestrator's run topics).

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use cap_dashboard_tab::{escape_html, DashboardTab};
use chrono::{TimeZone, Utc};

use crate::schedule::format_schedule;
use crate::state::{JobRuntime, LastStatus, SchedulerState};

/// Stable tab id (URL path segment under `/tabs/{id}` and htmx target
/// discriminator) and human label.
const TAB_ID: &str = "cron";
const TAB_TITLE: &str = "Cron";

/// Max number of recent output files listed per job.
const RECENT_OUTPUTS: usize = 5;

/// Read-only Cron dashboard tab. Holds the shared scheduler state plus the agent
/// root so it can list recent output files at render time.
pub struct CronTab {
    state: Arc<SchedulerState>,
    root: std::path::PathBuf,
}

impl CronTab {
    pub fn new(state: Arc<SchedulerState>, root: std::path::PathBuf) -> Self {
        Self { state, root }
    }
}

impl DashboardTab for CronTab {
    fn id(&self) -> &str {
        TAB_ID
    }

    fn title(&self) -> &str {
        TAB_TITLE
    }

    fn render(&self) -> Result<String> {
        let now_ms = Utc::now().timestamp_millis();
        let jobs = self.state.jobs();
        let rows: Vec<JobRow> = jobs
            .iter()
            .map(|job| {
                let rt = self.state.runtime(&job.id);
                let outputs = recent_outputs(&self.root, &job.id, RECENT_OUTPUTS);
                JobRow::build(
                    &job.id,
                    &job.name,
                    &format_schedule(&job.schedule),
                    job.enabled,
                    &rt,
                    now_ms,
                    outputs,
                )
            })
            .collect();
        Ok(render_fragment(&rows))
    }
}

/// One job row prepared for rendering. All fields are pre-formatted and
/// HTML-escaped strings so the fragment template is a straight concat.
struct JobRow {
    name: String,
    schedule: String,
    enabled: bool,
    next_run: String,
    last_run: String,
    status_label: String,
    status_class: &'static str,
    error: Option<String>,
    running_for: Option<String>,
    outputs: Vec<String>,
}

impl JobRow {
    fn build(
        id: &str,
        name: &str,
        schedule: &str,
        enabled: bool,
        rt: &JobRuntime,
        now_ms: i64,
        outputs: Vec<String>,
    ) -> Self {
        let display_name = if name.trim().is_empty() { id } else { name };
        let (status_label, status_class) = match rt.last_status {
            Some(LastStatus::Ok) => ("ok", "completed"),
            Some(LastStatus::Error) => ("error", "failed"),
            None => ("never run", "other"),
        };
        let running_for = rt
            .running_since_ms
            .map(|since| fmt_duration((now_ms - since).max(0)));
        JobRow {
            name: escape_html(display_name),
            schedule: escape_html(schedule),
            enabled,
            next_run: fmt_instant(rt.next_run_at_ms),
            last_run: fmt_instant(rt.last_run_at_ms),
            status_label: status_label.to_string(),
            status_class,
            error: rt
                .last_error
                .as_deref()
                .filter(|e| !e.trim().is_empty())
                .map(escape_html),
            running_for,
            outputs: outputs.iter().map(|o| escape_html(o)).collect(),
        }
    }
}

/// Render the tab body as a self-contained HTML fragment. The dashboard's shared
/// `#content` poller re-fetches the active tab on its cadence, so the fragment
/// carries no inner poll of its own (matching the orchestrator run view and the
/// example tab, which rely on the same shared poller).
fn render_fragment(rows: &[JobRow]) -> String {
    let mut html = String::new();
    html.push_str("<main>");
    html.push_str("<section class=\"panel\"><h2>Cron jobs</h2>");
    if rows.is_empty() {
        html.push_str("<p class=\"empty\">No cron jobs configured.</p>");
    } else {
        html.push_str("<div class=\"run-list\">");
        for row in rows {
            html.push_str(&render_job(row));
        }
        html.push_str("</div>");
    }
    html.push_str("</section></main>");
    html
}

fn render_job(row: &JobRow) -> String {
    let enabled = if row.enabled {
        "<span class=\"pill completed\">enabled</span>"
    } else {
        "<span class=\"pill other\">disabled</span>"
    };
    let running = row
        .running_for
        .as_deref()
        .map(|d| format!("<span class=\"pill live\">running {d}</span>"))
        .unwrap_or_default();

    let mut block = String::new();
    block.push_str("<div class=\"run-row\">");
    block.push_str(&format!(
        "<div class=\"run-title\"><strong>{}</strong> {enabled} {running}</div>",
        row.name
    ));
    block.push_str(&format!("<div class=\"meta\">{}</div>", row.schedule));
    block.push_str(&format!(
        "<div class=\"meta\">Next run: {} &middot; Last run: {} &middot; Last status: <span class=\"pill {}\">{}</span></div>",
        row.next_run, row.last_run, row.status_class, row.status_label
    ));
    if let Some(err) = &row.error {
        block.push_str(&format!(
            "<div class=\"meta\" style=\"color:var(--bad)\">Error: {err}</div>"
        ));
    }
    if row.outputs.is_empty() {
        block.push_str("<div class=\"meta\">No outputs yet.</div>");
    } else {
        block.push_str("<div class=\"meta\">Recent outputs:");
        block.push_str("<ul style=\"margin:.2rem 0 0;padding-left:1.1rem\">");
        for out in &row.outputs {
            block.push_str(&format!("<li>{out}</li>"));
        }
        block.push_str("</ul></div>");
    }
    block.push_str("</div>");
    block
}

/// List the most recent output filenames for a job, newest first. Filenames are
/// `YYYY-MM-DD_HH-mm-ss.md`, so lexical sort is chronological. Missing dir or
/// read errors yield an empty list (the tab is read-only and tolerant).
fn recent_outputs(root: &Path, job_id: &str, limit: usize) -> Vec<String> {
    let dir = root.join("cron").join("output").join(job_id);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".md"))
        .collect();
    names.sort();
    names.reverse();
    names.truncate(limit);
    names
}

/// Format a UTC epoch-millis instant as `YYYY-MM-DD HH:mm:ss` (UTC), or `—`
/// when absent.
fn fmt_instant(ms: Option<i64>) -> String {
    match ms.and_then(|ms| Utc.timestamp_millis_opt(ms).single()) {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        None => "\u{2014}".to_string(),
    }
}

/// Format a positive duration in milliseconds as a coarse `Ns`/`Nm`/`Nh`/`Nd`.
fn fmt_duration(ms: i64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Payload, Schedule, ScheduleJob};

    fn job(id: &str, name: &str, enabled: bool) -> ScheduleJob {
        ScheduleJob {
            id: id.to_string(),
            name: name.to_string(),
            enabled,
            schedule: Schedule {
                cron: "0 8 * * *".to_string(),
                tz: "Europe/Paris".to_string(),
                start_at: None,
            },
            payload: Payload {
                message: "hi".to_string(),
            },
            timeout_ms: None,
        }
    }

    fn tab_with(state: Arc<SchedulerState>, root: std::path::PathBuf) -> CronTab {
        CronTab::new(state, root)
    }

    #[test]
    fn tab_id_and_title_are_stable() {
        let dir = tempfile::tempdir().unwrap();
        let tab = tab_with(
            Arc::new(SchedulerState::new(vec![])),
            dir.path().to_path_buf(),
        );
        assert_eq!(tab.id(), "cron");
        assert_eq!(tab.title(), "Cron");
    }

    #[test]
    fn empty_state_renders_empty_fragment() {
        let dir = tempfile::tempdir().unwrap();
        let tab = tab_with(
            Arc::new(SchedulerState::new(vec![])),
            dir.path().to_path_buf(),
        );
        let html = tab.render().unwrap();
        assert!(html.contains("No cron jobs configured"));
        // It is a fragment, not a full page; the dashboard's shared #content
        // poller drives refresh, so the fragment declares no inner poll.
        assert!(!html.contains("<body"));
        assert!(!html.contains("hx-get"), "no inner poll: {html}");
    }

    #[test]
    fn renders_schedule_enabled_next_last_status_and_running() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(SchedulerState::new(vec![job(
            "digest",
            "Morning digest",
            true,
        )]));
        state.set_next_run(
            "digest",
            Some(
                Utc.with_ymd_and_hms(2026, 6, 14, 6, 0, 0)
                    .unwrap()
                    .timestamp_millis(),
            ),
        );
        assert!(state.try_claim_running("digest", Utc::now().timestamp_millis()));
        state.mark_finished("digest", LastStatus::Ok, None);
        let tab = tab_with(state, dir.path().to_path_buf());
        let html = tab.render().unwrap();
        assert!(html.contains("Morning digest"), "job name shown: {html}");
        assert!(
            html.contains("0 8 * * * Europe/Paris"),
            "schedule + tz shown"
        );
        assert!(html.contains(">enabled<"), "enabled flag shown");
        assert!(html.contains("2026-06-14 06:00:00 UTC"), "next run shown");
        assert!(
            html.contains("Last status: <span class=\"pill completed\">ok"),
            "ok status shown"
        );
    }

    #[test]
    fn failed_run_shows_error_message() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(SchedulerState::new(vec![job("j", "Job", true)]));
        assert!(state.try_claim_running("j", Utc::now().timestamp_millis()));
        state.mark_finished(
            "j",
            LastStatus::Error,
            Some("runner exited abnormally".to_string()),
        );
        let tab = tab_with(state, dir.path().to_path_buf());
        let html = tab.render().unwrap();
        assert!(
            html.contains("class=\"pill failed\">error"),
            "error status shown"
        );
        assert!(
            html.contains("runner exited abnormally"),
            "error message shown"
        );
    }

    #[test]
    fn disabled_job_renders_disabled_pill() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(SchedulerState::new(vec![job("j", "Job", false)]));
        let tab = tab_with(state, dir.path().to_path_buf());
        let html = tab.render().unwrap();
        assert!(html.contains(">disabled<"), "disabled flag shown");
        assert!(html.contains("never run"), "never-run status shown");
    }

    #[test]
    fn lists_recent_outputs_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("cron").join("output").join("j");
        std::fs::create_dir_all(&out).unwrap();
        for ts in [
            "2026-06-14_06-00-00",
            "2026-06-14_07-00-00",
            "2026-06-14_08-00-00",
        ] {
            std::fs::write(out.join(format!("{ts}.md")), "x").unwrap();
        }
        // A non-md stray must be ignored.
        std::fs::write(out.join("notes.txt"), "x").unwrap();
        let state = Arc::new(SchedulerState::new(vec![job("j", "Job", true)]));
        let tab = tab_with(state, dir.path().to_path_buf());
        let html = tab.render().unwrap();
        let newest = html.find("2026-06-14_08-00-00").unwrap();
        let oldest = html.find("2026-06-14_06-00-00").unwrap();
        assert!(newest < oldest, "newest output listed first");
        assert!(!html.contains("notes.txt"), "non-md files excluded");
    }

    #[test]
    fn html_in_job_name_is_escaped() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(SchedulerState::new(vec![job(
            "j",
            "<script>alert(1)</script>",
            true,
        )]));
        let tab = tab_with(state, dir.path().to_path_buf());
        let html = tab.render().unwrap();
        assert!(
            !html.contains("<script>alert"),
            "raw script not injected: {html}"
        );
        assert!(html.contains("&lt;script&gt;"), "name escaped");
    }

    #[test]
    fn recent_outputs_missing_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(recent_outputs(dir.path(), "nope", 5).is_empty());
    }

    #[test]
    fn recent_outputs_caps_at_limit() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("cron").join("output").join("j");
        std::fs::create_dir_all(&out).unwrap();
        for i in 0..10 {
            std::fs::write(out.join(format!("2026-06-14_06-00-{i:02}.md")), "x").unwrap();
        }
        assert_eq!(recent_outputs(dir.path(), "j", 5).len(), 5);
    }
}
