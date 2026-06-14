//! Dash tab state: the orchestrator's retained [`RunSnapshot`] rendered as
//! text rows, plus the Stop/Pause/Resume key controls. The tab exists only
//! when `subscribe_retained::<RunSnapshot>` succeeds at startup (orchestrator
//! linked — see the crate docs for the deliberate spec reading), and the
//! controls are fire-and-forget [`ControlMsg`] publishes mutating no local
//! state: the orchestrator stays the single writer of run state (including
//! `paused`), so the badge flips only once the next snapshot reflects it —
//! exactly the web dashboard's usage.

use chrono::{DateTime, Utc};
use host_api::EventBus;
use orchestrator_api::{ControlMsg, RunSnapshot, CONTROL_TOPIC, RUN_SNAPSHOT_TOPIC};
use tokio::sync::watch;

/// Rows shown for the recent-history and events tails (newest kept).
const TAIL: usize = 10;

/// The Dash keymap's controls. A separate enum (not [`ControlMsg`] itself)
/// because `ControlMsg` carries reply channels and derives neither `Debug`
/// nor `PartialEq`, which `Action` needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    Pause,
    Resume,
    Stop,
}

/// Fire-and-forget publish on `orchestrator.control`. Failures are dropped
/// deliberately: the Dash tab only exists when the orchestrator is linked,
/// and there is no reply to wait for (Stop/Pause/Resume carry none).
pub fn publish_control(bus: &EventBus, control: Control) {
    let msg = match control {
        Control::Pause => ControlMsg::Pause,
        Control::Resume => ControlMsg::Resume,
        Control::Stop => ControlMsg::Stop,
    };
    let _ = bus.publish(CONTROL_TOPIC, msg);
}

/// Dash tab state: the latest retained snapshot, rendered as-is.
pub struct DashState {
    pub snapshot: RunSnapshot,
}

impl Default for DashState {
    fn default() -> Self {
        Self {
            snapshot: RunSnapshot::empty(),
        }
    }
}

/// The Dash tab's bus feed over the retained snapshot topic.
pub struct DashFeed {
    rx: watch::Receiver<RunSnapshot>,
}

impl DashFeed {
    /// Subscribe to the retained snapshot topic. `None` when the topic is
    /// unregistered (orchestrator not linked): the caller must then leave the
    /// Dash tab out of the tab list entirely. On `Some`, the currently
    /// retained snapshot has been seeded into `dash`.
    pub fn subscribe(bus: &EventBus, dash: &mut DashState) -> Option<Self> {
        let mut rx = bus
            .subscribe_retained::<RunSnapshot>(RUN_SNAPSHOT_TOPIC)
            .ok()?;
        dash.snapshot = rx.borrow_and_update().clone();
        Some(Self { rx })
    }

    /// Wait for the next snapshot. `None` once the topic owner is gone: stop
    /// polling, keep the last snapshot visible.
    pub async fn next(&mut self) -> Option<RunSnapshot> {
        self.rx.changed().await.ok()?;
        Some(self.rx.borrow_and_update().clone())
    }
}

/// All dashboard rows for one snapshot: the header line, then one text table
/// per snapshot section. The caller has already handled `version == 0`.
pub fn lines(snapshot: &RunSnapshot, now: DateTime<Utc>) -> Vec<String> {
    let mut out = vec![header(snapshot, now), String::new()];
    section(
        &mut out,
        format!("active runs ({})", snapshot.active_runs.len()),
        snapshot.active_runs.iter().map(|run| {
            format!(
                "  {}  {:?}  state={}  pid={}  started {}  {}",
                run.identifier,
                run.status,
                run.state,
                run.pid,
                age(now, run.started_at),
                run.last_event,
            )
        }),
    );
    section(
        &mut out,
        format!("queue ({})", snapshot.queue.len()),
        snapshot.queue.iter().map(|item| {
            format!(
                "  {}  state={}  priority={}  {}",
                item.identifier,
                item.state,
                item.priority
                    .map_or_else(|| "-".to_string(), |p| p.to_string()),
                item.title,
            )
        }),
    );
    section(
        &mut out,
        format!("retry ({})", snapshot.retry.len()),
        snapshot.retry.iter().map(|item| {
            format!(
                "  {}  attempt={}  {}  {}",
                item.identifier,
                item.attempt,
                due(now, item.due_at),
                item.last_error,
            )
        }),
    );
    section(
        &mut out,
        tail_label("history", snapshot.history.len()),
        // Snapshot is already newest-first; take the head so a vertically
        // clipped pane still shows the runs that just finished.
        snapshot.history.iter().take(TAIL).map(|entry| {
            format!(
                "  {}  {:?}  {}  {}",
                entry.identifier,
                entry.status,
                age(now, entry.ended_at),
                entry.note,
            )
        }),
    );
    let tail_start = snapshot.events.len().saturating_sub(TAIL);
    section(
        &mut out,
        tail_label("events", snapshot.events.len()),
        snapshot.events[tail_start..]
            .iter()
            .map(|event| format!("  {event}")),
    );
    out
}

/// agent id/tracker/runner, the paused badge, last-tick age, and the
/// rate-limit reading when the snapshot carries one.
fn header(snapshot: &RunSnapshot, now: DateTime<Utc>) -> String {
    let agent = &snapshot.agent;
    let mut header = format!(
        "agent {} | tracker {} | runner {}",
        agent.id, agent.tracker, agent.runner
    );
    if snapshot.paused {
        header.push_str(" | PAUSED");
    }
    let tick = match snapshot.last_tick_at {
        Some(at) => age(now, at),
        None => "never".to_string(),
    };
    header.push_str(&format!(" | last tick {tick}"));
    if let Some(minutes) = snapshot.rate_limit_min_remaining {
        header.push_str(&format!(" | rate limit {minutes}min remaining"));
    }
    header
}

/// Section header for the tailed tables: total count plus how many show.
fn tail_label(name: &str, total: usize) -> String {
    if total > TAIL {
        format!("{name} (last {TAIL} of {total})")
    } else {
        format!("{name} ({total})")
    }
}

/// Push a `header:` row plus the item rows (or a `(none)` marker), followed
/// by a separating blank row.
fn section(out: &mut Vec<String>, header: String, rows: impl Iterator<Item = String>) {
    out.push(format!("{header}:"));
    let before = out.len();
    out.extend(rows);
    if out.len() == before {
        out.push("  (none)".to_string());
    }
    out.push(String::new());
}

fn age(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let secs = (now - then).num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}

fn due(now: DateTime<Utc>, at: DateTime<Utc>) -> String {
    let secs = (at - now).num_seconds();
    if secs <= 0 {
        "due now".to_string()
    } else {
        format!("due in {secs}s")
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use orchestrator_api::{ActiveRun, HistoryEntry, QueueItem, RetryItem, RunStatus};

    use crate::app::{Action, App, Tab};

    use super::*;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    /// The control topic exactly as the orchestrator registers it.
    fn control_bus() -> EventBus {
        let mut bus = EventBus::new();
        bus.register_broadcast::<ControlMsg>(CONTROL_TOPIC, 16)
            .unwrap();
        bus
    }

    fn snapshot_bus(snapshot: RunSnapshot) -> EventBus {
        let mut bus = EventBus::new();
        bus.register_retained::<RunSnapshot>(RUN_SNAPSHOT_TOPIC, snapshot)
            .unwrap();
        bus
    }

    fn populated_snapshot(now: DateTime<Utc>) -> RunSnapshot {
        let mut snapshot = RunSnapshot::empty();
        snapshot.version = 7;
        snapshot.agent.id = "demo".to_string();
        snapshot.agent.tracker = "files".to_string();
        snapshot.agent.runner = "claude".to_string();
        snapshot.paused = true;
        snapshot.last_tick_at = Some(now - Duration::seconds(3));
        snapshot.rate_limit_min_remaining = Some(42);
        snapshot.active_runs.push(ActiveRun {
            run_id: "run-1".to_string(),
            identifier: "ISSUE-1".to_string(),
            state: "doing".to_string(),
            workspace: String::new(),
            pid: 123,
            started_at: now - Duration::seconds(65),
            last_event: "tool: cargo test".to_string(),
            status: RunStatus::Running,
        });
        snapshot.queue.push(QueueItem {
            identifier: "ISSUE-2".to_string(),
            title: "Fix the flaky test".to_string(),
            state: "todo".to_string(),
            priority: Some(1),
            created_at: None,
        });
        snapshot.retry.push(RetryItem {
            identifier: "ISSUE-3".to_string(),
            attempt: 2,
            due_at: now + Duration::seconds(30),
            last_error: "exit code 1".to_string(),
        });
        snapshot.history.push(HistoryEntry {
            identifier: "ISSUE-4".to_string(),
            status: RunStatus::Succeeded,
            pid: 99,
            ended_at: now - Duration::seconds(7200),
            note: "clean exit".to_string(),
        });
        snapshot.events.push("ISSUE-1 dispatched".to_string());
        snapshot
    }

    // -- key → publish mapping -------------------------------------------

    #[tokio::test]
    async fn dash_keys_publish_pause_resume_stop_on_the_control_topic() {
        let bus = control_bus();
        let mut rx = bus.subscribe::<ControlMsg>(CONTROL_TOPIC).unwrap();
        let mut app = App::new();
        app.enable_dash();
        app.tab = Tab::Dash;

        for (code, expected) in [
            (KeyCode::Char('p'), Control::Pause),
            (KeyCode::Char('r'), Control::Resume),
            (KeyCode::Char('s'), Control::Stop),
        ] {
            let action = app.handle_event(key(code));
            assert_eq!(action, Action::Control(expected));
            let Action::Control(control) = action else {
                unreachable!()
            };
            publish_control(&bus, control);
        }
        assert!(matches!(rx.recv().await.unwrap(), ControlMsg::Pause));
        assert!(matches!(rx.recv().await.unwrap(), ControlMsg::Resume));
        assert!(matches!(rx.recv().await.unwrap(), ControlMsg::Stop));
    }

    #[test]
    fn dash_keys_mutate_no_local_state() {
        // Single-writer discipline: pressing pause never flips anything in
        // the TUI — the paused badge waits for the orchestrator's snapshot.
        let mut app = App::new();
        app.enable_dash();
        app.tab = Tab::Dash;
        assert!(!app.dash.snapshot.paused);
        app.handle_event(key(KeyCode::Char('p')));
        assert!(!app.dash.snapshot.paused);
    }

    #[test]
    fn publish_without_the_topic_is_dropped_not_a_panic() {
        publish_control(&EventBus::new(), Control::Stop);
    }

    // -- feed --------------------------------------------------------------

    #[test]
    fn unregistered_topic_yields_no_feed() {
        let mut dash = DashState::default();
        assert!(DashFeed::subscribe(&EventBus::new(), &mut dash).is_none());
    }

    #[tokio::test]
    async fn subscribe_seeds_the_retained_snapshot_and_next_delivers_updates() {
        let now = Utc::now();
        let bus = snapshot_bus(populated_snapshot(now));
        let mut dash = DashState::default();
        let mut feed = DashFeed::subscribe(&bus, &mut dash).unwrap();
        assert_eq!(dash.snapshot.version, 7, "retained snapshot seeded");

        let mut next = populated_snapshot(now);
        next.version = 8;
        next.paused = false;
        bus.publish(RUN_SNAPSHOT_TOPIC, next).unwrap();
        let snapshot = feed.next().await.unwrap();
        assert_eq!(snapshot.version, 8);
        assert!(!snapshot.paused);

        drop(bus); // topic owner gone: the feed ends instead of spinning
        assert!(feed.next().await.is_none());
    }

    // -- row content --------------------------------------------------------

    #[test]
    fn header_carries_agent_badge_tick_age_and_rate_limit() {
        let now = Utc::now();
        let rows = lines(&populated_snapshot(now), now);
        assert_eq!(
            rows[0],
            "agent demo | tracker files | runner claude | PAUSED \
             | last tick 3s ago | rate limit 42min remaining"
        );
    }

    #[test]
    fn header_omits_badge_and_rate_limit_when_absent() {
        let now = Utc::now();
        let mut snapshot = populated_snapshot(now);
        snapshot.paused = false;
        snapshot.rate_limit_min_remaining = None;
        snapshot.last_tick_at = None;
        let rows = lines(&snapshot, now);
        assert_eq!(
            rows[0],
            "agent demo | tracker files | runner claude | last tick never"
        );
    }

    #[test]
    fn tables_render_one_row_per_snapshot_item() {
        let now = Utc::now();
        let rows = lines(&populated_snapshot(now), now).join("\n");
        assert!(rows.contains("active runs (1):"));
        assert!(rows.contains("  ISSUE-1  Running  state=doing  pid=123  started 1m ago  tool: cargo test"));
        assert!(rows.contains("queue (1):"));
        assert!(rows.contains("  ISSUE-2  state=todo  priority=1  Fix the flaky test"));
        assert!(rows.contains("retry (1):"));
        assert!(rows.contains("  ISSUE-3  attempt=2  due in 30s  exit code 1"));
        assert!(rows.contains("history (1):"));
        assert!(rows.contains("  ISSUE-4  Succeeded  2h ago  clean exit"));
        assert!(rows.contains("events (1):"));
        assert!(rows.contains("  ISSUE-1 dispatched"));
    }

    #[test]
    fn empty_sections_show_the_none_marker() {
        let mut snapshot = RunSnapshot::empty();
        snapshot.version = 1;
        let now = Utc::now();
        let rows = lines(&snapshot, now).join("\n");
        for label in [
            "active runs (0):",
            "queue (0):",
            "retry (0):",
            "history (0):",
            "events (0):",
        ] {
            assert!(rows.contains(label), "missing {label:?}");
        }
        assert_eq!(rows.matches("  (none)").count(), 5);
    }

    #[test]
    fn history_and_events_keep_only_the_newest_tail() {
        let now = Utc::now();
        let mut snapshot = RunSnapshot::empty();
        snapshot.version = 2;
        for i in 0..15 {
            snapshot.history.push(HistoryEntry {
                identifier: format!("ISSUE-{i}"),
                status: RunStatus::Succeeded,
                pid: 0,
                ended_at: now,
                note: String::new(),
            });
            snapshot.events.push(format!("event-{i}"));
        }
        let rows = lines(&snapshot, now).join("\n");
        assert!(rows.contains("history (last 10 of 15):"));
        assert!(rows.contains("ISSUE-14"), "newest history entry shows");
        assert!(!rows.contains("ISSUE-4 "), "oldest entries dropped");
        assert!(rows.contains("events (last 10 of 15):"));
        assert!(rows.contains("  event-14"));
        assert!(!rows.contains("  event-4\n"));
    }

    #[test]
    fn retry_rows_past_their_due_time_read_due_now() {
        let now = Utc::now();
        let mut snapshot = populated_snapshot(now);
        snapshot.retry[0].due_at = now - Duration::seconds(5);
        let rows = lines(&snapshot, now).join("\n");
        assert!(rows.contains("  ISSUE-3  attempt=2  due now  exit code 1"));
    }
}
