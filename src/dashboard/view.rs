//! askama template structs mapping `AppState` snapshots into the compiled HTML.
//!
//! Built once per request by cloning the `AppState` read-guards. A single
//! template (`templates/index.html`) renders all five PRD sections.

use askama::Template;
use chrono::{DateTime, Utc};
use std::sync::atomic::Ordering;

use crate::state::{ActiveRun, AgentInfo, AppState};

/// One pre-formatted history row for the template.
pub struct HistoryRow {
    pub status: String,
    pub status_class: String,
    pub identifier: String,
    pub run: String,
    pub run_id: String,
    pub age: String,
}

/// The whole dashboard page. Field names are the contract the template binds to.
#[derive(Template)]
#[template(path = "index.html")]
pub struct DashboardTemplate {
    pub agent: AgentInfo,
    pub active_runs: Vec<ActiveRun>,
    pub history: Vec<HistoryRow>,
    /// Minimum Linear rate-limit requests remaining observed since startup.
    /// `None` when no Linear API calls have been made (e.g. FileTracker).
    pub rate_limit_min_remaining: Option<i64>,
    pub active_count: usize,
    pub recent_count: usize,
    pub last_tick: String,
}

impl DashboardTemplate {
    /// Snapshot the shared state into an owned, render-ready template.
    pub async fn from_state(s: &AppState) -> Self {
        let active_runs = s.active_runs.read().await.clone();
        let last_tick = s
            .last_tick_at
            .read()
            .await
            .map(|ts| ts.to_rfc3339())
            .unwrap_or_else(|| "never".to_string());

        let history: Vec<HistoryRow> = s
            .store
            .list_runs(50)
            .unwrap_or_default()
            .into_iter()
            .filter(|run| run.outcome.as_deref() != Some("park_barrier"))
            .map(|run| {
                let (label, class) = history_bucket(&run);
                HistoryRow {
                    status: label.to_string(),
                    status_class: class.to_string(),
                    identifier: run.issue_identifier,
                    run: run.run_id.clone(),
                    run_id: run.run_id,
                    age: fmt_run_age(run.finished_at.as_deref().unwrap_or(&run.started_at)),
                }
            })
            .collect();

        let recent_count = history.len();
        let active_count = active_runs.len();

        let rate_limit_min_remaining = {
            let v = s.rate_limit_min_remaining.load(Ordering::SeqCst);
            if v == i64::MAX {
                None
            } else {
                Some(v)
            }
        };

        Self {
            agent: s.agent.clone(),
            active_runs,
            history,
            rate_limit_min_remaining,
            active_count,
            recent_count,
            last_tick,
        }
    }
}

/// Compact relative age: "12s", "5m", "3h", "2d".
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

fn history_bucket(run: &crate::store::RunRow) -> (&'static str, &'static str) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::RunRow;

    fn run_with_outcome(outcome: &str) -> RunRow {
        RunRow {
            run_id: "r1".to_string(),
            issue_id: "i1".to_string(),
            issue_identifier: "TST-1".to_string(),
            workspace: "/tmp/w".to_string(),
            profile_json: None,
            workflow_path: None,
            workflow_sha: None,
            pid: 0,
            worker_id: None,
            started_at: "2024-01-01T00:00:00Z".to_string(),
            finished_at: Some("2024-01-01T00:01:00Z".to_string()),
            outcome: Some(outcome.to_string()),
            exit_code: None,
            process_alive: false,
        }
    }

    #[test]
    fn stalled_maps_to_interrupted() {
        assert_eq!(history_bucket(&run_with_outcome("stalled")), ("Interrupted", "interrupted"));
    }

    #[test]
    fn needs_human_maps_to_interrupted() {
        assert_eq!(
            history_bucket(&run_with_outcome("needs_human")),
            ("Interrupted", "interrupted")
        );
    }

    #[test]
    fn error_maps_to_failed() {
        assert_eq!(history_bucket(&run_with_outcome("error")), ("Failed", "failed"));
    }

    #[test]
    fn hook_failed_maps_to_failed() {
        assert_eq!(history_bucket(&run_with_outcome("hook_failed")), ("Failed", "failed"));
    }

    #[test]
    fn completed_maps_to_completed() {
        assert_eq!(history_bucket(&run_with_outcome("completed")), ("Completed", "completed"));
    }

    #[test]
    fn terminal_maps_to_completed() {
        assert_eq!(history_bucket(&run_with_outcome("terminal")), ("Completed", "completed"));
    }

    #[test]
    fn interrupted_maps_to_interrupted() {
        assert_eq!(
            history_bucket(&run_with_outcome("interrupted")),
            ("Interrupted", "interrupted")
        );
    }

    #[test]
    fn interrupted_gateway_restart_maps_to_interrupted() {
        assert_eq!(
            history_bucket(&run_with_outcome("interrupted_gateway_restart")),
            ("Interrupted", "interrupted")
        );
    }
}
