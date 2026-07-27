//! Chat pane state: transcript blocks, the single-line input, the in-flight
//! turn counter, the TUI-side turn timer, and the one-shot context preamble
//! prepended to the first outbound prompt.

use std::path::Path;
use std::time::{Duration, Instant};

use cap_chat::{ChatEvent, ChatRole};
use orchestrator_api::RunSnapshot;

use crate::editor::TextArea;

/// Last reported context usage for the status line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextUsage {
    pub tokens_used: u64,
    pub context_window: Option<u64>,
}

/// Max lines shown per snapshot section (active/queue/history), keeping the
/// whole snapshot summary within the spec'd ~30-line cap (1 + 3·(1+8)).
const PREAMBLE_SECTION_CAP: usize = 8;
/// Max `issues/` entries listed in the preamble.
const PREAMBLE_ISSUE_CAP: usize = 20;

/// Build the one-shot context preamble prepended to the first outbound prompt.
/// Loop-enabled agents get coding/tracker orientation plus best-effort
/// snapshot and issue summaries. Passive agents get only identity-neutral
/// folder orientation so the agent's system prompt remains primary.
pub fn build_preamble(snapshot: Option<&RunSnapshot>, agent_root: &Path) -> String {
    // Snapshot presence alone is not the loop-enabled signal: passive agents
    // publish a retained snapshot (version > 0) with an empty tracker. Treat a
    // published snapshot with no tracker as passive. A pre-tick coding agent
    // (version 0, tracker not yet populated) stays loop-enabled.
    let loop_enabled = match snapshot {
        Some(s) => !(s.version > 0 && s.agent.tracker.is_empty()),
        None => false,
    };
    let mut out = if loop_enabled {
        format!(
            "[context] This is an operator chat session inside the dar agent folder at {}. \
             Issues live at ./issues (the tracker owns their state: field); per-issue \
             workspaces at ./workspaces. You run trusted with full access to this folder. \
             The operator's message follows this context.\n",
            agent_root.display()
        )
    } else {
        format!(
            "[context] This is an operator chat session inside the dar agent folder at {}. \
             You run trusted with full access to this folder. The operator's message follows \
             this context.\n",
            agent_root.display()
        )
    };
    if loop_enabled {
        if let Some(snapshot) = snapshot.filter(|s| s.version > 0) {
            out.push('\n');
            out.push_str(&snapshot_summary(snapshot));
        }
        if let Some(listing) = issues_listing(&agent_root.join("issues")) {
            out.push('\n');
            out.push_str(&listing);
        }
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
                item.priority
                    .map_or_else(|| "-".to_string(), |p| p.to_string()),
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

/// Parse numbered input against a pending question set: one whitespace-
/// separated token per question. A question with no options at all can't be
/// answered by number — for that case the *whole set* must be exactly one
/// question, and the raw trimmed input line is taken verbatim as its free-text
/// answer (an empty line yields `None`). Otherwise each token is one or more
/// 1-based option numbers: a `multiple: true` question accepts a comma-
/// separated list (e.g. `1,3`), collapsing duplicate numbers to one label each
/// in first-seen order; a `multiple: false` question must be exactly one
/// number and rejects a comma list. Any mismatch (wrong token count,
/// non-numeric token, a number out of range, or a comma list on a
/// non-multiple question) yields `None` so the input falls through as a
/// normal chat message instead of being swallowed as a malformed answer.
pub fn parse_answer(input: &str, questions: &[cap_chat::QuestionInfo]) -> Option<Vec<Vec<String>>> {
    if let [question] = questions {
        if question.options.is_empty() {
            let text = input.trim();
            return (!text.is_empty()).then(|| vec![vec![text.to_string()]]);
        }
    }
    let tokens: Vec<&str> = input.split_whitespace().collect();
    if tokens.len() != questions.len() {
        return None;
    }
    tokens
        .iter()
        .zip(questions)
        .map(|(token, question)| parse_question_token(token, question))
        .collect()
}

/// Parse one token (a single question's share of the input line) against its
/// question: see [`parse_answer`] for the comma/multiple rules. `None` on any
/// invalid or out-of-range number, or a comma list against a non-multiple
/// question.
fn parse_question_token(token: &str, question: &cap_chat::QuestionInfo) -> Option<Vec<String>> {
    if !question.multiple && token.contains(',') {
        return None;
    }
    let mut labels: Vec<String> = Vec::new();
    for part in token.split(',') {
        let n: usize = part.parse().ok()?;
        if n == 0 || n > question.options.len() {
            return None;
        }
        let label = question.options[n - 1].label.clone();
        if !labels.contains(&label) {
            labels.push(label);
        }
    }
    Some(labels)
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
    /// An interactive question the backend's agent asked the operator (e.g.
    /// opencode's `question` tool). Answered via numbered input; `done` flips
    /// only on the matching `ChatEvent::QuestionResolved` (or a dismissal on
    /// turn/session end), never locally on submit.
    Question {
        request_id: String,
        questions: Vec<cap_chat::QuestionInfo>,
        done: bool,
        rejected: bool,
        /// Rendered answer summary once resolved ("A; custom text" / "dismissed").
        answer: String,
    },
}

#[derive(Default)]
pub struct ChatState {
    pub blocks: Vec<ChatBlock>,
    pub input: TextArea,
    /// Most recent context-usage reading from the backend; `None` until the
    /// runner reports one (or never, when it doesn't surface usage).
    pub usage: Option<ContextUsage>,
    pub in_flight: bool,
    pub pending_turns: usize,
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
    /// Take the input as a user message. While a turn is already in flight,
    /// this still accepts the message and leaves backend queuing/injection to
    /// `ChatSession::send_turn`.
    pub fn submit(&mut self) -> Option<String> {
        if self.disabled || self.stale_finishes > 0 {
            return None;
        }
        let prompt = self.input.text().trim().to_string();
        if prompt.is_empty() {
            return None;
        }
        self.input.clear();
        self.blocks.push(ChatBlock::User(prompt.clone()));
        self.pending_turns += 1;
        if !self.in_flight {
            self.in_flight = true;
            self.turn_started_at = Some(Instant::now());
        }
        self.scroll_back = 0;
        Some(prompt)
    }

    /// End the in-flight turn on a TUI-side failure where the backend will
    /// never emit a `TurnFinished` (open/send error) with an error block.
    pub fn fail_turn(&mut self, message: String) {
        self.in_flight = false;
        self.pending_turns = 0;
        self.turn_started_at = None;
        self.blocks.push(ChatBlock::Error(message));
    }

    /// End the in-flight turn on a TUI-side turn timeout. The backend was
    /// asked to abort, so its `TurnFinished` is still coming: count it as
    /// stale so `submit` stays gated until it arrives and `apply_event`
    /// swallows it instead of attributing it to the next turn.
    pub fn abandon_turn(&mut self, message: String) {
        let stale_finishes = self.pending_turns.max(1);
        self.fail_turn(message);
        self.stale_finishes += stale_finishes;
    }

    /// Backend resolution found no registered chat backend at all: reject
    /// input permanently (the registry is frozen after boot), release the
    /// pending turn's gate, and show the banner.
    pub fn disable(&mut self, banner: String) {
        self.disabled = true;
        self.in_flight = false;
        self.pending_turns = 0;
        self.turn_started_at = None;
        self.blocks.push(ChatBlock::Notice(banner));
    }

    pub fn push_notice(&mut self, message: String) {
        self.blocks.push(ChatBlock::Notice(message));
    }

    /// Drop the visible transcript so the pane opens empty, mirroring a fresh
    /// launch. Used by `/new`: the prior (possibly hydrated) conversation
    /// belongs to the closed session, so clearing the view matches the
    /// brand-new file `pi` forks next. Display-only; touches no turn counters.
    pub fn clear_transcript(&mut self) {
        self.blocks.clear();
        self.scroll_back = 0;
    }

    /// Replay prior session messages into the transcript on launch so a resumed
    /// conversation is visible before the human's first new turn. Display-only:
    /// these blocks are never sent to the backend and don't touch the turn
    /// counters (`pi` already holds them in the resumed session). When the
    /// session was display-truncated, a leading "earlier messages" notice marks
    /// that older turns are reachable only via the recall tools. A no-op when
    /// there are no messages, so a fresh / post-`/new` launch stays empty.
    pub fn hydrate(&mut self, messages: &[crate::archive::Message], truncated: bool) {
        if messages.is_empty() {
            return;
        }
        if truncated {
            self.blocks.push(ChatBlock::Notice(
                "— earlier messages hidden; use the recall tools to reach the full history —"
                    .to_string(),
            ));
        }
        for msg in messages {
            let block = match msg.role.as_str() {
                "assistant" => ChatBlock::Assistant(msg.text.clone()),
                _ => ChatBlock::User(msg.text.clone()),
            };
            self.blocks.push(block);
        }
    }

    pub fn push_error(&mut self, message: String) {
        self.blocks.push(ChatBlock::Error(message));
    }

    pub fn turn_timed_out(&self, timeout: Duration) -> bool {
        self.in_flight && self.turn_started_at.is_some_and(|t| t.elapsed() >= timeout)
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll_back = self.scroll_back.saturating_add(lines);
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll_back = self.scroll_back.saturating_sub(lines);
    }

    pub fn follow_tail(&mut self) {
        self.scroll_back = 0;
    }

    pub fn apply_event(&mut self, event: ChatEvent) {
        match event {
            ChatEvent::User { text } => self.blocks.push(ChatBlock::User(text)),
            ChatEvent::SessionReset => {
                self.clear_transcript();
                self.in_flight = false;
                self.pending_turns = 0;
                self.turn_started_at = None;
                self.stale_finishes = 0;
                self.push_notice("— started a fresh session —".to_string());
            }
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
            ChatEvent::QuestionAsked {
                request_id,
                questions,
            } => self.blocks.push(ChatBlock::Question {
                request_id,
                questions,
                done: false,
                rejected: false,
                answer: String::new(),
            }),
            ChatEvent::QuestionResolved {
                request_id,
                answers,
                rejected,
            } => {
                let block = self.blocks.iter_mut().rev().find(|block| {
                    matches!(block, ChatBlock::Question { request_id: id, done: false, .. } if *id == request_id)
                });
                if let Some(ChatBlock::Question {
                    done,
                    rejected: block_rejected,
                    answer,
                    ..
                }) = block
                {
                    *done = true;
                    *block_rejected = rejected;
                    *answer = if rejected {
                        "dismissed".to_string()
                    } else {
                        answers
                            .iter()
                            .map(|a| a.join(", "))
                            .collect::<Vec<_>>()
                            .join("; ")
                    };
                }
            }
            ChatEvent::Error(message) => self.blocks.push(ChatBlock::Error(message)),
            ChatEvent::ContextUsage {
                tokens_used,
                context_window,
            } => {
                self.usage = Some(ContextUsage {
                    tokens_used,
                    context_window,
                });
            }
            ChatEvent::TurnFinished { ok, error } => {
                // A pending question cannot outlive its turn; this also
                // covers a rejected-question event lost to an abort.
                self.dismiss_pending_questions();
                if self.stale_finishes > 0 {
                    // The finish of a turn the TUI already timed out and
                    // abandoned; consuming it re-opens the submit gate.
                    self.stale_finishes -= 1;
                    return;
                }
                if !self.in_flight {
                    return;
                }
                if !ok {
                    let remaining = self.pending_turns.saturating_sub(1);
                    self.pending_turns = 0;
                    self.in_flight = false;
                    self.turn_started_at = None;
                    self.stale_finishes += remaining;
                    let error = error.unwrap_or_else(|| "unknown error".to_string());
                    self.blocks.push(ChatBlock::Error(if error == "aborted" {
                        "turn aborted".to_string()
                    } else {
                        format!("turn failed: {error}")
                    }));
                    return;
                }
                self.pending_turns = self.pending_turns.saturating_sub(1);
                if self.pending_turns == 0 {
                    self.in_flight = false;
                    self.turn_started_at = None;
                } else {
                    self.turn_started_at = Some(Instant::now());
                }
            }
            ChatEvent::SessionClosed { error } => {
                self.dismiss_pending_questions();
                self.in_flight = false;
                self.pending_turns = 0;
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

    /// Dismiss every still-pending question block: a pending question cannot
    /// outlive its turn, and this also covers a rejected-question event lost
    /// to an abort.
    fn dismiss_pending_questions(&mut self) {
        for block in self.blocks.iter_mut() {
            if let ChatBlock::Question {
                done,
                rejected,
                answer,
                ..
            } = block
            {
                if !*done {
                    *done = true;
                    *rejected = true;
                    *answer = "dismissed".to_string();
                }
            }
        }
    }

    /// The newest unanswered question, if any: (request_id, questions).
    pub fn pending_question(&self) -> Option<(&str, &[cap_chat::QuestionInfo])> {
        self.blocks.iter().rev().find_map(|b| match b {
            ChatBlock::Question {
                request_id,
                questions,
                done: false,
                ..
            } => Some((request_id.as_str(), questions.as_slice())),
            _ => None,
        })
    }

    fn set_tool_output(&mut self, id: &str, text: String, is_error: bool, done: bool) {
        // ToolOutput text REPLACES prior output for the same id (cap-chat
        // contract: pi streams the accumulated partialResult, not deltas).
        let existing =
            self.blocks.iter_mut().rev().find(
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

    /// Build an input buffer from a string for the state tests.
    fn ta(text: &str) -> TextArea {
        let mut area = TextArea::default();
        area.insert_str(text);
        area
    }

    fn busy_snapshot() -> RunSnapshot {
        let mut snapshot = RunSnapshot::empty();
        snapshot.version = 9;
        snapshot.agent.id = "demo".to_string();
        snapshot.agent.folder = "/tmp/demo".to_string();
        snapshot.agent.tracker = "files".to_string();
        snapshot.agent.runner = "fake".to_string();
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
    fn passive_preamble_is_identity_neutral_without_snapshot_or_issues() {
        let temp = tempfile::tempdir().unwrap();
        let preamble = build_preamble(None, temp.path());
        assert!(preamble.contains("[context]"));
        assert!(preamble.contains(&temp.path().display().to_string()));
        assert!(!preamble.contains("Issues live at ./issues"));
        assert!(!preamble.contains("per-issue workspaces"));
        assert!(!preamble.contains("software"));
        assert!(!preamble.contains("Orchestrator snapshot"));
        assert!(!preamble.contains("Issue files"));
    }

    /// Mirror `orchestrator::passive_snapshot`: a retained snapshot with
    /// `version > 0` and an empty tracker. This is the real passive runtime
    /// shape PR #78 did not cover.
    fn passive_snapshot() -> RunSnapshot {
        let mut snapshot = RunSnapshot::empty();
        snapshot.version = 1;
        snapshot.agent.id = "kalel".to_string();
        snapshot.agent.folder = "/tmp/kalel".to_string();
        snapshot.agent.tracker = String::new();
        snapshot.agent.runner = "pi".to_string();
        snapshot
    }

    #[test]
    fn passive_preamble_is_identity_neutral_with_real_passive_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        // Issues dir present: a passive agent must still not get coding framing.
        let issues = temp.path().join("issues");
        std::fs::create_dir(&issues).unwrap();
        std::fs::write(
            issues.join("ISSUE-01.md"),
            "---\nstate: todo\ntitle: Fix thing\n---\nbody\n",
        )
        .unwrap();
        let preamble = build_preamble(Some(&passive_snapshot()), temp.path());
        assert!(preamble.contains("[context]"));
        assert!(preamble.contains(&temp.path().display().to_string()));
        assert!(!preamble.contains("Issues live at ./issues"));
        assert!(!preamble.contains("per-issue workspaces"));
        assert!(!preamble.contains("Orchestrator snapshot"));
        assert!(!preamble.contains("Issue files"));
    }

    #[test]
    fn coding_preamble_keeps_issue_framing_with_version_zero_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        // No tick has published yet: the empty snapshot says nothing useful.
        let preamble = build_preamble(Some(&RunSnapshot::empty()), temp.path());
        assert!(preamble.contains("Issues live at ./issues"));
        assert!(preamble.contains("per-issue workspaces"));
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
        let preamble = build_preamble(Some(&RunSnapshot::empty()), temp.path());
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

    #[test]
    fn context_usage_event_updates_state_and_is_last_writer_wins() {
        let mut chat = ChatState::default();
        assert!(chat.usage.is_none());
        chat.apply_event(ChatEvent::ContextUsage {
            tokens_used: 10_928,
            context_window: Some(200_000),
        });
        assert_eq!(
            chat.usage,
            Some(ContextUsage {
                tokens_used: 10_928,
                context_window: Some(200_000),
            })
        );
        chat.apply_event(ChatEvent::ContextUsage {
            tokens_used: 12_000,
            context_window: None,
        });
        assert_eq!(
            chat.usage,
            Some(ContextUsage {
                tokens_used: 12_000,
                context_window: None,
            })
        );
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
            input: ta("  hello there  "),
            ..Default::default()
        };
        assert_eq!(chat.submit().as_deref(), Some("hello there"));
        assert!(chat.in_flight);
        assert!(chat.turn_started_at.is_some());
        assert!(chat.input.is_empty());
        assert_eq!(
            chat.blocks,
            vec![ChatBlock::User("hello there".to_string())]
        );
    }

    #[test]
    fn submit_accepts_steering_while_in_flight_and_acknowledges_it() {
        let mut chat = ChatState {
            input: ta("first"),
            ..Default::default()
        };
        chat.submit().unwrap();
        chat.input = ta("second");
        assert_eq!(chat.submit().as_deref(), Some("second"));
        assert!(chat.input.is_empty());
        assert_eq!(
            chat.blocks,
            vec![
                ChatBlock::User("first".to_string()),
                ChatBlock::User("second".to_string()),
            ]
        );
        assert!(chat.in_flight);

        chat.apply_event(ChatEvent::TurnFinished {
            ok: true,
            error: None,
        });
        assert!(chat.in_flight);
        chat.apply_event(ChatEvent::TurnFinished {
            ok: true,
            error: None,
        });
        assert!(!chat.in_flight);

        let mut chat = ChatState {
            input: ta("   "),
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
            vec![ChatBlock::Tool {
                id: "call_1".to_string(),
                name: "bash".to_string(),
                args: "ls".to_string(),
                output: "total 48\nsrc".to_string(),
                is_error: false,
                done: true,
            },]
        );
    }

    #[test]
    fn turn_finished_clears_gate_and_reports_failures() {
        let mut chat = ChatState {
            input: ta("go"),
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

        chat.input = ta("again");
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
            input: ta("slow"),
            ..Default::default()
        };
        chat.submit().unwrap();
        chat.abandon_turn("turn timed out".to_string());
        assert!(!chat.in_flight);

        // Turn A's TurnFinished is still outstanding: submitting turn B now
        // would let A's finish release B's gate, so it must stay blocked.
        chat.input = ta("next");
        assert!(chat.submit().is_none());
        assert_eq!(chat.input.text(), "next");

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
    fn failed_finishes_for_queued_turns_do_not_reopen_submit_until_all_are_seen() {
        let mut chat = ChatState {
            input: ta("first"),
            ..Default::default()
        };
        chat.submit().unwrap();
        chat.input = ta("second");
        chat.submit().unwrap();

        chat.apply_event(ChatEvent::TurnFinished {
            ok: false,
            error: Some("aborted".to_string()),
        });
        assert!(!chat.in_flight);
        chat.input = ta("too early");
        assert!(chat.submit().is_none());
        assert_eq!(chat.input.text(), "too early");

        chat.apply_event(ChatEvent::TurnFinished {
            ok: false,
            error: Some("aborted".to_string()),
        });
        assert!(!chat.in_flight);
        assert_eq!(chat.submit().as_deref(), Some("too early"));
    }

    #[test]
    fn timeout_with_queued_turns_gates_submit_until_all_stale_finishes_arrive() {
        let mut chat = ChatState {
            input: ta("first"),
            ..Default::default()
        };
        chat.submit().unwrap();
        chat.input = ta("second");
        chat.submit().unwrap();

        chat.abandon_turn("turn timed out".to_string());
        chat.input = ta("next");
        chat.apply_event(ChatEvent::TurnFinished {
            ok: false,
            error: Some("aborted".to_string()),
        });
        assert!(chat.submit().is_none());

        chat.apply_event(ChatEvent::TurnFinished {
            ok: false,
            error: Some("aborted".to_string()),
        });
        assert_eq!(chat.submit().as_deref(), Some("next"));
    }

    #[test]
    fn session_closed_clears_outstanding_stale_finishes() {
        let mut chat = ChatState {
            input: ta("slow"),
            ..Default::default()
        };
        chat.submit().unwrap();
        chat.abandon_turn("turn timed out".to_string());
        chat.apply_event(ChatEvent::SessionClosed { error: None });
        assert_eq!(chat.stale_finishes, 0);

        // The dead session can never deliver the stale finish; a new submit
        // (which reopens a fresh session) must not be blocked forever.
        chat.input = ta("retry");
        assert!(chat.submit().is_some());
    }

    #[test]
    fn turn_timer_only_fires_in_flight_and_past_the_deadline() {
        let mut chat = ChatState::default();
        assert!(!chat.turn_timed_out(Duration::ZERO));
        chat.input = ta("go");
        chat.submit().unwrap();
        assert!(!chat.turn_timed_out(Duration::from_secs(600)));
        chat.turn_started_at = Some(Instant::now() - Duration::from_secs(601));
        assert!(chat.turn_timed_out(Duration::from_secs(600)));
    }

    #[test]
    fn session_closed_clears_gate_and_surfaces_the_error() {
        let mut chat = ChatState {
            input: ta("go"),
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

    fn msg(role: &str, text: &str) -> crate::archive::Message {
        crate::archive::Message {
            index: 0,
            role: role.to_string(),
            text: text.to_string(),
        }
    }

    #[test]
    fn hydrate_replays_prior_messages_without_counting_a_turn() {
        let mut chat = ChatState::default();
        let prior = vec![
            msg("user", "hello"),
            msg("assistant", "hi there"),
            msg("user", "continue"),
        ];
        chat.hydrate(&prior, false);
        assert_eq!(
            chat.blocks,
            vec![
                ChatBlock::User("hello".to_string()),
                ChatBlock::Assistant("hi there".to_string()),
                ChatBlock::User("continue".to_string()),
            ]
        );
        // Display-only: no turn counted, no in-flight state, input untouched.
        assert!(!chat.in_flight);
        assert_eq!(chat.pending_turns, 0);
        assert!(chat.turn_started_at.is_none());
    }

    #[test]
    fn hydrate_empty_leaves_transcript_empty() {
        let mut chat = ChatState::default();
        chat.hydrate(&[], false);
        assert!(chat.blocks.is_empty());
    }

    #[test]
    fn clear_transcript_empties_hydrated_blocks() {
        // Simulates `/new` after a resumed launch hydrated prior turns: the
        // transcript must open empty, like a cold launch.
        let mut chat = ChatState::default();
        chat.hydrate(&[msg("user", "hello"), msg("assistant", "hi")], true);
        assert!(!chat.blocks.is_empty());
        chat.scroll_back = 5;

        chat.clear_transcript();
        assert!(chat.blocks.is_empty());
        assert_eq!(chat.scroll_back, 0);

        // The post-`/new` notice lands on an otherwise empty pane.
        chat.push_notice("— started a fresh session —".to_string());
        assert_eq!(
            chat.blocks,
            vec![ChatBlock::Notice("— started a fresh session —".to_string())]
        );
    }

    fn question(labels: &[&str]) -> cap_chat::QuestionInfo {
        cap_chat::QuestionInfo {
            header: "Pick one".to_string(),
            question: "Which one?".to_string(),
            options: labels
                .iter()
                .map(|label| cap_chat::QuestionOption {
                    label: label.to_string(),
                    description: String::new(),
                })
                .collect(),
            multiple: false,
            custom: false,
        }
    }

    #[test]
    fn question_asked_pushes_a_pending_block() {
        let mut chat = ChatState::default();
        chat.apply_event(ChatEvent::QuestionAsked {
            request_id: "req-1".to_string(),
            questions: vec![question(&["A", "B"])],
        });
        assert_eq!(
            chat.blocks,
            vec![ChatBlock::Question {
                request_id: "req-1".to_string(),
                questions: vec![question(&["A", "B"])],
                done: false,
                rejected: false,
                answer: String::new(),
            }]
        );
    }

    #[test]
    fn question_resolved_marks_the_matching_block_answered() {
        let mut chat = ChatState::default();
        chat.apply_event(ChatEvent::QuestionAsked {
            request_id: "req-1".to_string(),
            questions: vec![question(&["A", "B"])],
        });
        chat.apply_event(ChatEvent::QuestionResolved {
            request_id: "req-1".to_string(),
            answers: vec![
                vec!["A".to_string(), "B".to_string()],
                vec!["custom".to_string()],
            ],
            rejected: false,
        });
        match &chat.blocks[0] {
            ChatBlock::Question {
                done,
                rejected,
                answer,
                ..
            } => {
                assert!(*done);
                assert!(!*rejected);
                assert_eq!(answer, "A, B; custom");
            }
            other => panic!("expected a Question block, got {other:?}"),
        }
    }

    #[test]
    fn turn_finished_dismisses_only_pending_questions() {
        let mut chat = ChatState::default();
        chat.apply_event(ChatEvent::QuestionAsked {
            request_id: "answered".to_string(),
            questions: vec![question(&["A"])],
        });
        chat.apply_event(ChatEvent::QuestionResolved {
            request_id: "answered".to_string(),
            answers: vec![vec!["A".to_string()]],
            rejected: false,
        });
        chat.apply_event(ChatEvent::QuestionAsked {
            request_id: "pending".to_string(),
            questions: vec![question(&["A"])],
        });
        chat.apply_event(ChatEvent::TurnFinished {
            ok: false,
            error: Some("aborted".to_string()),
        });
        match &chat.blocks[0] {
            ChatBlock::Question {
                done,
                rejected,
                answer,
                ..
            } => {
                assert!(*done);
                assert!(!*rejected);
                assert_eq!(answer, "A");
            }
            other => panic!("expected the answered block untouched, got {other:?}"),
        }
        match &chat.blocks[1] {
            ChatBlock::Question {
                done,
                rejected,
                answer,
                ..
            } => {
                assert!(*done);
                assert!(*rejected);
                assert_eq!(answer, "dismissed");
            }
            other => panic!("expected the pending block dismissed, got {other:?}"),
        }
    }

    #[test]
    fn session_closed_dismisses_pending_questions() {
        let mut chat = ChatState::default();
        chat.apply_event(ChatEvent::QuestionAsked {
            request_id: "pending".to_string(),
            questions: vec![question(&["A"])],
        });
        chat.apply_event(ChatEvent::SessionClosed { error: None });
        match &chat.blocks[0] {
            ChatBlock::Question {
                done,
                rejected,
                answer,
                ..
            } => {
                assert!(*done);
                assert!(*rejected);
                assert_eq!(answer, "dismissed");
            }
            other => panic!("expected the pending block dismissed, got {other:?}"),
        }
    }

    #[test]
    fn parse_answer_maps_numbers_to_option_labels() {
        let single = vec![question(&["opt1-label", "opt2-label", "opt3-label"])];
        assert_eq!(
            parse_answer("2", &single),
            Some(vec![vec!["opt2-label".to_string()]])
        );

        let two = vec![question(&["a1", "a2"]), question(&["b1", "b2", "b3"])];
        assert_eq!(
            parse_answer("1 3", &two),
            Some(vec![vec!["a1".to_string()], vec!["b3".to_string()]])
        );

        // Out-of-range, non-numeric, and count-mismatch inputs all fall
        // through as None (the input becomes a normal chat message).
        assert_eq!(parse_answer("0", &single), None);
        assert_eq!(parse_answer("4", &single), None);
        assert_eq!(parse_answer("x", &single), None);
        assert_eq!(parse_answer("1 2", &single), None);
    }

    fn multi_question(labels: &[&str]) -> cap_chat::QuestionInfo {
        let mut q = question(labels);
        q.multiple = true;
        q
    }

    #[test]
    fn parse_answer_multi_select_accepts_comma_separated_numbers() {
        let one = vec![multi_question(&["a1", "a2", "a3"])];
        assert_eq!(
            parse_answer("1,3", &one),
            Some(vec![vec!["a1".to_string(), "a3".to_string()]])
        );

        // Duplicates collapse to a single label, first-seen order kept.
        assert_eq!(
            parse_answer("2,2,1", &one),
            Some(vec![vec!["a2".to_string(), "a1".to_string()]])
        );

        // Mixed set: a single-select token alongside a multi-select token,
        // space-separated (one token per question).
        let mixed = vec![question(&["x1", "x2"]), multi_question(&["y1", "y2", "y3"])];
        assert_eq!(
            parse_answer("2 1,3", &mixed),
            Some(vec![
                vec!["x2".to_string()],
                vec!["y1".to_string(), "y3".to_string()]
            ])
        );
    }

    #[test]
    fn parse_answer_rejects_comma_list_for_non_multiple_question() {
        let single = vec![question(&["a1", "a2", "a3"])];
        assert_eq!(parse_answer("1,2", &single), None);

        let multi = vec![multi_question(&["a1", "a2", "a3"])];
        // Out-of-range/non-numeric entries in a comma list still fail.
        assert_eq!(parse_answer("1,9", &multi), None);
        assert_eq!(parse_answer("1,x", &multi), None);
    }

    #[test]
    fn parse_answer_free_text_for_an_option_less_question() {
        // `question(&[])` already carries `custom: false` — this is exactly
        // the degenerate shape Finding C covers: no options to pick a number
        // for, answerable only as free text.
        let questions = vec![question(&[])];
        assert_eq!(
            parse_answer("  it's the third one, actually  ", &questions),
            Some(vec![vec!["it's the third one, actually".to_string()]])
        );
        // An empty line still falls through as no answer.
        assert_eq!(parse_answer("   ", &questions), None);
    }

    #[test]
    fn hydrate_truncated_prepends_earlier_messages_marker() {
        let mut chat = ChatState::default();
        let prior = vec![
            msg("user", "recent question"),
            msg("assistant", "recent answer"),
        ];
        chat.hydrate(&prior, true);
        assert!(
            matches!(chat.blocks.first(), Some(ChatBlock::Notice(n)) if n.contains("earlier messages"))
        );
        assert_eq!(chat.blocks.len(), 3);
        assert_eq!(
            chat.blocks[1],
            ChatBlock::User("recent question".to_string())
        );
        assert_eq!(
            chat.blocks[2],
            ChatBlock::Assistant("recent answer".to_string())
        );
    }
}
