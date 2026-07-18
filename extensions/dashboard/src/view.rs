use askama::Template;
use cap_dashboard_tab::escape_html;
use chrono::{DateTime, Utc};
use orchestrator_api::{ActiveRun, AgentInfo, EventRow, HistoryRow, RunRow, RunSnapshot};

/// One entry in the dashboard's top tab navigation. Built from the registered
/// `cap_dashboard_tab::DashboardTab` providers; the orchestrator run view is
/// rendered separately as the default "Runs" tab.
pub struct TabNav {
    pub id: String,
    pub title: String,
    pub self_refreshing: bool,
}

#[derive(Template)]
#[template(path = "index.html")]
pub struct DashboardTemplate {
    pub content: ContentTemplate,
    pub tabs: Vec<TabNav>,
    /// Which tab is initially active: `"content"` for the built-in Runs view,
    /// or a registered tab's id.
    pub active_tab: String,
    /// Pre-rendered fragment for `active_tab` when it is not `"content"`;
    /// `None` renders `content` (the Runs view) as the initial body.
    pub initial_fragment: Option<String>,
}

impl DashboardTemplate {
    pub fn page(snapshot: RunSnapshot) -> Self {
        Self::page_with_tabs(snapshot, Vec::new())
    }

    pub fn page_with_tabs(snapshot: RunSnapshot, tabs: Vec<TabNav>) -> Self {
        Self {
            content: ContentTemplate::from_snapshot(snapshot),
            tabs,
            active_tab: "content".to_string(),
            initial_fragment: None,
        }
    }

    /// Passive-agent variant: `active_tab_id` is the id of the tab that
    /// claimed `passive_default()`, pre-rendered as `fragment`. The Runs view
    /// is still built from `snapshot` (it just isn't the initial body) so a
    /// tab render failure can fall back to it.
    pub fn page_with_active_tab(
        snapshot: RunSnapshot,
        tabs: Vec<TabNav>,
        active_tab_id: String,
        fragment: String,
    ) -> Self {
        Self {
            content: ContentTemplate::from_snapshot(snapshot),
            tabs,
            active_tab: active_tab_id,
            initial_fragment: Some(fragment),
        }
    }
}

/// Number of history rows shown per page in the Recent runs list.
pub const PAGE_SIZE: usize = 10;

/// One pagination control rendered below the Recent runs list.
pub struct PageLink {
    pub page: usize,
    pub label: String,
    pub current: bool,
}

#[derive(Template)]
#[template(path = "content.html")]
pub struct ContentTemplate {
    pub agent: AgentInfo,
    /// Active model formatted for display (`provider/model`, bare model, or `—`).
    pub agent_model: String,
    pub active_runs: Vec<ActiveRun>,
    pub history: Vec<HistoryRow>,
    pub rate_limit_min_remaining: Option<i64>,
    pub active_count: usize,
    pub recent_count: usize,
    pub last_tick: String,
    pub last_tick_at: String,
    pub page: usize,
    pub total_pages: usize,
    pub pages: Vec<PageLink>,
    pub has_prev: bool,
    pub has_next: bool,
}

impl ContentTemplate {
    pub fn from_snapshot(s: RunSnapshot) -> Self {
        Self::from_snapshot_page(s, 1)
    }

    pub fn from_snapshot_page(s: RunSnapshot, requested_page: usize) -> Self {
        let tick_at = s.last_tick_at;
        let last_tick = tick_at
            .map(|ts| fmt_age((Utc::now() - ts).num_seconds().max(0)))
            .unwrap_or_else(|| "never".to_string());
        let last_tick_at = tick_at.map(|ts| ts.to_rfc3339()).unwrap_or_default();
        let active_ids: std::collections::HashSet<&str> =
            s.active_runs.iter().map(|r| r.run_id.as_str()).collect();
        let all_history = s
            .runs
            .into_iter()
            .filter(|run| run.outcome.as_deref() != Some("park_barrier"))
            .filter(|run| !active_ids.contains(run.run_id.as_str()))
            .map(history_row)
            .collect::<Vec<_>>();
        let recent_count = all_history.len();
        let total_pages = recent_count.div_ceil(PAGE_SIZE).max(1);
        let page = requested_page.clamp(1, total_pages);
        let start = (page - 1) * PAGE_SIZE;
        let history = all_history
            .into_iter()
            .skip(start)
            .take(PAGE_SIZE)
            .collect::<Vec<_>>();
        let pages = (1..=total_pages)
            .map(|p| PageLink {
                page: p,
                label: p.to_string(),
                current: p == page,
            })
            .collect::<Vec<_>>();
        let agent_model = fmt_model_display(
            &s.agent.runner,
            s.agent.model.as_deref(),
            s.agent.provider.as_deref(),
        );
        Self {
            agent: s.agent,
            agent_model,
            active_count: s.active_runs.len(),
            recent_count,
            active_runs: s.active_runs,
            history,
            rate_limit_min_remaining: s.rate_limit_min_remaining,
            last_tick,
            last_tick_at,
            page,
            total_pages,
            pages,
            has_prev: page > 1,
            has_next: page < total_pages,
        }
    }
}

/// Format the dispatched/active model for display.
///
/// Returns `—` when no model is configured. When a provider is set *and* the
/// runner is multi-provider (`opencode`/`pi`), the model is prefixed as
/// `provider/model`. `codex` is OpenAI-only, so it never carries a provider
/// prefix; the model name is shown alone.
fn fmt_model_display(runner: &str, model: Option<&str>, provider: Option<&str>) -> String {
    let model = match model.map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => m,
        None => return "\u{2014}".to_string(),
    };
    let multi_provider = matches!(runner, "opencode" | "pi");
    match provider.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) if multi_provider => format!("{p}/{model}"),
        _ => model.to_string(),
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
    pub event_id: i64,
    pub ts: String,
    pub kind: String,
    pub row_type: String,
    pub text: String,
    pub detail: String,
    pub rendered: String,
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
    /// Runner this run was dispatched with, or `—` when unknown (legacy rows).
    pub runner: String,
    /// Model this run was dispatched with, formatted for display.
    pub model: String,
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
            .map(|p| std::fs::read_to_string(p).unwrap_or_else(|_| "unavailable".to_string()))
            .unwrap_or_else(|| "unavailable".to_string());
        let last_event_id = events.iter().map(|e| e.event_id).max().unwrap_or(0);
        let event_lines: Vec<EventLine> = events.into_iter().map(event_line).collect();
        let log_events: Vec<EventLine> = event_lines
            .iter()
            .filter(|e| !e.row_type.is_empty())
            .map(|e| {
                let rendered = render_log_row(e);
                EventLine {
                    event_id: e.event_id,
                    ts: e.ts.clone(),
                    kind: e.kind.clone(),
                    row_type: e.row_type.clone(),
                    text: e.text.clone(),
                    detail: e.detail.clone(),
                    rendered,
                }
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
            runner: run
                .runner
                .as_deref()
                .map(str::to_string)
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| "\u{2014}".to_string()),
            model: fmt_model_display(
                run.runner.as_deref().unwrap_or_default(),
                run.model.as_deref(),
                run.provider.as_deref(),
            ),
            process_alive: run.process_alive,
            workflow_content,
            log_events,
            last_event_id,
            events: event_lines,
        }
    }
}

pub(crate) fn fmt_event_ts(ts: &str) -> String {
    DateTime::parse_from_rfc3339(ts)
        .map(|dt| {
            dt.with_timezone(&Utc)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|_| ts.to_string())
}

fn event_line(e: EventRow) -> EventLine {
    if let Ok(serde_json::Value::Object(map)) =
        serde_json::from_str::<serde_json::Value>(&e.payload)
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
            let detail = map
                .get("detail")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            return EventLine {
                event_id: e.event_id,
                ts: fmt_event_ts(&e.ts),
                kind: e.kind,
                row_type,
                text,
                detail,
                rendered: String::new(),
            };
        }
    }
    EventLine {
        event_id: e.event_id,
        ts: fmt_event_ts(&e.ts),
        kind: e.kind,
        row_type: String::new(),
        text: e.payload,
        detail: String::new(),
        rendered: String::new(),
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

fn render_markdown(md: &str) -> String {
    use pulldown_cmark::{html, Event, Options, Parser};
    let parser = Parser::new_ext(md, Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES);
    let safe_parser = parser.map(|event| match event {
        Event::Html(s) | Event::InlineHtml(s) => Event::Text(pulldown_cmark::CowStr::Boxed(
            escape_html(&s).into_boxed_str(),
        )),
        other => other,
    });
    let mut output = String::new();
    html::push_html(&mut output, safe_parser);
    output
}

pub(crate) fn render_log_row(ev: &EventLine) -> String {
    let time_part = ev.ts.get(11..19).unwrap_or("");
    match ev.row_type.as_str() {
        "assistant" => {
            let md = render_markdown(&ev.text);
            format!(
                "<div class=\"log-assistant\" data-event-id=\"{}\"><span class=\"log-ts\">{}</span><div class=\"log-md\">{}</div></div>",
                ev.event_id, time_part, md
            )
        }
        "thinking" => {
            format!(
                "<details class=\"log-think\" data-eid=\"{}\" data-event-id=\"{}\"><summary>Thinking</summary><pre class=\"log-pre\">{}</pre></details>",
                ev.event_id, ev.event_id, escape_html(&ev.text)
            )
        }
        "tool_call" => {
            let body = if ev.detail.is_empty() {
                format!("$ {}", escape_html(&ev.text))
            } else {
                format!("$ {}\n\n{}", escape_html(&ev.text), escape_html(&ev.detail))
            };
            format!(
                "<details class=\"log-tool\" data-eid=\"{}\" data-event-id=\"{}\"><summary><span class=\"log-pill\">tool</span><span class=\"log-cmd\">{}</span></summary><pre class=\"log-pre\">{}</pre></details>",
                ev.event_id, ev.event_id, escape_html(&ev.text), body
            )
        }
        "tool_output" => {
            format!(
                "<details class=\"log-tool\" data-eid=\"{}\" data-event-id=\"{}\"><summary><span class=\"log-pill\">output</span><span class=\"log-cmd\">{}</span></summary><pre class=\"log-pre\">{}</pre></details>",
                ev.event_id, ev.event_id, escape_html(&ev.text), escape_html(&ev.text)
            )
        }
        "error" => {
            format!(
                "<div class=\"log-error\" data-event-id=\"{}\"><span class=\"log-ts\">{}</span><pre class=\"log-pre\">{}</pre></div>",
                ev.event_id, time_part, escape_html(&ev.text)
            )
        }
        "user" => {
            format!(
                "<div class=\"log-user\" data-event-id=\"{}\"><pre class=\"log-pre\">{}</pre></div>",
                ev.event_id, escape_html(&ev.text)
            )
        }
        other => {
            format!(
                "<div class=\"log-line\" data-event-id=\"{}\"><span class=\"kind\">{}</span><span class=\"ev-text\">{}</span></div>",
                ev.event_id, escape_html(other), escape_html(&ev.text)
            )
        }
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
            runner: Some("codex".to_string()),
            model: Some("gpt-5".to_string()),
            provider: None,
        }
    }

    fn snapshot_with_runs(n: usize) -> RunSnapshot {
        let mut s = RunSnapshot::empty();
        s.runs = (0..n)
            .map(|i| {
                let mut r = sample_run();
                r.run_id = format!("ALG-{i}-run");
                r.issue_identifier = format!("ALG-{i}");
                r
            })
            .collect();
        s
    }

    #[test]
    fn pagination_slices_to_page_size() {
        let t = ContentTemplate::from_snapshot_page(snapshot_with_runs(23), 1);
        assert_eq!(t.recent_count, 23, "full count reported");
        assert_eq!(t.history.len(), PAGE_SIZE, "first page capped at page size");
        assert_eq!(t.total_pages, 3, "23 runs -> 3 pages");
        assert_eq!(t.page, 1);
        assert!(!t.has_prev);
        assert!(t.has_next);
        assert_eq!(t.history[0].identifier, "ALG-0", "newest-first preserved");
    }

    #[test]
    fn pagination_last_page_has_remainder() {
        let t = ContentTemplate::from_snapshot_page(snapshot_with_runs(23), 3);
        assert_eq!(t.history.len(), 3, "last page holds the remainder");
        assert_eq!(t.page, 3);
        assert!(t.has_prev);
        assert!(!t.has_next);
        assert_eq!(
            t.history[0].identifier, "ALG-20",
            "page 3 starts at index 20"
        );
    }

    #[test]
    fn pagination_clamps_out_of_range_page() {
        let over = ContentTemplate::from_snapshot_page(snapshot_with_runs(15), 99);
        assert_eq!(over.page, 2, "clamped to last page");
        let under = ContentTemplate::from_snapshot_page(snapshot_with_runs(15), 0);
        assert_eq!(under.page, 1, "clamped to first page");
    }

    #[test]
    fn pagination_single_page_when_few_runs() {
        let t = ContentTemplate::from_snapshot_page(snapshot_with_runs(4), 1);
        assert_eq!(t.total_pages, 1);
        assert_eq!(t.history.len(), 4);
        assert!(!t.has_prev);
        assert!(!t.has_next);
    }

    #[test]
    fn pagination_empty_history_is_single_page() {
        let t = ContentTemplate::from_snapshot_page(snapshot_with_runs(0), 1);
        assert_eq!(t.total_pages, 1);
        assert_eq!(t.recent_count, 0);
        assert!(t.history.is_empty());
    }

    #[test]
    fn pagination_controls_render_below_list() {
        let html = ContentTemplate::from_snapshot_page(snapshot_with_runs(23), 2)
            .render()
            .expect("content renders");
        assert!(html.contains("class=\"pager\""), "pager rendered");
        assert!(
            html.contains("hx-get=\"/content\""),
            "pager uses /content endpoint"
        );
        assert!(
            html.contains("window.__dashPage=1"),
            "first-page control present"
        );
        assert!(
            html.contains("window.__dashPage=3"),
            "last-page control present"
        );
        assert!(
            html.contains("aria-current=\"page\""),
            "current page marked"
        );
    }

    #[test]
    fn pagination_controls_hidden_for_single_page() {
        let html = ContentTemplate::from_snapshot_page(snapshot_with_runs(7), 1)
            .render()
            .expect("content renders");
        assert!(
            !html.contains("class=\"pager\""),
            "no pager for a single page"
        );
    }

    #[test]
    fn index_without_tabs_renders_no_tab_nav() {
        let html = DashboardTemplate::page(RunSnapshot::empty())
            .render()
            .expect("page renders");
        assert!(
            !html.contains("id=\"dash-tabs\""),
            "no tab nav element when zero tabs registered"
        );
        assert!(html.contains("id=\"content\""), "content shell preserved");
    }

    #[test]
    fn index_with_tabs_renders_nav_and_default_runs_tab() {
        let tabs = vec![TabNav {
            id: "scheduler".to_string(),
            title: "Scheduler".to_string(),
            self_refreshing: false,
        }];
        let html = DashboardTemplate::page_with_tabs(RunSnapshot::empty(), tabs)
            .render()
            .expect("page renders");
        assert!(html.contains("id=\"dash-tabs\""), "tab nav rendered");
        assert!(html.contains(">Runs<"), "default Runs tab present");
        assert!(
            html.contains("/tabs/scheduler"),
            "registered tab links to its fragment"
        );
        assert!(
            html.contains(">Scheduler<"),
            "registered tab title rendered"
        );
        assert!(
            !html.contains("hx-trigger=\"every 2s\""),
            "no declarative poll on #content"
        );
        assert!(
            !html.contains("hx-swap=\"outerHTML\""),
            "no body/outer swap introduced"
        );
    }

    #[test]
    fn index_tab_buttons_carry_self_refreshing_data_attr() {
        let tabs = vec![
            TabNav {
                id: "scheduler".to_string(),
                title: "Scheduler".to_string(),
                self_refreshing: false,
            },
            TabNav {
                id: "chat".to_string(),
                title: "Chat".to_string(),
                self_refreshing: true,
            },
        ];
        let html = DashboardTemplate::page_with_tabs(RunSnapshot::empty(), tabs)
            .render()
            .expect("page renders");
        assert!(
            html.contains(r#"data-tab-url="/content" data-self-refreshing="false""#),
            "Runs button is not self-refreshing: {html}"
        );
        assert!(
            html.contains(r#"data-tab-url="/tabs/scheduler" data-self-refreshing="false""#),
            "polling tab marked non-self-refreshing: {html}"
        );
        assert!(
            html.contains(r#"data-tab-url="/tabs/chat" data-self-refreshing="true""#),
            "live tab marked self-refreshing: {html}"
        );
    }

    #[test]
    fn index_poller_is_a_setinterval_gated_on_dashtablive() {
        let html = DashboardTemplate::page(RunSnapshot::empty())
            .render()
            .expect("page renders");
        assert!(
            html.contains("setInterval"),
            "poller uses setInterval, not hx-trigger: {html}"
        );
        assert!(
            html.contains("__dashTabLive"),
            "poller gated on __dashTabLive: {html}"
        );
    }

    #[test]
    fn index_passive_active_tab_renders_claimed_fragment_as_default_and_active() {
        let tabs = vec![TabNav {
            id: "chat".to_string(),
            title: "Chat".to_string(),
            self_refreshing: true,
        }];
        let html = DashboardTemplate::page_with_active_tab(
            RunSnapshot::empty(),
            tabs,
            "chat".to_string(),
            "<p id=\"chat-body\">hi</p>".to_string(),
        )
        .render()
        .expect("page renders");
        assert!(
            html.contains("<p id=\"chat-body\">hi</p>"),
            "claimed tab's fragment is the initial #content body: {html}"
        );
        let chat_btn = html.find("/tabs/chat").expect("chat button present");
        let snippet = &html[chat_btn.saturating_sub(60)..chat_btn];
        assert!(
            snippet.contains("dash-tab active"),
            "chat button carries active class: {snippet}"
        );
        let runs_btn = html.find("/content").expect("runs button present");
        let snippet = &html[runs_btn.saturating_sub(60)..runs_btn];
        assert!(
            !snippet.contains("active"),
            "Runs button is not active when a passive tab claims default: {snippet}"
        );
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
        // Dispatched runner + model (codex is OpenAI-only: no provider prefix).
        assert!(html.contains(">Runner<"), "runner label shown");
        assert!(html.contains(">codex<"), "dispatched runner shown");
        assert!(html.contains(">Model<"), "model label shown");
        assert!(
            html.contains(">gpt-5<"),
            "dispatched model shown without provider prefix"
        );
        // protocol_event unwrapped to log_row + text.
        assert!(html.contains("tool_call"), "log_row tag shown");
        assert!(
            html.contains("<details"),
            "tool_call renders as details block"
        );
        assert!(html.contains("ran bash"), "protocol text shown");
        // Non-protocol payload rendered raw.
        assert!(
            html.contains("dispatch attempt=0 pid=4242"),
            "raw payload shown"
        );
        // Formatted timestamp.
        assert!(html.contains("2026-06-12 00:00:30"), "formatted ts shown");
        // Close button clears the drawer.
        assert!(
            html.contains("getElementById('run-detail')"),
            "close button present"
        );
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
        assert!(
            !snippet.contains("disabled"),
            "Interrupt button enabled for live run"
        );
    }

    #[test]
    fn run_detail_shows_interrupt_kill_disabled_for_finished_run() {
        let html = RunDetailTemplate::build(sample_run(), Vec::new())
            .render()
            .expect("renders");
        // process_alive=false in sample_run, so buttons should have disabled
        assert!(
            html.contains("disabled"),
            "buttons disabled for finished run"
        );
    }

    #[test]
    fn model_display_unset_shows_dash() {
        assert_eq!(fmt_model_display("fake", None, None), "\u{2014}");
        assert_eq!(
            fmt_model_display("pi", Some(""), Some("anthropic")),
            "\u{2014}"
        );
    }

    #[test]
    fn model_display_multi_provider_prefixes() {
        assert_eq!(
            fmt_model_display("opencode", Some("claude-opus-4-8"), Some("anthropic")),
            "anthropic/claude-opus-4-8"
        );
        assert_eq!(
            fmt_model_display("pi", Some("claude-opus-4-8"), Some("anthropic")),
            "anthropic/claude-opus-4-8"
        );
    }

    #[test]
    fn model_display_codex_never_prefixes_provider() {
        // codex is OpenAI-only: provider is ignored, model shown alone.
        assert_eq!(
            fmt_model_display("codex", Some("gpt-5"), Some("openai")),
            "gpt-5"
        );
    }

    #[test]
    fn model_display_no_provider_shows_bare_model() {
        assert_eq!(
            fmt_model_display("pi", Some("some-model"), None),
            "some-model"
        );
    }

    #[test]
    fn content_header_renders_active_runner_and_model() {
        let mut s = RunSnapshot::empty();
        s.agent.runner = "opencode".to_string();
        s.agent.model = Some("claude-opus-4-8".to_string());
        s.agent.provider = Some("anthropic".to_string());
        let html = ContentTemplate::from_snapshot(s)
            .render()
            .expect("content renders");
        assert!(html.contains("opencode"), "active runner shown");
        assert!(
            html.contains("anthropic/claude-opus-4-8"),
            "active model shown with provider prefix"
        );
    }

    #[test]
    fn assistant_markdown_renders() {
        let ev = EventLine {
            event_id: 0,
            ts: "2026-06-12 10:30:45".to_string(),
            kind: "runner.codex".to_string(),
            row_type: "assistant".to_string(),
            text: "**bold** text".to_string(),
            detail: String::new(),
            rendered: String::new(),
        };
        let html = render_log_row(&ev);
        assert!(
            html.contains("<strong>bold</strong>"),
            "markdown rendered: {html}"
        );
    }

    #[test]
    fn assistant_html_in_text_is_escaped() {
        let ev = EventLine {
            event_id: 0,
            ts: "2026-06-12 10:30:45".to_string(),
            kind: "runner.codex".to_string(),
            row_type: "assistant".to_string(),
            text: "<script>alert(1)</script>".to_string(),
            detail: String::new(),
            rendered: String::new(),
        };
        let html = render_log_row(&ev);
        assert!(
            !html.contains("<script>"),
            "raw script tag not in output: {html}"
        );
        // pulldown-cmark treats bare angle brackets as inline HTML and escapes them;
        // the Rust string will contain either &lt;script&gt; or &amp;lt;script — both
        // indicate the tag is neutralised and will not execute in a browser.
        assert!(
            html.contains("&lt;script") || html.contains("&amp;lt;script"),
            "escaped: {html}"
        );
    }

    #[test]
    fn tool_call_renders_details_block() {
        let ev = EventLine {
            event_id: 0,
            ts: "2026-06-12 10:30:45".to_string(),
            kind: "runner.codex".to_string(),
            row_type: "tool_call".to_string(),
            text: "ls -la".to_string(),
            detail: "total 42\nfile1".to_string(),
            rendered: String::new(),
        };
        let html = render_log_row(&ev);
        assert!(html.contains("<details"), "details block: {html}");
        assert!(!html.contains(" open"), "not open by default: {html}");
        assert!(html.contains("ls -la"), "command shown: {html}");
        assert!(html.contains("total 42"), "output shown: {html}");
    }

    #[test]
    fn error_renders_callout() {
        let ev = EventLine {
            event_id: 0,
            ts: "2026-06-12 10:30:45".to_string(),
            kind: "runner.codex".to_string(),
            row_type: "error".to_string(),
            text: "connection refused".to_string(),
            detail: String::new(),
            rendered: String::new(),
        };
        let html = render_log_row(&ev);
        assert!(html.contains("log-error"), "error class: {html}");
        assert!(html.contains("connection refused"), "message shown: {html}");
    }
}
