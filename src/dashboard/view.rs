//! askama template structs mapping `AppState` snapshots into the compiled HTML.
//!
//! Built once per request by cloning the `AppState` read-guards. A single
//! template (`templates/index.html`) renders all five PRD sections.

use askama::Template;
use chrono::Utc;

use crate::state::{ActiveRun, AgentInfo, AppState, QueueItem, RetryItem, RunStatus};

/// One pre-formatted history row for the template.
pub struct HistoryRow {
    pub status: String,
    pub status_class: String,
    pub identifier: String,
    pub run: String,
    pub age: String,
}

/// The whole dashboard page. Field names are the contract the template binds to.
#[derive(Template)]
#[template(path = "index.html")]
pub struct DashboardTemplate {
    pub agent: AgentInfo,
    pub paused: bool,
    pub active: Option<ActiveRun>,
    /// `active.status` pre-formatted (`RunStatus` lacks `Display`).
    pub active_status: String,
    pub elapsed_secs: i64,
    pub queue: Vec<QueueItem>,
    pub retry: Vec<RetryItem>,
    pub events: Vec<String>,
    pub history: Vec<HistoryRow>,
}

impl DashboardTemplate {
    /// Snapshot the shared state into an owned, render-ready template.
    pub async fn from_state(s: &AppState) -> Self {
        let paused = s.paused.load(std::sync::atomic::Ordering::SeqCst);

        let active = s.active.read().await.clone();
        let queue = s.queue.read().await.clone();
        let retry = s.retry.read().await.clone();
        let events = s.events.snapshot();

        // Elapsed since the active run started, computed at render time.
        let elapsed_secs = active
            .as_ref()
            .map(|a| (Utc::now() - a.started_at).num_seconds().max(0))
            .unwrap_or(0);

        // RunStatus has no Display impl; render its Debug form for the template.
        let active_status = active
            .as_ref()
            .map(|a| format!("{:?}", a.status))
            .unwrap_or_default();

        let now = Utc::now();
        let history: Vec<HistoryRow> = s
            .history
            .snapshot()
            .into_iter()
            .map(|h| {
                let (label, class) = match h.status {
                    RunStatus::Succeeded => ("Completed", "completed"),
                    RunStatus::Failed => ("Failed", "failed"),
                    RunStatus::Cancelled => ("Interrupted", "interrupted"),
                    RunStatus::Interrupted => ("Interrupted", "interrupted"),
                    RunStatus::RetryQueued => ("Retrying", "retrying"),
                    RunStatus::Running => ("Running", "running"),
                    RunStatus::Crashed => ("Crashed", "failed"),
                    RunStatus::NeedsHuman => ("Needs Human", "needs-human"),
                };
                HistoryRow {
                    status: label.to_string(),
                    status_class: class.to_string(),
                    identifier: h.identifier,
                    run: format!("claude:{}", h.pid),
                    age: fmt_age((now - h.ended_at).num_seconds().max(0)),
                }
            })
            .collect();

        Self {
            agent: s.agent.clone(),
            paused,
            active,
            active_status,
            elapsed_secs,
            queue,
            retry,
            events,
            history,
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
