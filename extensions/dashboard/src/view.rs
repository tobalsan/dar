use askama::Template;
use chrono::{DateTime, Utc};
use orchestrator_api::{ActiveRun, AgentInfo, EventRow, HistoryRow, RunRow, RunSnapshot};

/// Max events rendered in the run-detail drawer.
pub const RUN_DETAIL_EVENT_CAP: usize = 500;

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

/// One event line rendered in the run-detail drawer. `protocol_event` payloads
/// are unwrapped to their `log_row` type + `text`; any other payload renders raw.
pub struct EventLine {
    pub ts: String,
    pub kind: String,
    pub row_type: String,
    pub text: String,
}

#[derive(Template)]
#[template(path = "run_detail.html")]
pub struct RunDetailTemplate {
    pub run_id: String,
    pub identifier: String,
    pub status: String,
    pub status_class: String,
    pub outcome: String,
    pub exit_code: String,
    pub pid: u32,
    pub workspace: String,
    pub started_at: String,
    pub finished_at: String,
    pub events: Vec<EventLine>,
    pub event_count: usize,
    pub process_alive: bool,
    pub workflow_content: String,
    pub log_events: Vec<EventLine>,
    pub last_event_id: i64,
}

impl RunDetailTemplate {
    pub fn build(run: RunRow, events: Vec<EventRow>) -> Self {
        let (label, class) = history_bucket(&run);
        let workflow_content = run
            .workflow_path
            .as_deref()
            .map(|p| {
                std::fs::read_to_string(p).unwrap_or_else(|_| "unavailable".to_string())
            })
            .unwrap_or_else(|| "unavailable".to_string());
        let last_event_id = events.iter().map(|e| e.event_id).max().unwrap_or(0);
        let event_lines: Vec<EventLine> = events.into_iter().map(event_line).collect();
        let log_events: Vec<EventLine> = event_lines
            .iter()
            .filter(|e| !e.row_type.is_empty())
            .map(|e| EventLine {
                ts: e.ts.clone(),
                kind: e.kind.clone(),
                row_type: e.row_type.clone(),
                text: e.text.clone(),
            })
            .collect();
        Self {
            event_count: event_lines.len(),
            run_id: run.run_id,
            identifier: run.issue_identifier,
            status: label.to_string(),
            status_class: class.to_string(),
            outcome: run.outcome.unwrap_or_default(),
            exit_code: run
                .exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".to_string()),
            pid: run.pid,
            workspace: run.workspace,
            started_at: run.started_at,
            finished_at: run.finished_at.unwrap_or_default(),
            process_alive: run.process_alive,
            workflow_content,
            log_events,
            last_event_id,
            events: event_lines,
        }
    }
}

fn event_line(e: EventRow) -> EventLine {
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(&e.payload)
    {
        if map.get("type").and_then(|v| v.as_str()) == Some("protocol_event") {
            let row_type = map
                .get("log_row")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let text = map
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            return EventLine {
                ts: e.ts,
                kind: e.kind,
                row_type,
                text,
            };
        }
    }
    EventLine {
        ts: e.ts,
        kind: e.kind,
        row_type: String::new(),
        text: e.payload,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_run() -> RunRow {
        RunRow {
            run_id: "ALG-1-123".to_string(),
            issue_id: "abc".to_string(),
            issue_identifier: "ALG-1".to_string(),
            workspace: "/tmp/ws".to_string(),
            profile_json: None,
            workflow_path: None,
            workflow_sha: None,
            pid: 4242,
            worker_id: None,
            started_at: "2026-06-12T00:00:00Z".to_string(),
            finished_at: Some("2026-06-12T00:01:00Z".to_string()),
            outcome: Some("completed".to_string()),
            exit_code: Some(0),
            process_alive: false,
        }
    }

    #[test]
    fn run_detail_renders_metadata_and_events() {
        let events = vec![
            EventRow {
                event_id: 1,
                run_id: Some("ALG-1-123".to_string()),
                issue_identifier: "ALG-1".to_string(),
                kind: "runner_event".to_string(),
                payload: r#"{"type":"protocol_event","stream":"stdout","log_row":"tool_call","text":"ran bash"}"#
                    .to_string(),
                ts: "2026-06-12T00:00:30Z".to_string(),
            },
            EventRow {
                event_id: 2,
                run_id: Some("ALG-1-123".to_string()),
                issue_identifier: "ALG-1".to_string(),
                kind: "lifecycle".to_string(),
                payload: "dispatch attempt=0 pid=4242".to_string(),
                ts: "2026-06-12T00:00:00Z".to_string(),
            },
        ];

        let html = RunDetailTemplate::build(sample_run(), events)
            .render()
            .expect("run detail template renders");

        // Metadata.
        assert!(html.contains("ALG-1-123"), "run_id shown");
        assert!(html.contains("ALG-1"), "identifier shown");
        assert!(html.contains("/tmp/ws"), "workspace shown");
        assert!(html.contains("4242"), "pid shown");
        assert!(html.contains("completed"), "outcome shown");
        // protocol_event unwrapped to log_row + text.
        assert!(html.contains("tool_call"), "log_row tag shown");
        assert!(html.contains("ran bash"), "protocol text shown");
        // Non-protocol payload rendered raw.
        assert!(html.contains("dispatch attempt=0 pid=4242"), "raw payload shown");
        // Close button clears the drawer.
        assert!(html.contains("getElementById('run-detail')"), "close button present");
    }

    #[test]
    fn run_detail_handles_empty_events() {
        let html = RunDetailTemplate::build(sample_run(), Vec::new())
            .render()
            .expect("renders with no events");
        assert!(html.contains("No persisted events"), "empty state shown");
    }

    #[test]
    fn run_detail_renders_tabs() {
        let html = RunDetailTemplate::build(sample_run(), Vec::new())
            .render()
            .expect("renders");
        assert!(html.contains("Logs"), "Logs tab present");
        assert!(html.contains("Events"), "Events tab present");
        assert!(html.contains("Workflow"), "Workflow tab present");
    }

    #[test]
    fn run_detail_shows_interrupt_kill_enabled_for_live_run() {
        let mut run = sample_run();
        run.process_alive = true;
        run.finished_at = None;
        run.outcome = None;
        let html = RunDetailTemplate::build(run, Vec::new())
            .render()
            .expect("renders");
        assert!(html.contains("Interrupt"), "Interrupt button present");
        assert!(html.contains("Kill"), "Kill button present");
        let interrupt_pos = html.find("Interrupt").unwrap();
        let snippet = &html[interrupt_pos.saturating_sub(100)..interrupt_pos];
        assert!(!snippet.contains("disabled"), "Interrupt button enabled for live run");
    }

    #[test]
    fn run_detail_shows_interrupt_kill_disabled_for_finished_run() {
        let html = RunDetailTemplate::build(sample_run(), Vec::new())
            .render()
            .expect("renders");
        // process_alive=false in sample_run, so buttons should have disabled
        assert!(html.contains("disabled"), "buttons disabled for finished run");
    }
}
