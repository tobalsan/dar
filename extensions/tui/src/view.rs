//! Tab bar + per-tab rendering. Chat: transcript blocks (user/assistant/
//! thinking/tool/error/notice), streaming spinner while a turn is in flight,
//! single-line input. Logs: follow-tail window over the log ring, in
//! `frontend-log`'s exact line format. Dash: header + tables off the
//! orchestrator's retained snapshot (rows built in `dash::lines`).
//!
//! Chat lines are wrapped manually (char-chunked at the viewport width) so
//! the scroll math stays exact without ratatui's unstable line-count APIs.

use chrono::Utc;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Tabs};
use ratatui::Frame;

use crate::app::{App, Tab};
use crate::chat::{ChatBlock, ChatState};
use crate::dash;
use crate::logs::LogRow;

const SPINNER: [&str; 4] = ["|", "/", "-", "\\"];
/// Max visual rows the input box grows to before it scrolls internally.
const INPUT_MAX_ROWS: usize = 8;

pub fn render(frame: &mut Frame, app: &App) {
    let [tabs_area, content_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(frame.area());
    render_tab_bar(frame, tabs_area, app);
    match app.tab {
        Tab::Chat => render_chat(frame, content_area, app),
        Tab::Logs => render_logs(frame, content_area, app),
        Tab::Dash => render_dash(frame, content_area, app),
    }
}

fn render_tab_bar(frame: &mut Frame, area: Rect, app: &App) {
    let selected = app.tabs.iter().position(|tab| *tab == app.tab);
    let tabs = Tabs::new(app.tabs.iter().map(|tab| tab.title()))
        .select(selected)
        .highlight_style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD));
    frame.render_widget(tabs, area);
}

fn render_chat(frame: &mut Frame, area: Rect, app: &App) {
    // The input box grows with the buffer (1..=INPUT_MAX_ROWS visual rows,
    // plus a 2-row border); a single status line sits below it.
    let inner_width = (area.width as usize).saturating_sub(2).max(1);
    let input_rows = app
        .chat
        .input
        .layout(inner_width, INPUT_MAX_ROWS)
        .total_rows
        .clamp(1, INPUT_MAX_ROWS);
    let input_height = input_rows as u16 + 2; // + top/bottom border
    let [transcript_area, input_area, status_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(input_height),
        Constraint::Length(1),
    ])
    .areas(area);
    render_transcript(frame, transcript_area, app);
    render_input(frame, input_area, app);
    render_status(frame, status_area, app);
}

fn render_logs(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().title(" logs ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    if app.logs.unavailable {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "log topic unavailable (frontend-log not linked)",
                Style::new().add_modifier(Modifier::DIM),
            )),
            inner,
        );
        return;
    }
    let mut wrapped: Vec<Line> = Vec::new();
    for row in &app.logs.rows {
        let style = match row {
            LogRow::Event(_) => Style::new(),
            LogRow::Skipped(_) => Style::new().add_modifier(Modifier::DIM),
        };
        let text = row.text();
        for chunk in wrap_chunks(&text, (inner.width as usize).max(1)) {
            wrapped.push(Line::from(Span::styled(chunk, style)));
        }
    }
    let height = inner.height as usize;
    let max_start = wrapped.len().saturating_sub(height);
    let start = max_start.saturating_sub(app.logs.scroll_back);
    let visible: Vec<Line> = wrapped.into_iter().skip(start).take(height).collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

/// Dashboard pane: header + per-section tables straight off the retained
/// snapshot (the rows are built in `dash::lines`). A snapshot at version 0
/// has never been ticked, so there is nothing truthful to tabulate yet.
fn render_dash(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().title(" dashboard (p pause, r resume, s stop, q quit) ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    if app.dash.snapshot.version == 0 {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "waiting for first tick…",
                Style::new().add_modifier(Modifier::DIM),
            )),
            inner,
        );
        return;
    }
    let lines: Vec<Line> = dash::lines(&app.dash.snapshot, Utc::now())
        .into_iter()
        .map(Line::from)
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_transcript(frame: &mut Frame, area: Rect, app: &App) {
    let mut lines = transcript_lines(&app.chat, (area.width as usize).saturating_sub(2).max(1));
    if app.chat.in_flight {
        lines.push(Line::from(Span::styled(
            format!(
                "{} waiting for reply... (Esc aborts)",
                SPINNER[app.spinner_tick % SPINNER.len()]
            ),
            Style::new().add_modifier(Modifier::DIM),
        )));
    }
    let height = (area.height as usize).saturating_sub(2);
    let max_start = lines.len().saturating_sub(height);
    // Clamp scroll_back so usize::MAX (Ctrl+Home "oldest") lands at the top.
    let scroll_back = app.chat.scroll_back.min(max_start);
    let start = max_start - scroll_back;
    // Title shows a scroll cue while not following the tail.
    let title = if scroll_back > 0 {
        let shown_end = (start + height).min(lines.len());
        format!(" chat  [↑ {}/{} | End: follow] ", shown_end, lines.len())
    } else {
        " chat ".to_string()
    };
    let block = Block::bordered().title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let visible: Vec<Line> = lines.into_iter().skip(start).take(height).collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

fn render_input(frame: &mut Frame, area: Rect, app: &App) {
    let title = " message (Enter sends, Shift+Enter newline, Ctrl+C quits) ";
    let block = Block::bordered().title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    // Multi-line input: ask the buffer for the wrapped viewport slice and the
    // cursor position within it so what is drawn and where the caret sits
    // always agree (no more always-tail / pinned-at-end behavior).
    let width = inner.width as usize;
    let height = inner.height as usize;
    let layout = app.chat.input.layout(width, height);
    let lines: Vec<Line> = layout
        .rows
        .iter()
        .map(|row| Line::from(row.clone()))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
    let cursor_x = inner.x + layout.cursor_x.min(width.saturating_sub(1)) as u16;
    let cursor_y = inner.y + layout.cursor_y.min(height.saturating_sub(1)) as u16;
    frame.set_cursor_position(Position::new(cursor_x, cursor_y));
}

/// Status line under the input (Chat tab): agent name, provider+model, and a
/// best-effort context-usage readout. Fields the snapshot/runner don't supply
/// are omitted rather than shown wrong.
fn render_status(frame: &mut Frame, area: Rect, app: &App) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let mut parts: Vec<String> = Vec::new();
    let agent = &app.dash.snapshot.agent;
    if app.dash.snapshot.version > 0 && !agent.id.is_empty() {
        parts.push(agent.id.clone());
    }
    if let Some(workflow) = &agent.workflow {
        parts.push(workflow.clone());
    }
    match (&agent.provider, &agent.model) {
        (Some(provider), Some(model)) => parts.push(format!("{provider}/{model}")),
        (None, Some(model)) => parts.push(model.clone()),
        (Some(provider), None) => parts.push(provider.clone()),
        (None, None) => {}
    }
    if let Some(usage) = &app.chat.usage {
        parts.push(format_usage(usage));
    }
    let text = if parts.is_empty() {
        String::new()
    } else {
        format!(" {} ", parts.join("  ·  "))
    };
    frame.render_widget(
        Paragraph::new(Span::styled(
            text,
            Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM),
        )),
        area,
    );
}

/// `<used>k/<window>k (<pct>%)` when the window is known, else just `<used>k`
/// (accuracy over presence: never invent a percentage).
fn format_usage(usage: &crate::chat::ContextUsage) -> String {
    let used_k = (usage.tokens_used as f64 / 1000.0).round() as u64;
    match usage.context_window {
        Some(window) if window > 0 => {
            let window_k = (window as f64 / 1000.0).round() as u64;
            let pct = (usage.tokens_used as f64 / window as f64 * 100.0).round() as u64;
            format!("{used_k}k/{window_k}k ({pct}%)")
        }
        _ => format!("{used_k}k"),
    }
}

fn transcript_lines(chat: &ChatState, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for block in &chat.blocks {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        match block {
            ChatBlock::User(text) => push_wrapped(
                &mut lines,
                "> ",
                text,
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                width,
            ),
            ChatBlock::Assistant(text) => push_wrapped(&mut lines, "", text, Style::new(), width),
            ChatBlock::Thinking(text) => push_wrapped(
                &mut lines,
                "",
                text,
                Style::new().add_modifier(Modifier::DIM),
                width,
            ),
            ChatBlock::Tool {
                name,
                args,
                output,
                is_error,
                done,
                ..
            } => {
                let marker = if *done { "[tool]" } else { "[tool...]" };
                push_wrapped(
                    &mut lines,
                    "",
                    &format!("{marker} {name} {args}"),
                    Style::new().fg(Color::Yellow),
                    width,
                );
                if !output.is_empty() {
                    let style = if *is_error {
                        Style::new().fg(Color::Red)
                    } else {
                        Style::new().add_modifier(Modifier::DIM)
                    };
                    push_wrapped(&mut lines, "  ", output, style, width);
                }
            }
            ChatBlock::Error(text) => {
                push_wrapped(&mut lines, "! ", text, Style::new().fg(Color::Red), width)
            }
            ChatBlock::Notice(text) => {
                push_wrapped(&mut lines, "~ ", text, Style::new().fg(Color::Blue), width)
            }
            ChatBlock::Question {
                questions,
                done,
                rejected,
                answer,
                ..
            } => {
                let marker = if *done { "[question]" } else { "[question...]" };
                for q in questions {
                    push_wrapped(
                        &mut lines,
                        "",
                        &format!("{marker} {}", q.header),
                        Style::new().fg(Color::Magenta),
                        width,
                    );
                    push_wrapped(&mut lines, "  ", &q.question, Style::new(), width);
                    for (i, opt) in q.options.iter().enumerate() {
                        push_wrapped(
                            &mut lines,
                            "  ",
                            &format!("{}) {} — {}", i + 1, opt.label, opt.description),
                            Style::new().add_modifier(Modifier::DIM),
                            width,
                        );
                    }
                }
                if *done {
                    let style = if *rejected {
                        Style::new().fg(Color::Red)
                    } else {
                        Style::new().add_modifier(Modifier::DIM)
                    };
                    push_wrapped(&mut lines, "  ", &format!("-> {answer}"), style, width);
                } else {
                    let hint = if questions.len() > 1 {
                        "one number per question, space-separated".to_string()
                    } else {
                        let count = questions.first().map_or(0, |q| q.options.len());
                        format!("answer with a number (1-{count})")
                    };
                    push_wrapped(
                        &mut lines,
                        "  ",
                        &hint,
                        Style::new().add_modifier(Modifier::DIM),
                        width,
                    );
                }
            }
        }
    }
    lines
}

fn push_wrapped(
    lines: &mut Vec<Line<'static>>,
    prefix: &str,
    text: &str,
    style: Style,
    width: usize,
) {
    let body_width = width.saturating_sub(prefix.len()).max(1);
    let continuation = " ".repeat(prefix.len());
    let mut first = true;
    for raw in text.lines() {
        for chunk in wrap_chunks(raw, body_width) {
            let lead = if first { prefix } else { &continuation };
            lines.push(Line::from(Span::styled(format!("{lead}{chunk}"), style)));
            first = false;
        }
    }
    if first {
        lines.push(Line::from(Span::styled(prefix.to_string(), style)));
    }
}

fn wrap_chunks(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(width)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use cap_chat::{ChatEvent, QuestionInfo, QuestionOption};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use crate::app::{Action, App};
    use crate::chat::ChatBlock;

    use super::*;

    fn rendered(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(60, 18)).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn all_block_kinds_render() {
        let mut app = App::new();
        app.chat.blocks = vec![
            ChatBlock::User("what is failing?".to_string()),
            ChatBlock::Thinking("let me look".to_string()),
            ChatBlock::Tool {
                id: "call_1".to_string(),
                name: "bash".to_string(),
                args: "cargo test".to_string(),
                output: "2 passed".to_string(),
                is_error: false,
                done: true,
            },
            ChatBlock::Assistant("nothing is failing".to_string()),
            ChatBlock::Error("backend hiccup".to_string()),
        ];
        let screen = rendered(&app);
        assert!(screen.contains("> what is failing?"));
        assert!(screen.contains("let me look"));
        assert!(screen.contains("[tool] bash cargo test"));
        assert!(screen.contains("2 passed"));
        assert!(screen.contains("nothing is failing"));
        assert!(screen.contains("! backend hiccup"));
        assert!(screen.contains("message (Enter sends"));
    }

    #[test]
    fn long_lines_wrap_instead_of_truncating() {
        let mut app = App::new();
        app.chat.blocks = vec![ChatBlock::Assistant("x".repeat(80))];
        let screen = rendered(&app);
        // 80 chars cannot fit one 58-wide inner row; both wrapped rows show.
        assert!(screen.contains(&"x".repeat(58)));
        assert!(screen.contains(&"x".repeat(22)));
    }

    #[test]
    fn in_flight_turn_shows_spinner_without_gating_the_input() {
        let mut app = App::new();
        app.chat.input.insert_str("hello");
        assert_eq!(
            app.handle_event(key(KeyCode::Enter)),
            Action::Submit("hello".to_string())
        );
        let screen = rendered(&app);
        assert!(screen.contains("waiting for reply... (Esc aborts)"));
        assert!(screen.contains("message "));
        assert!(!screen.contains("Enter disabled"));

        // Steering: Enter while in flight accepts and renders the user input.
        app.chat.input.insert_str("queued");
        assert_eq!(
            app.handle_event(key(KeyCode::Enter)),
            Action::Submit("queued".to_string())
        );
        assert!(rendered(&app).contains("> queued"));
    }

    #[test]
    fn esc_abort_path_clears_spinner_and_reports_the_abort() {
        let mut app = App::new();
        app.chat.input.insert_str("long job");
        app.handle_event(key(KeyCode::Enter));
        assert_eq!(app.handle_event(key(KeyCode::Esc)), Action::AbortTurn);
        // Backend confirms the abort with an aborted TurnFinished.
        app.chat.apply_event(ChatEvent::TurnFinished {
            ok: false,
            error: Some("aborted".to_string()),
        });
        let screen = rendered(&app);
        assert!(!screen.contains("waiting for reply"));
        assert!(screen.contains("! turn aborted"));
        assert!(screen.contains("message (Enter sends"));
    }

    fn log_event(message: &str) -> host_api::LogEvent {
        host_api::LogEvent {
            time: "2026-07-18 10:00:00".to_string(),
            level: "INFO".to_string(),
            target: "issue=- event=test".to_string(),
            message: message.to_string(),
        }
    }

    #[test]
    fn tab_bar_lists_both_tabs_and_tab_key_switches_the_pane() {
        let mut app = App::new();
        let chat_screen = rendered(&app);
        assert!(chat_screen.contains("Chat"));
        assert!(chat_screen.contains("Logs"));
        assert!(chat_screen.contains(" chat "));

        app.handle_event(key(KeyCode::Tab));
        let logs_screen = rendered(&app);
        assert!(logs_screen.contains("Chat"));
        assert!(logs_screen.contains("Logs"));
        assert!(logs_screen.contains(" logs "));
        assert!(!logs_screen.contains("message (Enter sends"));
    }

    #[test]
    fn logs_tab_follows_the_tail_scrolls_back_and_end_refollows() {
        let mut app = App::new();
        app.tab = crate::app::Tab::Logs;
        for i in 0..40 {
            app.logs.push_event(log_event(&format!("row-{i}")));
        }
        let tail = rendered(&app);
        assert!(tail.contains("2026-07-18 10:00:00 INFO issue=- event=test row-39"));
        assert!(!tail.contains("row-0 "));

        app.logs.scroll_up(1000);
        let top = rendered(&app);
        assert!(top.contains("row-0 "));
        assert!(!top.contains("row-39"));

        app.logs.follow_tail();
        assert!(rendered(&app).contains("row-39"));
    }

    #[test]
    fn lagged_rows_render_as_the_skipped_marker() {
        let mut app = App::new();
        app.tab = crate::app::Tab::Logs;
        app.logs.push_event(log_event("before"));
        app.logs.push_skipped(12);
        app.logs.push_event(log_event("after"));
        let screen = rendered(&app);
        assert!(screen.contains("… 12 log lines skipped"));
        assert!(screen.contains("before"));
        assert!(screen.contains("after"));
    }

    #[test]
    fn unavailable_logs_show_the_placeholder_instead_of_rows() {
        let mut app = App::new();
        app.tab = crate::app::Tab::Logs;
        app.logs.unavailable = true;
        let screen = rendered(&app);
        assert!(screen.contains("log topic unavailable (frontend-log not linked)"));
    }

    /// End-to-end through the production feed path: a startup-style banner
    /// LogEvent published on the retained topic must show up in the rendered
    /// Logs tab (the M0 dashboard-URL banner appears here naturally).
    #[tokio::test]
    async fn published_startup_banner_appears_in_the_logs_render() {
        let mut bus = host_api::EventBus::new();
        bus.register_broadcast::<host_api::LogEvent>(host_api::LOG_EVENTS_TOPIC, 1024)
            .unwrap();
        bus.register_retained::<Option<host_api::LogEvent>>(host_api::STARTUP_BANNER_TOPIC, None)
            .unwrap();
        let mut app = App::new();
        app.tab = crate::app::Tab::Logs;
        let mut feed = crate::logs::LogFeed::subscribe(&bus, &mut app.logs);

        bus.publish(
            host_api::STARTUP_BANNER_TOPIC,
            Some(host_api::LogEvent {
                time: "2026-07-18 10:00:00".to_string(),
                level: "INFO".to_string(),
                target: "issue=- event=startup".to_string(),
                message: "dar running; dashboard on http://127.0.0.1:7878/".to_string(),
            }),
        )
        .unwrap();
        let delivery = feed.next().await;
        feed.apply(delivery, &mut app.logs);

        let screen = rendered(&app);
        assert!(screen.contains("2026-07-18 10:00:00 INFO issue=- event=startup dar running"));
    }

    // -- dash tab -----------------------------------------------------------

    /// Wider buffer than `rendered`: dash header lines exceed 60 columns and
    /// `Paragraph` clips (it never wraps here, matching the logs pane).
    fn rendered_wide(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn dash_app(snapshot: orchestrator_api::RunSnapshot) -> App {
        let mut app = App::new();
        app.enable_dash();
        app.tab = crate::app::Tab::Dash;
        app.dash.snapshot = snapshot;
        app
    }

    #[test]
    fn version_zero_snapshot_renders_the_waiting_placeholder() {
        let app = dash_app(orchestrator_api::RunSnapshot::empty());
        let screen = rendered_wide(&app);
        assert!(screen.contains("waiting for first tick…"));
        assert!(!screen.contains("active runs"));
    }

    #[test]
    fn empty_ticked_snapshot_renders_header_and_empty_tables() {
        let mut snapshot = orchestrator_api::RunSnapshot::empty();
        snapshot.version = 1;
        snapshot.agent.id = "demo".to_string();
        snapshot.agent.tracker = "files".to_string();
        snapshot.agent.runner = "fake".to_string();
        let screen = rendered_wide(&dash_app(snapshot));
        assert!(screen.contains("agent demo | tracker files | runner fake | last tick never"));
        assert!(!screen.contains("PAUSED"));
        assert!(screen.contains("active runs (0):"));
        assert!(screen.contains("queue (0):"));
        assert!(screen.contains("retry (0):"));
        assert!(screen.contains("history (0):"));
        assert!(screen.contains("events (0):"));
        assert!(screen.contains("(none)"));
        assert!(!screen.contains("waiting for first tick"));
    }

    #[test]
    fn populated_snapshot_renders_badge_tables_and_events_tail() {
        let now = chrono::Utc::now();
        let mut snapshot = orchestrator_api::RunSnapshot::empty();
        snapshot.version = 5;
        snapshot.agent.id = "demo".to_string();
        snapshot.agent.tracker = "files".to_string();
        snapshot.agent.runner = "fake".to_string();
        snapshot.paused = true;
        snapshot.last_tick_at = Some(now);
        snapshot.rate_limit_min_remaining = Some(17);
        snapshot.active_runs.push(orchestrator_api::ActiveRun {
            run_id: "run-1".to_string(),
            identifier: "ISSUE-1".to_string(),
            state: "doing".to_string(),
            workspace: String::new(),
            pid: 321,
            started_at: now,
            last_event: "spawned".to_string(),
            status: orchestrator_api::RunStatus::Running,
        });
        snapshot.queue.push(orchestrator_api::QueueItem {
            identifier: "ISSUE-2".to_string(),
            title: "Next up".to_string(),
            state: "todo".to_string(),
            priority: None,
            created_at: None,
        });
        snapshot.retry.push(orchestrator_api::RetryItem {
            identifier: "ISSUE-3".to_string(),
            attempt: 1,
            due_at: now,
            last_error: "boom".to_string(),
        });
        snapshot.history.push(orchestrator_api::HistoryEntry {
            identifier: "ISSUE-4".to_string(),
            status: orchestrator_api::RunStatus::Failed,
            pid: 7,
            ended_at: now,
            note: "retries exhausted".to_string(),
        });
        snapshot.events.push("ISSUE-1 dispatched".to_string());

        let screen = rendered_wide(&dash_app(snapshot));
        assert!(screen.contains("PAUSED"));
        assert!(screen.contains("rate limit 17min remaining"));
        assert!(screen.contains("ISSUE-1  Running  state=doing  pid=321"));
        assert!(screen.contains("ISSUE-2  state=todo  priority=-  Next up"));
        assert!(screen.contains("ISSUE-3  attempt=1  due now  boom"));
        assert!(screen.contains("ISSUE-4  Failed"));
        assert!(screen.contains("ISSUE-1 dispatched"));
        assert!(screen.contains("p pause, r resume, s stop, q quit"));
    }

    #[test]
    fn dash_tab_is_absent_from_the_bar_unless_enabled() {
        // Mirrors the production wiring: enable_dash is only called when
        // DashFeed::subscribe succeeded, so an unregistered snapshot topic
        // leaves the tab out of the bar entirely (no placeholder).
        let mut app = App::new();
        assert!(
            crate::dash::DashFeed::subscribe(&host_api::EventBus::new(), &mut app.dash).is_none()
        );
        assert!(!rendered(&app).contains("Dash"));

        let mut bus = host_api::EventBus::new();
        bus.register_retained::<orchestrator_api::RunSnapshot>(
            orchestrator_api::RUN_SNAPSHOT_TOPIC,
            orchestrator_api::RunSnapshot::empty(),
        )
        .unwrap();
        let mut app = App::new();
        assert!(crate::dash::DashFeed::subscribe(&bus, &mut app.dash).is_some());
        app.enable_dash();
        assert!(rendered(&app).contains("Dash"));
    }

    #[test]
    fn scroll_back_moves_the_window_and_follow_returns_to_tail() {
        let mut app = App::new();
        app.chat.blocks = (0..40)
            .map(|i| ChatBlock::Assistant(format!("line-{i}")))
            .collect();
        let tail = rendered(&app);
        assert!(tail.contains("line-39"));
        assert!(!tail.contains("line-0\n") && !tail.contains("line-0 "));

        app.chat.scroll_up(1000);
        let top = rendered(&app);
        assert!(top.contains("line-0"));
        assert!(!top.contains("line-39"));

        app.chat.follow_tail();
        assert!(rendered(&app).contains("line-39"));
    }

    #[test]
    fn status_line_shows_agent_provider_model_and_omits_unknown_usage() {
        let mut app = App::new();
        app.dash.snapshot.version = 1;
        app.dash.snapshot.agent.id = "demo".to_string();
        app.dash.snapshot.agent.provider = Some("vibeproxy".to_string());
        app.dash.snapshot.agent.model = Some("claude-opus-4-8".to_string());
        let screen = rendered(&app);
        assert!(screen.contains("demo"));
        assert!(screen.contains("vibeproxy/claude-opus-4-8"));
        // No usage reported yet: no token readout on the status line.
        assert!(!screen.contains("k/"));
        assert!(!screen.contains("%"));
    }

    #[test]
    fn status_line_appends_workflow_label_when_non_default() {
        let mut app = App::new();
        app.dash.snapshot.version = 1;
        app.dash.snapshot.agent.id = "demo".to_string();
        app.dash.snapshot.agent.workflow = Some("triage".to_string());
        let screen = rendered(&app);
        assert!(screen.contains("demo"));
        assert!(screen.contains("triage"));
    }

    #[test]
    fn status_line_renders_context_usage_when_window_is_known() {
        let mut app = App::new();
        app.dash.snapshot.version = 1;
        app.dash.snapshot.agent.id = "demo".to_string();
        app.chat.usage = Some(crate::chat::ContextUsage {
            tokens_used: 10_928,
            context_window: Some(200_000),
        });
        let screen = rendered(&app);
        // 10928 -> 11k, 200000 -> 200k, 5%.
        assert!(screen.contains("11k/200k (5%)"), "got: {screen}");
    }

    #[test]
    fn status_line_degrades_to_raw_tokens_without_a_window() {
        let mut app = App::new();
        app.chat.usage = Some(crate::chat::ContextUsage {
            tokens_used: 12_400,
            context_window: None,
        });
        let screen = rendered(&app);
        assert!(screen.contains("12k"));
        assert!(!screen.contains("%"));
    }

    #[test]
    fn scroll_indicator_appears_only_when_scrolled_back() {
        let mut app = App::new();
        app.chat.blocks = (0..40)
            .map(|i| ChatBlock::Assistant(format!("line-{i}")))
            .collect();
        // Following the tail: plain " chat " title, no cue.
        assert!(rendered(&app).contains(" chat "));
        app.chat.scroll_up(5);
        let screen = rendered(&app);
        assert!(screen.contains("End: follow"), "got: {screen}");
    }

    fn question(header: &str) -> QuestionInfo {
        QuestionInfo {
            header: header.to_string(),
            question: "Which one?".to_string(),
            options: vec![
                QuestionOption {
                    label: "A".to_string(),
                    description: "first option".to_string(),
                },
                QuestionOption {
                    label: "B".to_string(),
                    description: String::new(),
                },
            ],
            multiple: false,
            custom: false,
        }
    }

    #[test]
    fn pending_question_renders_marker_header_options_and_hint() {
        let mut app = App::new();
        app.chat.blocks = vec![ChatBlock::Question {
            request_id: "req-1".to_string(),
            questions: vec![question("Pick a color")],
            done: false,
            rejected: false,
            answer: String::new(),
        }];
        let screen = rendered(&app);
        assert!(screen.contains("[question...]"));
        assert!(screen.contains("Pick a color"));
        assert!(screen.contains("1) "));
        assert!(screen.contains("answer with a number (1-2)"));
    }

    #[test]
    fn resolved_question_renders_answered_marker_and_summary() {
        let mut app = App::new();
        app.chat.blocks = vec![ChatBlock::Question {
            request_id: "req-1".to_string(),
            questions: vec![question("Pick a color")],
            done: true,
            rejected: false,
            answer: "A".to_string(),
        }];
        let screen = rendered(&app);
        assert!(screen.contains("[question]"));
        assert!(screen.contains("-> A"));
    }

    #[test]
    fn multiline_input_renders_each_line_and_grows_the_box() {
        let mut app = App::new();
        app.chat.input.insert_str("first line\nsecond line");
        let screen = rendered(&app);
        assert!(screen.contains("first line"));
        assert!(screen.contains("second line"));
    }
}
