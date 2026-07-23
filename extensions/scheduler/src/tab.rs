//! "Cron" dashboard tab (ALG-225, redesigned).
//!
//! Contributes one [`DashboardTab`] to the web dashboard via the
//! `cap-dashboard-tab` contract (ALG-220). It renders an HTML *fragment* the
//! dashboard splices into its `#content` shell as a full-width, uniform-height
//! job list: title + status/running/disabled pills, controls (Run now,
//! Enable/Disable), humanized schedule, a one-line next/last summary, an error
//! line on failure, a one-line clamped prompt preview, and up to
//! [`RECENT_OUTPUTS`] recent output files per job. The job name and, when a
//! job has more outputs than shown, a trailing "all N outputs" line both open
//! a full job-detail drawer ([`crate::http`]'s `/detail` endpoint) with the
//! complete prompt and output history — kept out of the row itself so every
//! row stays the same height regardless of how much a job has produced.
//!
//! Mutations do not go through this trait: the "Run now" and enable/disable
//! controls fire plain `fetch()` calls against the scheduler's own HTTP API
//! ([`crate::http`]), which is the single source of truth for job mutation.
//! Direct edits to `cron/jobs.json` remain supported too. This module only
//! renders state and wires those calls; it holds no mutation logic itself.
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
use chrono::{NaiveDateTime, TimeZone, Utc};

use crate::state::{JobRuntime, LastStatus, SchedulerState};
use crate::store::ScheduleJob;

/// Stable tab id (URL path segment under `/tabs/{id}` and htmx target
/// discriminator) and human label.
const TAB_ID: &str = "cron";
const TAB_TITLE: &str = "Cron";

/// Max number of recent output files listed per job row. Kept small so every
/// row stays the same height regardless of how much a job has produced; the
/// full history is one click away in the job-detail drawer.
const RECENT_OUTPUTS: usize = 3;

/// Cron-scoped CSS. Prefixed `cron-` so the dashboard shell stays ignorant of
/// cron specifics; re-rendered with every poll, which is fine since it is
/// idempotent. Reuses the dashboard's shared tokens (`--muted`, `--border`,
/// `--accent`, `--bad`, `--panel`, ...) and pill vocabulary (`.pill`,
/// `.completed`, `.failed`, `.other`, `.live`) rather than introducing new
/// colors or nested `.panel` cards. The job-detail and output drawers
/// (rendered by [`crate::http`]) reuse several of these classes too — safe
/// because the only way to reach them is a click originating from this
/// fragment, so this `<style>` is already present in the DOM by then.
const CRON_STYLE: &str = "<style>\
.cron-job { display: grid; grid-template-columns: minmax(0, 1.3fr) minmax(0, 1fr); gap: 1.1rem 2rem; padding: 1.1rem 0; border-top: 1px solid var(--border); }\
.cron-job:first-child { border-top: none; padding-top: 0; }\
.cron-info { min-width: 0; display: grid; gap: .55rem; align-content: start; }\
.cron-title-line { display: flex; align-items: center; gap: .4rem; flex-wrap: wrap; }\
.cron-name { font-size: .95rem; }\
.cron-name-link { cursor: pointer; }\
.cron-name-link:hover { color: var(--accent); }\
.cron-controls { display: flex; gap: .35rem; }\
.cron-btn { font-size: .78rem; padding: .3rem .55rem; }\
.cron-schedule-line { display: flex; align-items: baseline; gap: .5rem; flex-wrap: wrap; }\
.cron-schedule-human { color: var(--fg); }\
.cron-schedule-raw { color: var(--muted); font-size: .72rem; }\
.cron-meta { color: var(--muted); font-size: .78rem; }\
.cron-error { color: var(--bad); font-size: .82rem; }\
.cron-prompt { color: var(--muted); font-size: .8rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }\
.cron-outputs { min-width: 0; display: grid; gap: .3rem; align-content: start; }\
.cron-outputs-head { color: var(--muted); text-transform: uppercase; font-size: .68rem; letter-spacing: .06em; }\
.cron-output-row { display: flex; justify-content: space-between; gap: .5rem; padding: .35rem .5rem; border: 1px solid transparent; border-radius: 5px; cursor: pointer; }\
.cron-output-row:hover { border-color: var(--accent); }\
.cron-output-label { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }\
.cron-output-file { color: var(--muted); font-size: .68rem; white-space: nowrap; }\
.cron-output-empty { margin: 0; }\
.cron-output-more { color: var(--muted); font-size: .72rem; cursor: pointer; padding: .3rem .5rem; }\
.cron-output-more:hover { color: var(--accent); }\
.cron-empty-hint { color: var(--muted); font-style: italic; margin: .2rem 0 0; font-size: .85rem; }\
@media (max-width: 760px) { .cron-job { grid-template-columns: 1fr; } }\
</style>";

/// Cron dashboard tab. Holds the shared scheduler state plus the agent root so
/// it can list recent output files at render time.
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
                let all_outputs = list_output_files(&self.root, &job.id);
                let total_outputs = all_outputs.len();
                let outputs = all_outputs
                    .iter()
                    .take(RECENT_OUTPUTS)
                    .map(|name| OutputEntry::build(name))
                    .collect();
                JobRow::build(job, &rt, now_ms, outputs, total_outputs)
            })
            .collect();
        Ok(render_fragment(&rows))
    }
}

/// One recent output file ready for rendering. Both fields are pre-escaped.
/// `pub(crate)` so the job-detail drawer ([`crate::http`]) can reuse the same
/// label/escaping logic for its complete (unlimited) output listing.
pub(crate) struct OutputEntry {
    /// Humanized timestamp label, or the raw filename when it doesn't parse.
    pub(crate) label: String,
    /// Raw filename, used both as the `hx-get` path segment and as a secondary
    /// muted label next to `label`.
    pub(crate) filename: String,
}

impl OutputEntry {
    pub(crate) fn build(name: &str) -> Self {
        let label = humanize_output_name(name).unwrap_or_else(|| name.to_string());
        OutputEntry {
            label: escape_html(&label),
            filename: escape_html(name),
        }
    }
}

/// One job row prepared for rendering. All fields are pre-formatted,
/// pre-escaped display strings (the render functions do zero escaping of their
/// own) except for the plain enums/bools used only to pick a CSS class or
/// branch on presence.
struct JobRow {
    id: String,
    name: String,
    enabled: bool,
    status_label: String,
    status_class: &'static str,
    running_for: Option<String>,
    schedule_human: String,
    schedule_raw: String,
    /// Fully-formed one-line next/last summary, e.g. `"<span title="...">next
    /// in 3h 12m</span> · <span title="...">last ok 12h ago</span>"`. The
    /// absolute UTC instants live only in each span's `title` tooltip; when an
    /// instant is absent that span carries no `title` and no dangling dash.
    meta_line: String,
    error: Option<String>,
    /// Full job prompt, HTML-escaped. Shown as a single clamped line in the
    /// row (ellipsis + `title` tooltip) and in full in the job-detail drawer.
    message: String,
    delivery: Option<String>,
    outputs: Vec<OutputEntry>,
    /// Total number of output files for this job (may exceed `outputs.len()`,
    /// which is capped at [`RECENT_OUTPUTS`]).
    total_outputs: usize,
}

impl JobRow {
    fn build(
        job: &ScheduleJob,
        rt: &JobRuntime,
        now_ms: i64,
        outputs: Vec<OutputEntry>,
        total_outputs: usize,
    ) -> Self {
        let display_name = if job.name.trim().is_empty() {
            &job.id
        } else {
            &job.name
        };
        let (status_label, status_class) = status_label_and_class(rt.last_status);
        let running_for = rt
            .running_since_ms
            .map(|since| fmt_duration((now_ms - since).max(0)));
        let schedule_human = humanize_cron(&job.schedule.cron)
            .unwrap_or_else(|| format!("{} · {}", job.schedule.cron, job.schedule.tz));
        let next_part = match rt.next_run_at_ms {
            Some(ms) => format!(
                "<span title=\"{}\">next {}</span>",
                fmt_instant(Some(ms)),
                fmt_relative(now_ms, ms)
            ),
            None => "<span>next \u{2014}</span>".to_string(),
        };
        let last_part = match rt.last_run_at_ms {
            Some(ms) => format!(
                "<span title=\"{}\">last {} {}</span>",
                fmt_instant(Some(ms)),
                status_label,
                fmt_relative(now_ms, ms)
            ),
            None => format!("<span>last {status_label}</span>"),
        };
        let execution = match (&rt.last_run_kind, rt.last_exit_code) {
            (Some(kind), Some(code)) => format!(" · <span>{kind}, exit {code}</span>"),
            (Some(kind), None) => format!(" · <span>{}</span>", escape_html(kind)),
            (None, Some(code)) => format!(" · <span>exit {code}</span>"),
            (None, None) => String::new(),
        };
        JobRow {
            id: escape_html(&job.id),
            name: escape_html(display_name),
            enabled: job.enabled,
            status_label: status_label.to_string(),
            status_class,
            running_for,
            schedule_human: escape_html(&schedule_human),
            schedule_raw: escape_html(&format!("{} · {}", job.schedule.cron, job.schedule.tz)),
            meta_line: format!("{next_part} · {last_part}{execution}"),
            error: rt
                .last_error
                .as_deref()
                .filter(|e| !e.trim().is_empty())
                .map(escape_html),
            message: escape_html(&format!(
                "{} · {}",
                job_shape(job),
                job.payload.message.as_deref().unwrap_or("")
            )),
            delivery: (!job.deliver.is_empty() || !rt.last_delivery.is_empty()).then(|| {
                let targets = job.deliver.iter().map(|target| target.target.as_str()).collect::<Vec<_>>().join(", ");
                let last = rt.last_delivery.join("; ");
                escape_html(&format!("deliver: {targets}{}", if last.is_empty() { String::new() } else { format!(" · {last}") }))
            }),
            outputs,
            total_outputs,
        }
    }
}

fn job_shape(job: &ScheduleJob) -> &'static str {
    match (job.payload.script.is_some(), job.payload.no_agent) {
        (false, _) => "agent",
        (true, true) => "script-only",
        (true, false) => "gated",
    }
}

/// Render the tab body as a self-contained HTML fragment. The dashboard's
/// shared `#content` poller re-fetches the active tab on its cadence, so the
/// fragment carries no inner poll of its own (matching the orchestrator run
/// view and the example tab, which rely on the same shared poller). Output
/// rows carry click-driven `hx-get` (not polling) to open the content viewer.
fn render_fragment(rows: &[JobRow]) -> String {
    let mut html = String::new();
    html.push_str("<main>");
    html.push_str("<section class=\"panel\"><h2>Cron jobs</h2>");
    if rows.is_empty() {
        html.push_str("<p class=\"empty\">No cron jobs configured.</p>");
        html.push_str(
            "<p class=\"empty cron-empty-hint\">Jobs live in cron/jobs.json or via the scheduler HTTP API.</p>",
        );
    } else {
        for row in rows {
            html.push_str(&render_job(row));
        }
    }
    html.push_str("</section></main>");
    html.push_str(CRON_STYLE);
    html
}

fn render_job(row: &JobRow) -> String {
    let running = row
        .running_for
        .as_deref()
        .map(|d| format!(" <span class=\"pill live\">running {d}</span>"))
        .unwrap_or_default();
    let disabled_pill = if row.enabled {
        String::new()
    } else {
        " <span class=\"pill other\">disabled</span>".to_string()
    };
    // Run-now is refused by the API (500 inactive) for a disabled job, and
    // overlap-gated while one is already in flight; the button reflects both.
    let run_disabled_attr = if row.running_for.is_some() || !row.enabled {
        " disabled"
    } else {
        ""
    };
    let toggle_value = if row.enabled { "false" } else { "true" };
    let toggle_label = if row.enabled { "Disable" } else { "Enable" };

    let mut block = String::new();
    block.push_str("<div class=\"cron-job\">");
    block.push_str("<div class=\"cron-info\">");

    block.push_str("<div class=\"cron-title-line\">");
    block.push_str(&format!(
        "<strong class=\"cron-name cron-name-link\" hx-get=\"/scheduler/jobs/{id}/detail\" hx-target=\"#run-detail\" hx-swap=\"innerHTML\">{name}</strong>",
        id = row.id,
        name = row.name,
    ));
    if let Some(delivery) = &row.delivery {
        block.push_str(&format!("<div class=\"cron-meta\">{delivery}</div>"));
    }
    block.push_str(&format!(
        " <span class=\"pill {}\">{}</span>{running}{disabled_pill}",
        row.status_class, row.status_label
    ));
    block.push_str("<span class=\"spacer\"></span>");
    block.push_str("<div class=\"cron-controls\">");
    block.push_str(&format!(
        "<button class=\"cron-btn\"{run_disabled_attr} onclick=\"fetch('/scheduler/jobs/{id}/run-now',{{method:'POST'}})\">Run now</button>",
        id = row.id,
    ));
    block.push_str(&format!(
        "<button class=\"cron-btn\" onclick=\"fetch('/scheduler/jobs/{id}',{{method:'PATCH',headers:{{'Content-Type':'application/json'}},body:'{{&quot;enabled&quot;:{toggle_value}}}'}})\">{toggle_label}</button>",
        id = row.id,
    ));
    block.push_str("</div>"); // cron-controls
    block.push_str("</div>"); // cron-title-line

    block.push_str(&format!(
        "<div class=\"cron-schedule-line\"><span class=\"cron-schedule-human\">{}</span> <span class=\"cron-schedule-raw\">{}</span></div>",
        row.schedule_human, row.schedule_raw
    ));

    block.push_str(&format!("<div class=\"cron-meta\">{}</div>", row.meta_line));

    if let Some(err) = &row.error {
        block.push_str(&format!("<div class=\"cron-error\">Error: {err}</div>"));
    }
    block.push_str(&format!(
        "<div class=\"cron-prompt\" title=\"{msg}\">{msg}</div>",
        msg = row.message
    ));
    block.push_str("</div>"); // cron-info

    block.push_str("<div class=\"cron-outputs\">");
    block.push_str("<div class=\"cron-outputs-head\">Recent outputs</div>");
    if row.outputs.is_empty() {
        block.push_str("<p class=\"empty cron-output-empty\">No outputs yet.</p>");
    } else {
        for out in &row.outputs {
            block.push_str(&format!(
                "<div class=\"cron-output-row\" hx-get=\"/scheduler/jobs/{id}/outputs/{file}\" hx-target=\"#run-detail\" hx-swap=\"innerHTML\"><span class=\"cron-output-label\">{label}</span><span class=\"cron-output-file\">{file}</span></div>",
                id = row.id,
                file = out.filename,
                label = out.label,
            ));
        }
        if row.total_outputs > row.outputs.len() {
            block.push_str(&format!(
                "<div class=\"cron-output-more\" hx-get=\"/scheduler/jobs/{id}/detail\" hx-target=\"#run-detail\" hx-swap=\"innerHTML\">all {total} outputs &rsaquo;</div>",
                id = row.id,
                total = row.total_outputs,
            ));
        }
    }
    block.push_str("</div>"); // cron-outputs

    block.push_str("</div>"); // cron-job
    block
}

/// Classify a job's last-run status into a display word + pill CSS class.
/// Shared by the tab row (title-line pill + meta line) and the job-detail
/// drawer ([`crate::http::job_detail`]).
pub(crate) fn status_label_and_class(status: Option<LastStatus>) -> (&'static str, &'static str) {
    match status {
        Some(LastStatus::Ok) => ("ok", "completed"),
        Some(LastStatus::Error) => ("error", "failed"),
        None => ("never run", "other"),
    }
}

/// List every output filename for a job, newest first. Filenames are
/// `YYYY-MM-DD_HH-mm-ss.md`, so lexical sort is chronological. Missing dir or
/// read errors yield an empty list (tolerant of a missing output directory).
/// `pub(crate)` so the job-detail drawer can render the complete history.
pub(crate) fn list_output_files(root: &Path, job_id: &str) -> Vec<String> {
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
    names
}

/// Format a UTC epoch-millis instant as `YYYY-MM-DD HH:mm:ss` (UTC), or `—`
/// when absent. `pub(crate)` so the job-detail drawer can render the same
/// absolute-time format.
pub(crate) fn fmt_instant(ms: Option<i64>) -> String {
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

/// Format a relative offset between `now_ms` and `target_ms` with at most two
/// significant units (`"3h 12m"`, `"2d 4h"`), seconds only under a minute
/// (`"45s"`). Future targets are prefixed `"in "`; past targets are suffixed
/// `" ago"`. `pub(crate)` so the job-detail drawer shares the same wording.
pub(crate) fn fmt_relative(now_ms: i64, target_ms: i64) -> String {
    let diff_ms = target_ms - now_ms;
    let future = diff_ms >= 0;
    let secs = diff_ms.unsigned_abs() / 1000;
    let core = if secs < 60 {
        format!("{secs}s")
    } else {
        let days = secs / 86_400;
        let hours = (secs % 86_400) / 3600;
        let mins = (secs % 3600) / 60;
        if days > 0 {
            if hours > 0 {
                format!("{days}d {hours}h")
            } else {
                format!("{days}d")
            }
        } else if hours > 0 {
            if mins > 0 {
                format!("{hours}h {mins}m")
            } else {
                format!("{hours}h")
            }
        } else {
            format!("{mins}m")
        }
    };
    if future {
        format!("in {core}")
    } else {
        format!("{core} ago")
    }
}

/// Humanize a handful of common 5-field cron shapes; anything else (or a
/// day-of-month / month restriction) returns `None` and the caller falls back
/// to the raw `cron · tz` string. `pub(crate)` so the job-detail drawer shares
/// the same humanization.
pub(crate) fn humanize_cron(cron: &str) -> Option<String> {
    let fields: Vec<&str> = cron.split_whitespace().collect();
    if fields.len() != 5 {
        return None;
    }
    let (min, hour, dom, mon, dow) = (fields[0], fields[1], fields[2], fields[3], fields[4]);
    if dom != "*" || mon != "*" {
        return None;
    }
    if min == "*" && hour == "*" && dow == "*" {
        return Some("every minute".to_string());
    }
    if let Some(n) = min.strip_prefix("*/") {
        if hour == "*" && dow == "*" && n.parse::<u32>().is_ok() {
            return Some(format!("every {n} min"));
        }
    }
    if min == "0" && hour == "*" && dow == "*" {
        return Some("hourly".to_string());
    }
    if let Some(n) = hour.strip_prefix("*/") {
        if min == "0" && dow == "*" && n.parse::<u32>().is_ok() {
            return Some(format!("every {n} hours"));
        }
    }
    if dow == "*" {
        let m: u32 = min.parse().ok()?;
        let h: u32 = hour.parse().ok()?;
        if m < 60 && h < 24 {
            return Some(format!("daily at {h:02}:{m:02}"));
        }
        return None;
    }
    if dow.len() == 1 && dow.chars().all(|c| c.is_ascii_digit()) {
        let m: u32 = min.parse().ok()?;
        let h: u32 = hour.parse().ok()?;
        let d: u32 = dow.parse().ok()?;
        if m < 60 && h < 24 && d <= 7 {
            return Some(format!("weekly on {} at {h:02}:{m:02}", day_name(d % 7)));
        }
    }
    None
}

fn day_name(d: u32) -> &'static str {
    match d {
        0 => "Sunday",
        1 => "Monday",
        2 => "Tuesday",
        3 => "Wednesday",
        4 => "Thursday",
        5 => "Friday",
        6 => "Saturday",
        _ => "Sunday",
    }
}

/// Parse the leading `YYYY-MM-DD_HH-mm-ss` of an output filename into a short
/// human label (`"Jul 17 00:00"`). Returns `None` (caller falls back to the raw
/// filename) when the prefix doesn't parse, e.g. it is shorter than expected.
pub(crate) fn humanize_output_name(name: &str) -> Option<String> {
    let prefix = name.get(..19)?;
    let dt = NaiveDateTime::parse_from_str(prefix, "%Y-%m-%d_%H-%M-%S").ok()?;
    // `%e` space-pads single-digit days; collapse the resulting double space.
    Some(dt.format("%b %e %H:%M").to_string().replace("  ", " "))
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
                message: Some("hi".to_string()),
                script: None,
                no_agent: false,
                quiet_output: false,
            },
            timeout_ms: None,
            deliver: Vec::new(),
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
        assert!(html.contains("cron/jobs.json"), "empty-state hint: {html}");
        // It is a fragment, not a full page; the dashboard's shared #content
        // poller drives refresh, so the fragment declares no inner poll, and
        // with zero jobs there are no output rows or job-detail links either.
        assert!(!html.contains("<body"));
        assert!(
            !html.contains("hx-get"),
            "no poll and no clickable rows: {html}"
        );
    }

    #[test]
    fn renders_schedule_next_last_status_and_running() {
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
            html.contains("daily at 08:00"),
            "humanized schedule: {html}"
        );
        assert!(
            html.contains("0 8 * * * · Europe/Paris"),
            "raw schedule + tz shown: {html}"
        );
        // The absolute next-fire instant now lives only in the tooltip.
        assert!(
            html.contains("title=\"2026-06-14 06:00:00 UTC\">next "),
            "next run shown as a tooltip on the meta line: {html}"
        );
        assert!(
            html.contains("class=\"pill completed\">ok"),
            "ok status pill shown: {html}"
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
        assert!(
            html.contains(">Enable<"),
            "enable control offered when disabled"
        );
    }

    #[test]
    fn enabled_job_offers_disable_control() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(SchedulerState::new(vec![job("j", "Job", true)]));
        let tab = tab_with(state, dir.path().to_path_buf());
        let html = tab.render().unwrap();
        assert!(
            html.contains(">Disable<"),
            "disable control offered when enabled"
        );
        assert!(
            !html.contains(">disabled<"),
            "no disabled pill when enabled"
        );
    }

    #[test]
    fn meta_line_for_never_run_job_has_no_dangling_dash() {
        // A brand-new, never-armed, never-run job: the meta line must read
        // "next — · last never run" with no dangling "—" after "never run" and
        // no absolute-time tooltip on either span (nothing to show a tooltip
        // for).
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(SchedulerState::new(vec![job("j", "Job", true)]));
        let tab = tab_with(state, dir.path().to_path_buf());
        let html = tab.render().unwrap();
        assert!(
            html.contains("<span>next \u{2014}</span>"),
            "next renders a bare dash with no tooltip: {html}"
        );
        assert!(
            html.contains("<span>last never run</span>"),
            "last renders just the status word with no tooltip or trailing dash: {html}"
        );
        assert!(
            html.contains("<div class=\"cron-meta\"><span>next \u{2014}</span> · <span>last never run</span></div>"),
            "meta line assembled without a kv grid: {html}"
        );
    }

    #[test]
    fn run_now_disabled_for_disabled_job() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(SchedulerState::new(vec![job("j", "Job", false)]));
        let tab = tab_with(state, dir.path().to_path_buf());
        let html = tab.render().unwrap();
        assert!(
            html.contains(
                "<button class=\"cron-btn\" disabled onclick=\"fetch('/scheduler/jobs/j/run-now'"
            ),
            "run now disabled for a disabled job: {html}"
        );
    }

    #[test]
    fn run_now_enabled_for_enabled_idle_job() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(SchedulerState::new(vec![job("j", "Job", true)]));
        let tab = tab_with(state, dir.path().to_path_buf());
        let html = tab.render().unwrap();
        assert!(
            html.contains("<button class=\"cron-btn\" onclick=\"fetch('/scheduler/jobs/j/run-now'"),
            "run now enabled for an idle enabled job: {html}"
        );
    }

    #[test]
    fn run_now_disabled_while_running() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(SchedulerState::new(vec![job("j", "Job", true)]));
        assert!(state.try_claim_running("j", Utc::now().timestamp_millis()));
        let tab = tab_with(state, dir.path().to_path_buf());
        let html = tab.render().unwrap();
        assert!(
            html.contains(
                "<button class=\"cron-btn\" disabled onclick=\"fetch('/scheduler/jobs/j/run-now'"
            ),
            "run now disabled while running: {html}"
        );
    }

    #[test]
    fn job_name_carries_detail_link() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(SchedulerState::new(vec![job("j", "Job", true)]));
        let tab = tab_with(state, dir.path().to_path_buf());
        let html = tab.render().unwrap();
        assert!(
            html.contains(
                "<strong class=\"cron-name cron-name-link\" hx-get=\"/scheduler/jobs/j/detail\" hx-target=\"#run-detail\" hx-swap=\"innerHTML\">Job</strong>"
            ),
            "job name opens the detail drawer: {html}"
        );
    }

    #[test]
    fn lists_recent_outputs_newest_first_as_clickable_rows() {
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
        assert!(
            html.contains("hx-get=\"/scheduler/jobs/j/outputs/2026-06-14_08-00-00.md\""),
            "output row links to the content viewer: {html}"
        );
        // Exactly RECENT_OUTPUTS (3) outputs: no "all N outputs" overflow line.
        assert!(
            !html.contains("outputs &rsaquo;"),
            "no overflow line when the count matches the cap: {html}"
        );
    }

    #[test]
    fn outputs_capped_at_three_with_overflow_line() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("cron").join("output").join("j");
        std::fs::create_dir_all(&out).unwrap();
        for i in 0..5 {
            std::fs::write(out.join(format!("2026-06-14_06-00-{i:02}.md")), "x").unwrap();
        }
        let state = Arc::new(SchedulerState::new(vec![job("j", "Job", true)]));
        let tab = tab_with(state, dir.path().to_path_buf());
        let html = tab.render().unwrap();
        assert_eq!(
            html.matches("class=\"cron-output-row\"").count(),
            RECENT_OUTPUTS,
            "capped at RECENT_OUTPUTS rows: {html}"
        );
        assert!(
            html.contains(
                "<div class=\"cron-output-more\" hx-get=\"/scheduler/jobs/j/detail\" hx-target=\"#run-detail\" hx-swap=\"innerHTML\">all 5 outputs &rsaquo;</div>"
            ),
            "overflow line names the total count and opens the job detail: {html}"
        );
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
    fn list_output_files_missing_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list_output_files(dir.path(), "nope").is_empty());
    }

    #[test]
    fn list_output_files_returns_all_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("cron").join("output").join("j");
        std::fs::create_dir_all(&out).unwrap();
        for i in 0..10 {
            std::fs::write(out.join(format!("2026-06-14_06-00-{i:02}.md")), "x").unwrap();
        }
        let all = list_output_files(dir.path(), "j");
        assert_eq!(all.len(), 10, "unlimited: full history returned");
        assert_eq!(all[0], "2026-06-14_06-00-09.md", "newest first");
    }

    // ---- humanize_cron ------------------------------------------------------

    #[test]
    fn humanize_cron_common_shapes() {
        assert_eq!(humanize_cron("* * * * *").as_deref(), Some("every minute"));
        assert_eq!(
            humanize_cron("*/15 * * * *").as_deref(),
            Some("every 15 min")
        );
        assert_eq!(humanize_cron("0 * * * *").as_deref(), Some("hourly"));
        assert_eq!(
            humanize_cron("0 */2 * * *").as_deref(),
            Some("every 2 hours")
        );
        assert_eq!(
            humanize_cron("30 9 * * *").as_deref(),
            Some("daily at 09:30")
        );
        assert_eq!(
            humanize_cron("0 8 * * 1").as_deref(),
            Some("weekly on Monday at 08:00")
        );
    }

    #[test]
    fn humanize_cron_unrecognized_shapes_return_none() {
        assert_eq!(humanize_cron("0 8 1 * *"), None); // fixed day-of-month
        assert_eq!(humanize_cron("0 8 * 6 *"), None); // fixed month
        assert_eq!(humanize_cron("1,2 * * * *"), None); // list expression
        assert_eq!(humanize_cron("0 8 * * mon"), None); // named weekday, not a single digit
        assert_eq!(humanize_cron("bad"), None); // wrong field count
    }

    // ---- fmt_relative --------------------------------------------------------

    #[test]
    fn fmt_relative_future_and_past() {
        let now = 1_000_000_000_i64;
        assert_eq!(fmt_relative(now, now + 45_000), "in 45s");
        assert_eq!(
            fmt_relative(now, now + (3 * 3600 + 12 * 60) * 1000),
            "in 3h 12m"
        );
        assert_eq!(
            fmt_relative(now, now + (2 * 86_400 + 4 * 3600) * 1000),
            "in 2d 4h"
        );
        assert_eq!(fmt_relative(now, now - 45_000), "45s ago");
        assert_eq!(fmt_relative(now, now - (12 * 3600) * 1000), "12h ago");
    }

    // ---- humanize_output_name -------------------------------------------------

    #[test]
    fn humanize_output_name_parses_leading_timestamp() {
        assert_eq!(
            humanize_output_name("2026-07-17_00-00-00.397.md").as_deref(),
            Some("Jul 17 00:00")
        );
        assert_eq!(
            humanize_output_name("2026-01-05_23-59-00.md").as_deref(),
            Some("Jan 5 23:59")
        );
    }

    #[test]
    fn humanize_output_name_unparseable_returns_none() {
        assert_eq!(humanize_output_name("notes.txt"), None);
        assert_eq!(humanize_output_name("short.md"), None);
    }
}
