//! Chat pane state: transcript blocks, the single-line input, the
//! one-turn-in-flight gate, the TUI-side turn timer, and the one-shot
//! context preamble prepended to the first outbound prompt.

use std::path::Path;
use std::time::{Duration, Instant};

use cap_chat::{ChatEvent, ChatRole};
use orchestrator_api::RunSnapshot;

/// Max lines shown per snapshot section (active/queue/history), keeping the
/// whole snapshot summary within the spec'd ~30-line cap (1 + 3·(1+8)).
const PREAMBLE_SECTION_CAP: usize = 8;
/// Max `issues/` entries listed in the preamble.
const PREAMBLE_ISSUE_CAP: usize = 20;

/// Build the one-shot context preamble prepended to the first outbound
/// prompt: an orientation paragraph, a best-effort orchestrator snapshot
/// summary (agent id/folder + active/queue/recent-history, capped ~30
/// lines), and a bounded listing of `<agent_root>/issues/` when present.
pub fn build_preamble(snapshot: Option<&RunSnapshot>, agent_root: &Path) -> String {
    let mut out = format!(
        "[context] This is an operator chat session inside the agentropy agent folder at {}. \
         Issues live at ./issues (the tracker owns their state: field); per-issue \
         workspaces at ./workspaces. You run trusted with full access to this folder. \
         The operator's message follows this context.\n",
        agent_root.display()
    );
    if let Some(snapshot) = snapshot.filter(|s| s.version > 0) {
        out.push('\n');
        out.push_str(&snapshot_summary(snapshot));
    }
    if let Some(listing) = issues_listing(&agent_root.join("issues")) {
        out.push('\n');
        out.push_str(&listing);
    }
    out
}

fn snapshot_summary(snapshot: &RunSnapshot) -> String {
    let mut lines = vec![format!(
        "Orchestrator snapshot (agent \"{}\", folder {}, tracker {}, runner {}{}):",
        snapshot.agent.id,
        snapshot.agent.folder,
        snapshot.agent.tracker,
        snapshot.agent.runner,
        if snapshot.paused { ", PAUSED" } else { "" },
    )];
    push_capped(
        &mut lines,
        "active runs",
        snapshot.active_runs.len(),
        snapshot.active_runs.iter().map(|run| {
            format!(
                "- {} {:?} state={} pid={}",
                run.identifier, run.status, run.state, run.pid
            )
        }),
    );
    push_capped(
        &mut lines,
        "queue",
        snapshot.queue.len(),
        snapshot.queue.iter().map(|item| {
            format!(
                "- {} state={} priority={} {}",
                item.identifier,
                item.state,
                item.priority.map_or_else(|| "-".to_string(), |p| p.to_string()),
                item.title
            )
        }),
    );
    push_capped(
        &mut lines,
        "recent history",
        snapshot.history.len(),
        // Most recent entries first.
        snapshot.history.iter().rev().map(|entry| {
            format!(
                "- {} {:?} at {} {}",
                entry.identifier, entry.status, entry.ended_at, entry.note
            )
        }),
    );
    lines.join("\n") + "\n"
}

/// Push a `label (total):` header plus at most [`PREAMBLE_SECTION_CAP`] item
/// lines, with an overflow marker when items were dropped.
fn push_capped(
    lines: &mut Vec<String>,
    label: &str,
    total: usize,
    items: impl Iterator<Item = String>,
) {
    lines.push(format!("{label} ({total}):"));
    lines.extend(items.take(PREAMBLE_SECTION_CAP));
    if total > PREAMBLE_SECTION_CAP {
        // The overflow marker takes the cap'th item line's place, so each
        // section stays at one header + PREAMBLE_SECTION_CAP lines.
        *lines.last_mut().expect("header pushed above") =
            format!("- ... and {} more", total - PREAMBLE_SECTION_CAP + 1);
    }
}

/// Bounded listing of the issues directory: file name + the first `state:`
/// and `title:` lines found in each file. `None` when the directory is
/// absent/unreadable (e.g. a non-files tracker).
fn issues_listing(dir: &Path) -> Option<String> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_file())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let total = names.len();
    let mut lines = vec![format!("Issue files in ./issues ({total}):")];
    for name in names.iter().take(PREAMBLE_ISSUE_CAP) {
        match issue_summary(&dir.join(name)) {
            Some(summary) => lines.push(format!("- {name} {summary}")),
            None => lines.push(format!("- {name}")),
        }
    }
    if total > PREAMBLE_ISSUE_CAP {
        lines.push(format!("- ... and {} more", total - PREAMBLE_ISSUE_CAP));
    }
    Some(lines.join("\n") + "\n")
}

/// First `state:` and `title:` lines of one issue file, scanning only the
/// frontmatter-sized head of the file.
fn issue_summary(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut state = None;
    let mut title = None;
    for line in content.lines().take(20) {
        let line = line.trim();
        if state.is_none() {
            if let Some(value) = line.strip_prefix("state:") {
                state = Some(value.trim().to_string());
            }
        }
        if title.is_none() {
            if let Some(value) = line.strip_prefix("title:") {
                title = Some(value.trim().trim_matches('"').to_string());
            }
        }
        if state.is_some() && title.is_some() {
            break;
        }
    }
    if state.is_none() && title.is_none() {
        return None;
    }
    let mut summary = format!("[state: {}]", state.as_deref().unwrap_or("?"));
    if let Some(title) = title.filter(|t| !t.is_empty()) {
        summary.push(' ');
        summary.push_str(&title);
    }
    Some(summary)
}

/// One rendered transcript unit. Streamed deltas append to the last block of
/// the matching role; a role change starts a new block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatBlock {
    User(String),
    Assistant(String),
    Thinking(String),
    Tool {
        id: String,
        name: String,
        args: String,
        output: String,
        is_error: bool,
        done: bool,
    },
    Error(String),
    /// Operational note (backend fallback, disabled-chat banner) — not an
    /// error and not part of the conversation.
    Notice(String),
}

#[derive(Default)]
pub struct ChatState {
    pub blocks: Vec<ChatBlock>,
    pub input: String,
    pub in_flight: bool,
    pub turn_started_at: Option<Instant>,
    /// Turns abandoned TUI-side (timeout) whose backend `TurnFinished` has not
    /// arrived yet. While non-zero, `submit` stays gated and `apply_event`
    /// swallows that many `TurnFinished` events, so a late finish can never be
    /// misattributed to a newer turn.
    pub stale_finishes: usize,
    /// Lines scrolled back from the tail; 0 = follow the newest output.
    pub scroll_back: usize,
    /// Set when backend resolution found no registered chat backend at all:
    /// the input stays permanently rejected (the registry is frozen after
    /// boot, so this can never recover within one launch).
    pub disabled: bool,
}

impl ChatState {
    /// Take the input as a new user turn. Returns `None` (and leaves the
    /// input untouched) while a turn is in flight, an abandoned turn's
    /// `TurnFinished` is still outstanding, or the input is blank — the
    /// one-turn-at-a-time gate the `ChatSession` contract requires.
    pub fn submit(&mut self) -> Option<String> {
        if self.disabled || self.in_flight || self.stale_finishes > 0 {
            return None;
        }
        let prompt = self.input.trim().to_string();
        if prompt.is_empty() {
            return None;
        }
        self.input.clear();
        self.blocks.push(ChatBlock::User(prompt.clone()));
        self.in_flight = true;
        self.turn_started_at = Some(Instant::now());
        self.scroll_back = 0;
        Some(prompt)
    }

    /// End the in-flight turn on a TUI-side failure where the backend will
    /// never emit a `TurnFinished` (open/send error) with an error block.
    pub fn fail_turn(&mut self, message: String) {
        self.in_flight = false;
        self.turn_started_at = None;
        self.blocks.push(ChatBlock::Error(message));
    }

    /// End the in-flight turn on a TUI-side turn timeout. The backend was
    /// asked to abort, so its `TurnFinished` is still coming: count it as
    /// stale so `submit` stays gated until it arrives and `apply_event`
    /// swallows it instead of attributing it to the next turn.
    pub fn abandon_turn(&mut self, message: String) {
        self.fail_turn(message);
        self.stale_finishes += 1;
    }

    /// Backend resolution found no registered chat backend at all: reject
    /// input permanently (the registry is frozen after boot), release the
    /// pending turn's gate, and show the banner.
    pub fn disable(&mut self, banner: String) {
        self.disabled = true;
        self.in_flight = false;
        self.turn_started_at = None;
        self.blocks.push(ChatBlock::Notice(banner));
    }

    pub fn push_notice(&mut self, message: String) {
        self.blocks.push(ChatBlock::Notice(message));
    }

    pub fn push_error(&mut self, message: String) {
        self.blocks.push(ChatBlock::Error(message));
    }

    pub fn turn_timed_out(&self, timeout: Duration) -> bool {
        self.in_flight && self.turn_started_at.is_some_and(|t| t.elapsed() >= timeout)
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll_back += lines;
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll_back = self.scroll_back.saturating_sub(lines);
    }

    pub fn follow_tail(&mut self) {
        self.scroll_back = 0;
    }

    pub fn apply_event(&mut self, event: ChatEvent) {
        match event {
            ChatEvent::Delta { role, text } => self.append_delta(role, &text),
            ChatEvent::ToolCall { id, name, args } => self.blocks.push(ChatBlock::Tool {
                id,
                name,
                args,
                output: String::new(),
                is_error: false,
                done: false,
            }),
            ChatEvent::ToolOutput {
                id,
                text,
                is_error,
                done,
            } => self.set_tool_output(&id, text, is_error, done),
            ChatEvent::Error(message) => self.blocks.push(ChatBlock::Error(message)),
            ChatEvent::TurnFinished { ok, error } => {
                if self.stale_finishes > 0 {
                    // The finish of a turn the TUI already timed out and
                    // abandoned; consuming it re-opens the submit gate.
                    self.stale_finishes -= 1;
                    return;
                }
                if !self.in_flight {
                    return;
                }
                self.in_flight = false;
                self.turn_started_at = None;
                if !ok {
                    let error = error.unwrap_or_else(|| "unknown error".to_string());
                    self.blocks.push(ChatBlock::Error(if error == "aborted" {
                        "turn aborted".to_string()
                    } else {
                        format!("turn failed: {error}")
                    }));
                }
            }
            ChatEvent::SessionClosed { error } => {
                self.in_flight = false;
                self.turn_started_at = None;
                // The process is gone; no stale TurnFinished is coming.
                self.stale_finishes = 0;
                self.blocks.push(ChatBlock::Error(match error {
                    Some(error) => format!("chat session closed: {error}"),
                    None => "chat session closed".to_string(),
                }));
            }
        }
    }

    fn append_delta(&mut self, role: ChatRole, text: &str) {
        if let Some(block) = self.blocks.last_mut() {
            match (block, role) {
                (ChatBlock::Assistant(existing), ChatRole::Assistant)
                | (ChatBlock::Thinking(existing), ChatRole::Thinking) => {
                    existing.push_str(text);
                    return;
                }
                _ => {}
            }
        }
        self.blocks.push(match role {
            ChatRole::Assistant => ChatBlock::Assistant(text.to_string()),
            ChatRole::Thinking => ChatBlock::Thinking(text.to_string()),
        });
    }

    fn set_tool_output(&mut self, id: &str, text: String, is_error: bool, done: bool) {
        // ToolOutput text REPLACES prior output for the same id (cap-chat
        // contract: pi streams the accumulated partialResult, not deltas).
        let existing = self.blocks.iter_mut().rev().find(
            |block| matches!(block, ChatBlock::Tool { id: block_id, .. } if block_id == id),
        );
        if let Some(ChatBlock::Tool {
            output,
            is_error: block_is_error,
            done: block_done,
            ..
        }) = existing
        {
            *output = text;
            *block_is_error = is_error;
            *block_done = done;
        } else {
            // Output without a preceding ToolCall: keep it visible anyway.
            self.blocks.push(ChatBlock::Tool {
                id: id.to_string(),
                name: id.to_string(),
                args: String::new(),
                output: text,
                is_error,
                done,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use orchestrator_api::{ActiveRun, HistoryEntry, QueueItem, RunStatus};

    use super::*;

    fn busy_snapshot() -> RunSnapshot {
        let mut snapshot = RunSnapshot::empty();
        snapshot.version = 9;
        snapshot.agent.id = "demo".to_string();
        snapshot.agent.folder = "/tmp/demo".to_string();
        snapshot.agent.tracker = "files".to_string();
        snapshot.agent.runner = "claude".to_string();
        for i in 0..12 {
            snapshot.active_runs.push(ActiveRun {
                run_id: format!("run-{i}"),
                identifier: format!("ISSUE-{i}"),
                state: "doing".to_string(),
                workspace: String::new(),
                pid: 100 + i as u32,
                started_at: Utc::now(),
                last_event: String::new(),
                status: RunStatus::Running,
            });
            snapshot.queue.push(QueueItem {
                identifier: format!("ISSUE-{}", 20 + i),
                title: format!("queued thing {i}"),
                state: "todo".to_string(),
                priority: Some(i),
                created_at: None,
            });
            snapshot.history.push(HistoryEntry {
                identifier: format!("ISSUE-{}", 40 + i),
                status: RunStatus::Succeeded,
                pid: 0,
                ended_at: Utc::now(),
                note: "done".to_string(),
            });
        }
        snapshot
    }

    #[test]
    fn preamble_is_orientation_only_without_snapshot_or_issues() {
        let temp = tempfile::tempdir().unwrap();
        let preamble = build_preamble(None, temp.path());
        assert!(preamble.contains("[context]"));
        assert!(preamble.contains(&temp.path().display().to_string()));
        assert!(!preamble.contains("Orchestrator snapshot"));
        assert!(!preamble.contains("Issue files"));
    }

    #[test]
    fn version_zero_snapshot_is_left_out_of_the_preamble() {
        let temp = tempfile::tempdir().unwrap();
        // No tick has published yet: the empty snapshot says nothing useful.
        let preamble = build_preamble(Some(&RunSnapshot::empty()), temp.path());
        assert!(!preamble.contains("Orchestrator snapshot"));
    }

    #[test]
    fn snapshot_summary_respects_the_thirty_line_cap() {
        let temp = tempfile::tempdir().unwrap();
        let preamble = build_preamble(Some(&busy_snapshot()), temp.path());
        // Orientation + blank + 1 summary header + 3 sections of
        // (1 header + PREAMBLE_SECTION_CAP item lines) = 30 lines total.
        assert_eq!(preamble.lines().count(), 30);
        assert!(preamble.contains("Orchestrator snapshot (agent \"demo\""));
        for section in ["active runs (12):", "queue (12):", "recent history (12):"] {
            assert!(preamble.contains(section), "missing {section:?}");
        }
        // 12 items, cap 8: 7 real lines + an overflow marker per section.
        assert_eq!(preamble.matches("- ... and 5 more").count(), 3);
    }

    #[test]
    fn issues_listing_caps_at_twenty_entries_with_state_and_title() {
        let temp = tempfile::tempdir().unwrap();
        let issues = temp.path().join("issues");
        std::fs::create_dir(&issues).unwrap();
        for i in 1..=25 {
            std::fs::write(
                issues.join(format!("ISSUE-{i:02}.md")),
                format!("---\nstate: todo\ntitle: Fix thing {i}\n---\nbody\n"),
            )
            .unwrap();
        }
        let preamble = build_preamble(None, temp.path());
        assert!(preamble.contains("Issue files in ./issues (25):"));
        assert!(preamble.contains("- ISSUE-01.md [state: todo] Fix thing 1"));
        let entries = preamble
            .lines()
            .filter(|line| line.starts_with("- ISSUE"))
            .count();
        assert_eq!(entries, PREAMBLE_ISSUE_CAP);
        assert!(!preamble.contains("ISSUE-21.md"));
        assert!(preamble.contains("- ... and 5 more"));
    }

    fn delta(role: ChatRole, text: &str) -> ChatEvent {
        ChatEvent::Delta {
            role,
            text: text.to_string(),
        }
    }

    #[test]
    fn submit_takes_input_and_arms_the_gate() {
        let mut chat = ChatState {
            input: "  hello there  ".to_string(),
            ..Default::default()
        };
        assert_eq!(chat.submit().as_deref(), Some("hello there"));
        assert!(chat.in_flight);
        assert!(chat.turn_started_at.is_some());
        assert!(chat.input.is_empty());
        assert_eq!(chat.blocks, vec![ChatBlock::User("hello there".to_string())]);
    }

    #[test]
    fn submit_is_rejected_while_in_flight_and_on_blank_input() {
        let mut chat = ChatState {
            input: "first".to_string(),
            ..Default::default()
        };
        chat.submit().unwrap();
        chat.input = "second".to_string();
        assert!(chat.submit().is_none());
        assert_eq!(chat.input, "second"); // input preserved, not swallowed
        assert_eq!(chat.blocks.len(), 1);

        let mut chat = ChatState {
            input: "   ".to_string(),
            ..Default::default()
        };
        assert!(chat.submit().is_none());
    }

    #[test]
    fn deltas_append_to_same_role_block_and_split_on_role_change() {
        let mut chat = ChatState::default();
        chat.apply_event(delta(ChatRole::Thinking, "Let me "));
        chat.apply_event(delta(ChatRole::Thinking, "look..."));
        chat.apply_event(delta(ChatRole::Assistant, "pong"));
        chat.apply_event(delta(ChatRole::Assistant, "!"));
        chat.apply_event(delta(ChatRole::Thinking, "more"));
        assert_eq!(
            chat.blocks,
            vec![
                ChatBlock::Thinking("Let me look...".to_string()),
                ChatBlock::Assistant("pong!".to_string()),
                ChatBlock::Thinking("more".to_string()),
            ]
        );
    }

    #[test]
    fn tool_output_replaces_prior_output_for_the_same_id() {
        let mut chat = ChatState::default();
        chat.apply_event(ChatEvent::ToolCall {
            id: "call_1".to_string(),
            name: "bash".to_string(),
            args: "ls".to_string(),
        });
        chat.apply_event(ChatEvent::ToolOutput {
            id: "call_1".to_string(),
            text: "total 48".to_string(),
            is_error: false,
            done: false,
        });
        chat.apply_event(ChatEvent::ToolOutput {
            id: "call_1".to_string(),
            text: "total 48\nsrc".to_string(),
            is_error: false,
            done: true,
        });
        assert_eq!(
            chat.blocks,
            vec![
                ChatBlock::Tool {
                    id: "call_1".to_string(),
                    name: "bash".to_string(),
                    args: "ls".to_string(),
                    output: "total 48\nsrc".to_string(),
                    is_error: false,
                    done: true,
                },
            ]
        );
    }

    #[test]
    fn turn_finished_clears_gate_and_reports_failures() {
        let mut chat = ChatState {
            input: "go".to_string(),
            ..Default::default()
        };
        chat.submit().unwrap();
        chat.apply_event(ChatEvent::TurnFinished {
            ok: true,
            error: None,
        });
        assert!(!chat.in_flight);
        assert!(chat.turn_started_at.is_none());
        assert_eq!(chat.blocks.len(), 1); // only the user block

        chat.input = "again".to_string();
        chat.submit().unwrap();
        chat.apply_event(ChatEvent::TurnFinished {
            ok: false,
            error: Some("aborted".to_string()),
        });
        assert!(!chat.in_flight);
        assert_eq!(
            chat.blocks.last(),
            Some(&ChatBlock::Error("turn aborted".to_string()))
        );
    }

    #[test]
    fn late_turn_finished_after_timeout_is_swallowed_and_gates_submit_until_then() {
        let mut chat = ChatState {
            input: "slow".to_string(),
            ..Default::default()
        };
        chat.submit().unwrap();
        chat.abandon_turn("turn timed out".to_string());
        assert!(!chat.in_flight);

        // Turn A's TurnFinished is still outstanding: submitting turn B now
        // would let A's finish release B's gate, so it must stay blocked.
        chat.input = "next".to_string();
        assert!(chat.submit().is_none());
        assert_eq!(chat.input, "next");

        let blocks = chat.blocks.clone();
        chat.apply_event(ChatEvent::TurnFinished {
            ok: false,
            error: Some("aborted".to_string()),
        });
        assert_eq!(chat.blocks, blocks); // no duplicate error block

        // The stale finish consumed: turn B may go out, and its own finish is
        // handled normally (failure surfaces, gate released).
        chat.submit().unwrap();
        assert!(chat.in_flight);
        chat.apply_event(ChatEvent::TurnFinished {
            ok: false,
            error: Some("boom".to_string()),
        });
        assert!(!chat.in_flight);
        assert_eq!(
            chat.blocks.last(),
            Some(&ChatBlock::Error("turn failed: boom".to_string()))
        );
    }

    #[test]
    fn session_closed_clears_outstanding_stale_finishes() {
        let mut chat = ChatState {
            input: "slow".to_string(),
            ..Default::default()
        };
        chat.submit().unwrap();
        chat.abandon_turn("turn timed out".to_string());
        chat.apply_event(ChatEvent::SessionClosed { error: None });
        assert_eq!(chat.stale_finishes, 0);

        // The dead session can never deliver the stale finish; a new submit
        // (which reopens a fresh session) must not be blocked forever.
        chat.input = "retry".to_string();
        assert!(chat.submit().is_some());
    }

    #[test]
    fn turn_timer_only_fires_in_flight_and_past_the_deadline() {
        let mut chat = ChatState::default();
        assert!(!chat.turn_timed_out(Duration::ZERO));
        chat.input = "go".to_string();
        chat.submit().unwrap();
        assert!(!chat.turn_timed_out(Duration::from_secs(600)));
        chat.turn_started_at = Some(Instant::now() - Duration::from_secs(601));
        assert!(chat.turn_timed_out(Duration::from_secs(600)));
    }

    #[test]
    fn session_closed_clears_gate_and_surfaces_the_error() {
        let mut chat = ChatState {
            input: "go".to_string(),
            ..Default::default()
        };
        chat.submit().unwrap();
        chat.apply_event(ChatEvent::SessionClosed {
            error: Some("pi exited unexpectedly: signal".to_string()),
        });
        assert!(!chat.in_flight);
        assert_eq!(
            chat.blocks.last(),
            Some(&ChatBlock::Error(
                "chat session closed: pi exited unexpectedly: signal".to_string()
            ))
        );
    }
}
