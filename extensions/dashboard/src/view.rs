use askama::Template;
use chrono::{DateTime, Utc};
use orchestrator_api::{ActiveRun, AgentInfo, HistoryRow, RunRow, RunSnapshot};

#[derive(Template)]
#[template(path = "index.html")]
pub struct DashboardTemplate {
    pub content: ContentTemplate,
}

impl DashboardTemplate {
    pub fn page(snapshot: RunSnapshot) -> Self {
        Self {
            content: ContentTemplate::from_snapshot(snapshot),
        }
    }
}

#[derive(Template)]
#[template(path = "content.html")]
pub struct ContentTemplate {
    pub agent: AgentInfo,
    pub active_runs: Vec<ActiveRun>,
    pub history: Vec<HistoryRow>,
    pub rate_limit_min_remaining: Option<i64>,
    pub active_count: usize,
    pub recent_count: usize,
    pub last_tick: String,
    pub last_tick_at: String,
}

impl ContentTemplate {
    pub fn from_snapshot(s: RunSnapshot) -> Self {
        let tick_at = s.last_tick_at;
        let last_tick = tick_at
            .map(|ts| fmt_age((Utc::now() - ts).num_seconds().max(0)))
            .unwrap_or_else(|| "never".to_string());
        let last_tick_at = tick_at.map(|ts| ts.to_rfc3339()).unwrap_or_default();
        let history = s
            .runs
            .into_iter()
            .filter(|run| run.outcome.as_deref() != Some("park_barrier"))
            .map(history_row)
            .collect::<Vec<_>>();
        Self {
            agent: s.agent,
            active_count: s.active_runs.len(),
            recent_count: history.len(),
            active_runs: s.active_runs,
            history,
            rate_limit_min_remaining: s.rate_limit_min_remaining,
            last_tick,
            last_tick_at,
        }
    }
}

fn history_row(run: RunRow) -> HistoryRow {
    let (label, class) = history_bucket(&run);
    let age = fmt_run_age(run.finished_at.as_deref().unwrap_or(&run.started_at));
    HistoryRow {
        status: label.to_string(),
        status_class: class.to_string(),
        identifier: run.issue_identifier,
        run: run.run_id.clone(),
        run_id: run.run_id,
        age,
    }
}

fn fmt_age(secs: i64) -> String {
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

fn fmt_run_age(ts: &str) -> String {
    DateTime::parse_from_rfc3339(ts)
        .map(|dt| fmt_age((Utc::now() - dt.with_timezone(&Utc)).num_seconds().max(0)))
        .unwrap_or_else(|_| "-".to_string())
}

fn history_bucket(run: &RunRow) -> (&'static str, &'static str) {
    if run.process_alive || run.finished_at.is_none() || run.outcome.is_none() {
        return ("Live", "live");
    }
    match run.outcome.as_deref().unwrap_or_default() {
        "completed" | "terminal" | "released" => ("Completed", "completed"),
        "error" | "failed" | "hook_failed" | "dispatch_failed" => ("Failed", "failed"),
        "interrupted"
        | "interrupted_gateway_restart"
        | "killed"
        | "orphaned"
        | "stalled"
        | "needs_human" => ("Interrupted", "interrupted"),
        _ => ("Other", "other"),
    }
}
