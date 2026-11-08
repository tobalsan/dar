//! Agent silence (`NO_REPLY`) and the agent-to-agent loop guard.
//!
//! Both are enforced once, at the backend boundary: every stock
//! [`ChatBackend`](crate::ChatBackend) opens its session through
//! [`open_guarded`], so every medium (web chat, TUI, channel extensions)
//! receives already-filtered events and never re-implements the rules.

use std::future::Future;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{self, Sender, WeakSender};

use crate::shape::{HumanChat, Shaper};
use crate::{BoxFuture, ChatEvent, ChatRole, ChatSession};

/// The token an agent replies with, alone, to deliver nothing.
pub const NO_REPLY_TOKEN: &str = "NO_REPLY";

/// System-prompt line teaching the model the silence token.
pub const NO_REPLY_INSTRUCTION: &str = "If no response is needed (e.g. the user asked you not to reply, or a message from another agent needs no answer), reply with exactly `NO_REPLY` and nothing else.";

fn is_decoration(c: char) -> bool {
    c == '*' || c == '`' || c.is_whitespace()
}

/// True when `text` is the silence token alone: surrounding whitespace, `*`,
/// `**`, backticks and a trailing `.` are tolerated. Token embedded in other
/// text is a normal reply.
pub fn is_no_reply(text: &str) -> bool {
    let t = text.trim_matches(is_decoration);
    let t = t.strip_suffix('.').unwrap_or(t).trim_matches(is_decoration);
    t == NO_REPLY_TOKEN
}

/// True while `text` (a streamed prefix) could still become a [`is_no_reply`]
/// match.
fn could_be_no_reply(text: &str) -> bool {
    let t = text.trim_start_matches(is_decoration);
    if t.len() <= NO_REPLY_TOKEN.len() {
        return NO_REPLY_TOKEN.starts_with(t);
    }
    t.strip_prefix(NO_REPLY_TOKEN)
        .is_some_and(|rest| rest.chars().all(|c| c == '.' || is_decoration(c)))
}

/// Line-start glyph of an activity marker line.
const ACTIVITY_GLYPH: char = '»';

/// Longest accepted activity label, in characters. A longer marker line is
/// not a label and stays normal text.
const ACTIVITY_LABEL_MAX: usize = 120;

/// System-prompt rule that teaches the model the activity marker. Only
/// surfaces that render activity labels (web chat) add it to their prompt.
pub const ACTIVITY_INSTRUCTION: &str = "Before each tool call, or before each batch of parallel tool calls, write one line that starts with `» ` and gives 3 to 8 plain words for the user. Put it on its own line as plain text: not in bold, a list, or a code block. Example: `» Read the chat styles`. Example: `» Check the open pull requests`. Do not write a `»` line before your final answer.";

/// The label of a complete marker line (`» label`), or `None` when `line` is
/// not a well-formed marker. The line must have no newline.
fn marker_label(line: &str) -> Option<String> {
    let label = line.strip_prefix(ACTIVITY_GLYPH)?.strip_prefix(' ')?.trim();
    (!label.is_empty() && label.chars().count() <= ACTIVITY_LABEL_MAX).then(|| label.to_owned())
}

/// Where the filter is inside the current assistant line.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Line {
    /// No character of the line seen yet.
    #[default]
    Start,
    /// The line starts with `»` and may still be a marker line; held back.
    Held,
    /// The line is normal text; released as it arrives.
    Plain,
}

/// One piece of assistant text after marker extraction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActivityPart {
    Text(String),
    Label(String),
}

/// Pulls `» label` lines out of assistant text and reports each as
/// [`ChatEvent::Activity`]. Only text that could still be a marker line is held
/// back (a line start `»`). All other text passes at once. Thinking text and
/// text inside a code fence are never touched. A malformed marker stays text.
#[derive(Default)]
pub struct ActivityFilter {
    line: Line,
    /// Held text of the current line while `line` is [`Line::Held`].
    held: String,
    /// Start of the current plain line, to spot code fences.
    probe: String,
    fence: bool,
}

impl ActivityFilter {
    pub fn push(&mut self, event: ChatEvent) -> Vec<ChatEvent> {
        match event {
            ChatEvent::Delta {
                role: ChatRole::Assistant,
                text,
            } => self.push_text(&text),
            // Thinking between two assistant chunks: the next chunk starts a new line.
            ChatEvent::Delta {
                role: ChatRole::Thinking,
                ..
            } => {
                if self.line == Line::Plain {
                    self.end_plain_line();
                }
                vec![event]
            }
            ChatEvent::ToolCall { .. } => self.boundary(true, event),
            ChatEvent::ToolOutput { .. }
            | ChatEvent::Error(_)
            | ChatEvent::TurnStarted { .. }
            | ChatEvent::TurnFinished { .. }
            | ChatEvent::SessionClosed { .. } => self.boundary(false, event),
            other => vec![other],
        }
    }

    fn push_text(&mut self, text: &str) -> Vec<ChatEvent> {
        let mut out = Vec::new();
        let mut plain = String::new();
        for ch in text.chars() {
            if self.line == Line::Start && ch == ACTIVITY_GLYPH && !self.fence {
                self.line = Line::Held;
            }
            match self.line {
                Line::Held => {
                    self.held.push(ch);
                    if ch == '\n' {
                        let line = std::mem::take(&mut self.held);
                        self.line = Line::Start;
                        match marker_label(line.trim_end_matches(['\n', '\r'])) {
                            Some(label) => {
                                if !plain.is_empty() {
                                    out.push(assistant(std::mem::take(&mut plain)));
                                }
                                out.push(ChatEvent::Activity { label });
                            }
                            None => plain.push_str(&line),
                        }
                    } else if !self.could_be_marker() {
                        plain.push_str(&std::mem::take(&mut self.held));
                        self.line = Line::Plain;
                    }
                }
                Line::Start | Line::Plain => {
                    self.line = Line::Plain;
                    plain.push(ch);
                    if ch == '\n' {
                        self.end_plain_line();
                    } else if self.probe.len() < 8 {
                        self.probe.push(ch);
                    }
                }
            }
        }
        if !plain.is_empty() {
            out.push(assistant(plain));
        }
        out
    }

    /// True while the held line can still become a well-formed marker.
    fn could_be_marker(&self) -> bool {
        let mut chars = self.held.chars();
        chars.next();
        match chars.next() {
            None | Some(' ') => chars.count() <= ACTIVITY_LABEL_MAX,
            Some(_) => false,
        }
    }

    fn end_plain_line(&mut self) {
        if self.probe.trim_start().starts_with("```") {
            self.fence = !self.fence;
        }
        self.probe.clear();
        self.line = Line::Start;
    }

    /// A tool call or a turn edge ends the segment. A held marker line without
    /// a newline becomes a label before a tool call and plain text otherwise.
    fn boundary(&mut self, tool_follows: bool, event: ChatEvent) -> Vec<ChatEvent> {
        let mut out = Vec::new();
        if self.line == Line::Held {
            let held = std::mem::take(&mut self.held);
            match marker_label(held.trim_end_matches('\r')) {
                Some(label) if tool_follows => out.push(ChatEvent::Activity { label }),
                Some(label) => out.push(assistant(label)),
                None => out.push(assistant(held)),
            }
        }
        *self = Self::default();
        out.push(event);
        out
    }
}

/// Split `text` into plain text and marker labels, as the live
/// [`ActivityFilter`] does. `tool_follows` says whether a tool call comes
/// after the text in the same message. Without one, a marker line stays text
/// and loses its glyph.
pub fn split_activity(text: &str, tool_follows: bool) -> Vec<ActivityPart> {
    let mut filter = ActivityFilter::default();
    let end = if tool_follows {
        ChatEvent::ToolCall {
            id: String::new(),
            name: String::new(),
            args: String::new(),
        }
    } else {
        ChatEvent::TurnFinished {
            ok: true,
            error: None,
        }
    };
    let events = filter.push(assistant(text.to_owned()));
    let mut parts = Vec::new();
    for event in events.into_iter().chain(filter.push(end)) {
        match event {
            ChatEvent::Delta { text, .. } => parts.push(ActivityPart::Text(text)),
            ChatEvent::Activity { label } if tool_follows => parts.push(ActivityPart::Label(label)),
            ChatEvent::Activity { label } => parts.push(ActivityPart::Text(format!("{label}\n"))),
            _ => {}
        }
    }
    parts
}

/// Why a turn delivered nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SilentReason {
    /// Loop guard: too many consecutive agent-authored turns.
    MaxAgentTurns,
    /// Loop guard: the incoming message exceeded the hop TTL.
    MaxHops,
}

impl SilentReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MaxAgentTurns => "max_agent_turns",
            Self::MaxHops => "max_hops",
        }
    }
}

/// Prefix hold-back over a backend's event stream. Assistant text is buffered
/// while it could still be `NO_REPLY`; tool boundaries, errors and a
/// non-matching prefix flush it. At `TurnFinished` a buffered token becomes
/// one [`ChatEvent::Silent`] instead of text.
#[derive(Default)]
pub struct NoReplyFilter {
    buf: String,
    /// Current assistant segment already proved it is not the token.
    passthrough: bool,
}

impl NoReplyFilter {
    pub fn push(&mut self, event: ChatEvent) -> Vec<ChatEvent> {
        match event {
            ChatEvent::Delta {
                role: ChatRole::Assistant,
                text,
            } => {
                if self.passthrough {
                    return vec![assistant(text)];
                }
                self.buf.push_str(&text);
                if could_be_no_reply(&self.buf) {
                    return Vec::new();
                }
                self.passthrough = true;
                vec![assistant(std::mem::take(&mut self.buf))]
            }
            ChatEvent::TurnFinished { .. } => {
                let mut out = Vec::new();
                if is_no_reply(&self.buf) {
                    out.push(ChatEvent::Silent {
                        reason: None,
                        text: std::mem::take(&mut self.buf),
                    });
                } else {
                    out.extend(self.flush());
                }
                self.passthrough = false;
                out.push(event);
                out
            }
            ChatEvent::ToolCall { .. }
            | ChatEvent::ToolOutput { .. }
            | ChatEvent::Error(_)
            | ChatEvent::TurnStarted { .. }
            | ChatEvent::SessionClosed { .. } => {
                let mut out: Vec<_> = self.flush().into_iter().collect();
                self.passthrough = false;
                out.push(event);
                out
            }
            other => vec![other],
        }
    }

    fn flush(&mut self) -> Option<ChatEvent> {
        (!self.buf.is_empty()).then(|| assistant(std::mem::take(&mut self.buf)))
    }
}

fn assistant(text: String) -> ChatEvent {
    ChatEvent::Delta {
        role: ChatRole::Assistant,
        text,
    }
}

/// Author of a turn that did not come from a human. `None` everywhere a
/// sender is accepted means a human turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSender {
    /// Stable id of the authoring agent, e.g. `discord:<botUserId>`.
    pub agent_id: String,
    /// Hops the message already travelled between agents, if known.
    #[serde(default)]
    pub hops: Option<u32>,
}

/// `agent_loop:` in `agent.yaml`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentLoopConfig {
    /// Consecutive agent-authored turns allowed per session before blocking.
    pub max_agent_turns: u32,
    /// Largest accepted `AgentSender::hops`.
    pub max_hops: u32,
}

impl Default for AgentLoopConfig {
    fn default() -> Self {
        Self {
            max_agent_turns: 8,
            max_hops: 5,
        }
    }
}

/// Per-session (= per-conversation) consecutive agent-turn counter.
pub struct LoopGuard {
    config: AgentLoopConfig,
    agent_turns: u32,
}

impl LoopGuard {
    pub fn new(config: AgentLoopConfig) -> Self {
        Self {
            config,
            agent_turns: 0,
        }
    }

    /// Admit a turn or say why it must be dropped. Human turns reset.
    pub fn admit(&mut self, sender: Option<&AgentSender>) -> Result<(), SilentReason> {
        let Some(sender) = sender else {
            self.agent_turns = 0;
            return Ok(());
        };
        if sender.hops.unwrap_or(0) > self.config.max_hops {
            return Err(SilentReason::MaxHops);
        }
        if self.agent_turns >= self.config.max_agent_turns {
            return Err(SilentReason::MaxAgentTurns);
        }
        self.agent_turns += 1;
        Ok(())
    }
}

struct GuardedSession {
    inner: Box<dyn ChatSession>,
    guard: LoopGuard,
    /// Weak so the caller still sees channel close when the backend dies.
    tx: WeakSender<ChatEvent>,
}

impl ChatSession for GuardedSession {
    fn send_turn(&mut self, prompt: String) -> BoxFuture<'_, anyhow::Result<()>> {
        self.send_turn_from(prompt, None)
    }

    fn send_turn_from(
        &mut self,
        prompt: String,
        sender: Option<AgentSender>,
    ) -> BoxFuture<'_, anyhow::Result<()>> {
        match self.guard.admit(sender.as_ref()) {
            Ok(()) => self.inner.send_turn(prompt),
            Err(reason) => {
                tracing::warn!(
                    agent_id = sender.as_ref().map(|s| s.agent_id.as_str()),
                    reason = reason.as_str(),
                    "agent loop guard blocked turn"
                );
                let tx = self.tx.upgrade();
                Box::pin(async move {
                    if let Some(tx) = tx {
                        let _ = tx
                            .send(ChatEvent::Silent {
                                reason: Some(reason),
                                text: String::new(),
                            })
                            .await;
                        let _ = tx
                            .send(ChatEvent::TurnFinished {
                                ok: true,
                                error: None,
                            })
                            .await;
                    }
                    Ok(())
                })
            }
        }
    }

    fn abort(&mut self) -> BoxFuture<'_, anyhow::Result<()>> {
        self.inner.abort()
    }

    fn close(self: Box<Self>) -> BoxFuture<'static, anyhow::Result<()>> {
        self.inner.close()
    }

    fn answer_question(
        &mut self,
        request_id: String,
        answers: Vec<Vec<String>>,
    ) -> BoxFuture<'_, anyhow::Result<()>> {
        self.inner.answer_question(request_id, answers)
    }
}

/// Open a backend session with `NO_REPLY` filtering and the loop guard
/// applied. `open` receives the sender the raw backend must emit on; the
/// caller's `tx` gets the filtered stream.
pub async fn open_guarded<F, Fut>(
    config: AgentLoopConfig,
    tx: Sender<ChatEvent>,
    open: F,
) -> anyhow::Result<Box<dyn ChatSession>>
where
    F: FnOnce(Sender<ChatEvent>) -> Fut,
    Fut: Future<Output = anyhow::Result<Box<dyn ChatSession>>>,
{
    open_shaped(config, None, tx, open).await
}

/// [`open_guarded`] plus the human-chat stage (hold, lint, one rewrite,
/// `/topics`) when `shape` is set. `None` = identical to [`open_guarded`].
pub async fn open_shaped<F, Fut>(
    config: AgentLoopConfig,
    shape: Option<HumanChat>,
    tx: Sender<ChatEvent>,
    open: F,
) -> anyhow::Result<Box<dyn ChatSession>>
where
    F: FnOnce(Sender<ChatEvent>) -> Fut,
    Fut: Future<Output = anyhow::Result<Box<dyn ChatSession>>>,
{
    let (raw_tx, mut raw_rx) = mpsc::channel(tx.max_capacity());
    let (mut shaper, handles) = match shape.map(Shaper::new) {
        Some((s, h)) => (Some(s), Some(h)),
        None => (None, None),
    };
    let out = tx.clone();
    tokio::spawn(async move {
        let mut filter = NoReplyFilter::default();
        let mut activity = ActivityFilter::default();
        while let Some(event) = raw_rx.recv().await {
            for event in filter
                .push(event)
                .into_iter()
                .flat_map(|e| activity.push(e))
            {
                let open = match shaper.as_mut() {
                    Some(shaper) => shaper.push(event, &tx).await,
                    None => tx.send(event).await.is_ok(),
                };
                if !open {
                    return;
                }
            }
        }
    });
    let weak = raw_tx.downgrade();
    let inner = open(raw_tx).await?;
    let guarded: Box<dyn ChatSession> = Box::new(GuardedSession {
        inner,
        guard: LoopGuard::new(config),
        tx: weak,
    });
    Ok(match handles {
        Some(handles) => handles.attach(guarded, &out).await,
        None => guarded,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_token_variants_only() {
        for s in [
            "NO_REPLY",
            "  NO_REPLY\n",
            "**NO_REPLY**",
            "*NO_REPLY*",
            "`NO_REPLY`",
            "NO_REPLY.",
            "**NO_REPLY.**",
        ] {
            assert!(is_no_reply(s), "{s:?}");
        }
        for s in [
            "Hello NO_REPLY world",
            "NO_REPLY please",
            "NO_REPL",
            "",
            "no_reply",
        ] {
            assert!(!is_no_reply(s), "{s:?}");
        }
    }

    fn run(events: Vec<ChatEvent>) -> Vec<String> {
        let mut f = NoReplyFilter::default();
        events
            .into_iter()
            .flat_map(|e| f.push(e))
            .map(|e| match e {
                ChatEvent::Delta { text, .. } => format!("text:{text}"),
                ChatEvent::Silent { text, .. } => format!("silent:{text}"),
                ChatEvent::ToolCall { .. } => "tool".into(),
                ChatEvent::Error(e) => format!("error:{e}"),
                ChatEvent::TurnFinished { .. } => "done".into(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    fn done() -> ChatEvent {
        ChatEvent::TurnFinished {
            ok: true,
            error: None,
        }
    }

    fn tool() -> ChatEvent {
        ChatEvent::ToolCall {
            id: "1".into(),
            name: "t".into(),
            args: String::new(),
        }
    }

    #[test]
    fn split_token_is_silent() {
        assert_eq!(
            run(vec![
                assistant("NO_".into()),
                assistant("REPLY".into()),
                done()
            ]),
            ["silent:NO_REPLY", "done"]
        );
    }

    #[test]
    fn non_token_passes_through() {
        assert_eq!(
            run(vec![
                assistant("NO".into()),
                assistant("pe".into()),
                assistant(" ok".into()),
                done()
            ]),
            ["text:NOpe", "text: ok", "done"]
        );
        assert_eq!(
            run(vec![assistant("Hi".into()), done()]),
            ["text:Hi", "done"]
        );
    }

    #[test]
    fn tool_boundary_flushes_and_resets() {
        assert_eq!(
            run(vec![
                assistant("NO_".into()),
                tool(),
                assistant("NO_REPLY".into()),
                done()
            ]),
            ["text:NO_", "tool", "silent:NO_REPLY", "done"]
        );
    }

    #[test]
    fn error_flushes() {
        assert_eq!(
            run(vec![
                assistant("NO_RE".into()),
                ChatEvent::Error("x".into()),
                done()
            ]),
            ["text:NO_RE", "error:x", "done"]
        );
    }

    #[test]
    fn thinking_passes_untouched() {
        let out = run(vec![
            ChatEvent::Delta {
                role: ChatRole::Thinking,
                text: "hm".into(),
            },
            assistant("NO_REPLY".into()),
            done(),
        ]);
        assert_eq!(out, ["text:hm", "silent:NO_REPLY", "done"]);
    }

    fn thinking(text: &str) -> ChatEvent {
        ChatEvent::Delta {
            role: ChatRole::Thinking,
            text: text.into(),
        }
    }

    fn activity(events: Vec<ChatEvent>) -> Vec<String> {
        let mut f = ActivityFilter::default();
        events
            .into_iter()
            .flat_map(|e| f.push(e))
            .map(|e| match e {
                ChatEvent::Delta { role, text } => format!("{role:?}:{text}"),
                ChatEvent::Activity { label } => format!("label:{label}"),
                ChatEvent::ToolCall { .. } => "tool".into(),
                ChatEvent::TurnFinished { .. } => "done".into(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn marker_split_across_deltas_becomes_a_label() {
        assert_eq!(
            activity(vec![
                assistant("»".into()),
                assistant(" Read the ".into()),
                assistant("chat styles\n".into()),
                tool(),
                done(),
            ]),
            ["label:Read the chat styles", "tool", "done"]
        );
    }

    #[test]
    fn label_is_sent_at_once_before_a_late_tool_call() {
        // chat-codex sends the ToolCall only after the tool finished.
        let mut f = ActivityFilter::default();
        let out = f.push(assistant("» Check git status\n".into()));
        assert!(matches!(
            out.as_slice(),
            [ChatEvent::Activity { label }] if label == "Check git status"
        ));
    }

    #[test]
    fn unterminated_marker_line_becomes_a_label_at_the_tool_call() {
        assert_eq!(
            activity(vec![assistant("» Read it".into()), tool(), done()]),
            ["label:Read it", "tool", "done"]
        );
    }

    #[test]
    fn normal_text_is_not_delayed() {
        assert_eq!(
            activity(vec![
                assistant("Hel".into()),
                assistant("lo\nsecond".into()),
                done()
            ]),
            ["Assistant:Hel", "Assistant:lo\nsecond", "done"]
        );
        // Only the line start glyph is held; a lone glyph is released as text.
        assert_eq!(
            activity(vec![assistant("»x".into()), done()]),
            ["Assistant:»x", "done"]
        );
    }

    #[test]
    fn text_around_a_marker_keeps_its_order() {
        assert_eq!(
            activity(vec![
                assistant("I look.\n» Read a file\nDone looking".into()),
                tool(),
            ]),
            [
                "Assistant:I look.\n",
                "label:Read a file",
                "Assistant:Done looking",
                "tool"
            ]
        );
    }

    #[test]
    fn marker_inside_a_code_fence_is_ignored() {
        assert_eq!(
            activity(vec![
                assistant("```\n» Read a file\n```\n".into()),
                assistant("» Real label\n".into()),
                tool(),
            ]),
            [
                "Assistant:```\n» Read a file\n```\n",
                "label:Real label",
                "tool"
            ]
        );
    }

    #[test]
    fn orphan_marker_at_turn_end_is_text_without_the_glyph() {
        assert_eq!(
            activity(vec![assistant("» All done".into()), done()]),
            ["Assistant:All done", "done"]
        );
    }

    #[test]
    fn malformed_markers_stay_text() {
        for text in ["»\n", "» \n", "»no space\n", "mid » line\n"] {
            assert_eq!(
                activity(vec![assistant(text.into()), done()]),
                [format!("Assistant:{text}"), "done".into()],
                "{text:?}"
            );
        }
        let long = format!("» {}\n", "x".repeat(ACTIVITY_LABEL_MAX + 1));
        assert_eq!(
            activity(vec![assistant(long.clone()), done()]),
            [format!("Assistant:{long}"), "done".into()]
        );
    }

    #[test]
    fn thinking_is_never_a_marker() {
        assert_eq!(
            activity(vec![thinking("» Plan the work\n"), tool()]),
            ["Thinking:» Plan the work\n", "tool"]
        );
    }

    #[test]
    fn thinking_between_chunks_starts_a_new_line() {
        // Interleaved thinking: the marker is the first text of a line again.
        assert_eq!(
            activity(vec![
                assistant("Let me check.".into()),
                thinking("hm"),
                assistant("» Read x\n".into()),
                tool(),
            ]),
            [
                "Assistant:Let me check.",
                "Thinking:hm",
                "label:Read x",
                "tool"
            ]
        );
    }

    #[test]
    fn turn_edges_reset_fence_and_held_marker() {
        // An unclosed fence does not hide the next turn's marker.
        assert_eq!(
            activity(vec![
                assistant("```\n".into()),
                done(),
                assistant("» Read x\n".into()),
                tool(),
            ]),
            ["Assistant:```\n", "done", "label:Read x", "tool"]
        );
        // An aborted turn releases a half-held marker as text; the next turn starts clean.
        let aborted = ChatEvent::TurnFinished {
            ok: false,
            error: Some("aborted".into()),
        };
        assert_eq!(
            activity(vec![
                assistant("» Rea".into()),
                aborted,
                assistant("d\n".into()),
                done(),
            ]),
            ["Assistant:Rea", "done", "Assistant:d\n", "done"]
        );
        // A new turn start also resets the filter.
        let started = ChatEvent::TurnStarted {
            origin: crate::TurnOrigin::Submitted,
        };
        assert_eq!(
            activity(vec![
                assistant("```\n".into()),
                started,
                assistant("» Read x\n".into()),
                tool(),
            ]),
            [
                "Assistant:```\n",
                "TurnStarted { origin: Submitted }",
                "label:Read x",
                "tool"
            ]
        );
    }

    #[test]
    fn split_activity_projects_archived_text() {
        let text = "Looking.\n» Read the styles\n";
        assert_eq!(
            split_activity(text, true),
            [
                ActivityPart::Text("Looking.\n".into()),
                ActivityPart::Label("Read the styles".into())
            ]
        );
        assert_eq!(
            split_activity(text, false),
            [
                ActivityPart::Text("Looking.\n".into()),
                ActivityPart::Text("Read the styles\n".into())
            ]
        );
    }

    #[tokio::test]
    async fn pump_turns_marker_lines_into_activity_events() {
        let (tx, mut rx) = mpsc::channel(16);
        let session = open_guarded(AgentLoopConfig::default(), tx, |tx| async move {
            Ok(Box::new(Model {
                calls: Default::default(),
                tx,
                reply: "» Read the styles\n",
            }) as Box<dyn ChatSession>)
        })
        .await
        .unwrap();
        let mut session = session;
        session.send_turn("hi".into()).await.unwrap();
        let events = drain_turn(&mut rx).await;
        assert!(events
            .iter()
            .any(|e| matches!(e, ChatEvent::Activity { label } if label == "Read the styles")));
        assert!(!events.iter().any(|e| matches!(e, ChatEvent::Delta { .. })));
    }

    fn agent(hops: Option<u32>) -> AgentSender {
        AgentSender {
            agent_id: "discord:1".into(),
            hops,
        }
    }

    #[test]
    fn guard_counts_agent_turns_and_resets_on_human() {
        let mut g = LoopGuard::new(AgentLoopConfig {
            max_agent_turns: 2,
            max_hops: 5,
        });
        assert!(g.admit(Some(&agent(None))).is_ok());
        assert!(g.admit(Some(&agent(None))).is_ok());
        assert_eq!(
            g.admit(Some(&agent(None))),
            Err(SilentReason::MaxAgentTurns)
        );
        assert!(g.admit(None).is_ok());
        assert!(g.admit(Some(&agent(None))).is_ok());
    }

    #[test]
    fn guard_enforces_hops() {
        let mut g = LoopGuard::new(AgentLoopConfig::default());
        assert!(g.admit(Some(&agent(Some(5)))).is_ok());
        assert_eq!(g.admit(Some(&agent(Some(6)))), Err(SilentReason::MaxHops));
    }

    struct Model {
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        tx: Sender<ChatEvent>,
        reply: &'static str,
    }

    impl ChatSession for Model {
        fn send_turn(&mut self, _prompt: String) -> BoxFuture<'_, anyhow::Result<()>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let tx = self.tx.clone();
            let reply = self.reply;
            Box::pin(async move {
                tx.send(ChatEvent::TurnStarted {
                    origin: crate::TurnOrigin::Submitted,
                })
                .await?;
                tx.send(assistant(reply.into())).await?;
                tx.send(done()).await?;
                Ok(())
            })
        }
        fn abort(&mut self) -> BoxFuture<'_, anyhow::Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn close(self: Box<Self>) -> BoxFuture<'static, anyhow::Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    async fn open_model(
        config: AgentLoopConfig,
        reply: &'static str,
    ) -> (
        Box<dyn ChatSession>,
        mpsc::Receiver<ChatEvent>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel(16);
        let c = calls.clone();
        let session = open_guarded(config, tx, |tx| async move {
            Ok(Box::new(Model {
                calls: c,
                tx,
                reply,
            }) as Box<dyn ChatSession>)
        })
        .await
        .unwrap();
        (session, rx, calls)
    }

    async fn drain_turn(rx: &mut mpsc::Receiver<ChatEvent>) -> Vec<ChatEvent> {
        let mut out = Vec::new();
        while let Some(e) = rx.recv().await {
            let end = matches!(e, ChatEvent::TurnFinished { .. });
            out.push(e);
            if end {
                break;
            }
        }
        out
    }

    #[tokio::test]
    async fn silent_turn_delivers_no_text_but_finishes() {
        let (mut s, mut rx, _) = open_model(AgentLoopConfig::default(), "NO_REPLY").await;
        s.send_turn("hi".into()).await.unwrap();
        let events = drain_turn(&mut rx).await;
        assert!(!events.iter().any(|e| matches!(e, ChatEvent::Delta { .. })));
        assert!(events.iter().any(|e| matches!(
            e,
            ChatEvent::Silent { reason: None, text } if text == "NO_REPLY"
        )));
        assert!(matches!(
            events.last(),
            Some(ChatEvent::TurnFinished { ok: true, .. })
        ));
    }

    #[tokio::test]
    async fn blocked_turn_skips_model_and_only_finishes() {
        let config = AgentLoopConfig {
            max_agent_turns: 1,
            max_hops: 5,
        };
        let (mut s, mut rx, calls) = open_model(config, "hi").await;
        s.send_turn_from("a".into(), Some(agent(None)))
            .await
            .unwrap();
        drain_turn(&mut rx).await;
        s.send_turn_from("b".into(), Some(agent(None)))
            .await
            .unwrap();
        let events = drain_turn(&mut rx).await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(matches!(
            events.as_slice(),
            [
                ChatEvent::Silent {
                    reason: Some(SilentReason::MaxAgentTurns),
                    ..
                },
                ChatEvent::TurnFinished { ok: true, .. }
            ]
        ));
    }

    #[tokio::test]
    async fn backend_drop_closes_caller_channel() {
        let (s, mut rx, _) = open_model(AgentLoopConfig::default(), "hi").await;
        drop(s);
        assert!(rx.recv().await.is_none());
    }
}
