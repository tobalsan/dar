//! human-chat adapter: short-turn replies for interactive human chat.
//!
//! Off unless `agent.yaml` has `extensions.human-chat`. When on, the stream
//! holds assistant text until the turn ends, lints it with the human-chat
//! core CLI (`node <core>/dist/cli.js lint`), and either delivers it or asks
//! the agent for one rewrite. Rules, nudge, and the `reply_mode` tool live in
//! the Pi extension (`pi_extension`). Lint failures open: the reply is sent
//! unchanged.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::mpsc::Sender;
use tokio::sync::watch;

use crate::{parse_compact_command, BoxFuture, ChatEvent, ChatRole, ChatSession, TurnOrigin};

/// Invisible prefix on an interactive human prompt. The Pi extension strips
/// it in its `input` hook and shapes only that run (rules, nudge, tool).
pub(crate) const HUMAN_MARKER: &str = "\u{2063}human-chat:human\u{2063}";
/// Same, for the rewrite hint: rules and tool, no nudge.
pub(crate) const REWRITE_MARKER: &str = "\u{2063}human-chat:rewrite\u{2063}";

const REWRITE_PREFIX: &str = "[human-chat] ";
const CLI_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_CORE: &str = "~/code/agentropy/human-chat";

/// Resolved `extensions.human-chat` block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HumanChat {
    pub pi_extension: PathBuf,
    core: PathBuf,
    log: PathBuf,
    topics_dir: PathBuf,
    agent: String,
    /// Optional reply limits, in `LIMIT_KEYS` order. Unset or invalid keys
    /// are `None` so the core picks its defaults.
    limits: [Option<u32>; 4],
}

/// `(agent.yaml key, core CLI flag, Pi child env var)` for each reply limit.
const LIMIT_KEYS: [(&str, &str, &str); 4] = [
    (
        "max_sentences",
        "--max-sentences",
        "HUMAN_CHAT_MAX_SENTENCES",
    ),
    (
        "sentence_leeway",
        "--sentence-leeway",
        "HUMAN_CHAT_SENTENCE_LEEWAY",
    ),
    (
        "max_sentence_words",
        "--max-sentence-words",
        "HUMAN_CHAT_MAX_SENTENCE_WORDS",
    ),
    ("word_leeway", "--word-leeway", "HUMAN_CHAT_WORD_LEEWAY"),
];

#[derive(Default, Deserialize)]
#[serde(default)]
struct Block {
    enabled: Option<bool>,
    log: Option<String>,
    core: Option<String>,
    pi_extension: Option<String>,
}

fn expand(path: &str, root: &Path) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return Path::new(&home).join(rest);
        }
    }
    root.join(path)
}

impl HumanChat {
    /// `None` when the block is absent, unreadable, or `enabled: false`.
    pub fn load(root: &Path) -> Option<Self> {
        let yaml = std::fs::read_to_string(root.join("agent.yaml")).ok()?;
        let doc: Value = serde_yaml::from_str(&yaml).ok()?;
        let block = doc.get("extensions")?.get("human-chat")?;
        let block_value = block.clone();
        let block: Block = if block.is_null() {
            Block::default()
        } else {
            serde_json::from_value(block.clone()).ok()?
        };
        if block.enabled == Some(false) {
            return None;
        }
        let core = expand(block.core.as_deref().unwrap_or(DEFAULT_CORE), root);
        let limits = LIMIT_KEYS.map(|(key, _, _)| {
            let n = block_value.get(key)?.as_u64()?;
            u32::try_from(n).ok()
        });
        Some(Self {
            limits,
            pi_extension: block
                .pi_extension
                .map_or_else(|| core.join("pi/extension.ts"), |p| expand(&p, root)),
            log: block.log.map_or_else(
                || root.join("data/human-chat/long-mode.jsonl"),
                |p| expand(&p, root),
            ),
            topics_dir: root.join("topics"),
            agent: root
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
            core,
        })
    }

    /// Env vars that hand the configured limits to the Pi child.
    pub fn limit_env(&self) -> Vec<(&'static str, String)> {
        LIMIT_KEYS
            .iter()
            .zip(self.limits)
            .filter_map(|((_, _, env), n)| Some((*env, n?.to_string())))
            .collect()
    }

    /// Run the core CLI; `None` on spawn failure, timeout, or bad exit code.
    async fn cli(&self, args: &[String], stdin: &str) -> Option<(i32, String)> {
        let mut cmd = Command::new("node");
        cmd.arg(self.core.join("dist/cli.js"))
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let run = async {
            let mut child = cmd.spawn().ok()?;
            let mut pipe = child.stdin.take()?;
            pipe.write_all(stdin.as_bytes()).await.ok()?;
            drop(pipe);
            let out = child.wait_with_output().await.ok()?;
            Some((
                out.status.code()?,
                String::from_utf8_lossy(&out.stdout).into(),
            ))
        };
        tokio::time::timeout(CLI_TIMEOUT, run).await.ok().flatten()
    }

    /// Lint `text`. `Err(())`-like `None` means the linter failed: fail open.
    async fn lint(
        &self,
        text: &str,
        decl: Option<&(String, String)>,
        session: &str,
    ) -> Option<Lint> {
        let mut args: Vec<String> = [
            "lint",
            "--json",
            "--agent",
            &self.agent,
            "--session",
            session,
        ]
        .map(String::from)
        .into();
        for ((_, flag, _), n) in LIMIT_KEYS.iter().zip(self.limits) {
            if let Some(n) = n {
                args.extend([flag.to_string(), n.to_string()]);
            }
        }
        if let Some((kind, note)) = decl {
            args.extend(["--mode", "long", "--type", kind, "--note", note].map(String::from));
            if let Some(dir) = self.log.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            args.extend(["--log".into(), self.log.to_string_lossy().into_owned()]);
        }
        let (code, out) = self.cli(&args, text).await?;
        let v: Value = serde_json::from_str(&out).ok().filter(|_| code <= 1)?;
        Some(Lint {
            ok: v["ok"].as_bool()?,
            hint: v["hint"].as_str().unwrap_or_default().into(),
            rules: v["violations"]
                .as_array()?
                .iter()
                .filter_map(|x| x["rule"].as_str().map(String::from))
                .collect(),
        })
    }

    /// Reply text for `/topics`.
    async fn topics(&self) -> String {
        let dir = self.topics_dir.to_string_lossy().into_owned();
        let list = self.cli(&["topics".into(), "--dir".into(), dir], "").await;
        let mut lines = Vec::new();
        for line in list.iter().flat_map(|(_, out)| out.lines()) {
            lines.push(line.to_string());
            if let Some((_, file)) = line.rsplit_once(": ") {
                if let Some((_, out)) = self.cli(&["topics".into(), file.into()], "").await {
                    lines.extend(out.lines().map(String::from));
                }
            }
        }
        if lines.is_empty() {
            "No open topics.".into()
        } else {
            lines.join("\n")
        }
    }

    fn log_rewrite_failed(&self, session: &str, rules: &[String]) {
        let line = serde_json::json!({
            "ts": iso_now(), "agent": self.agent, "session": session,
            "event": "rewrite_failed", "rules": rules,
        });
        if let Some(dir) = self.log.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let write = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .and_then(|mut f| std::io::Write::write_all(&mut f, format!("{line}\n").as_bytes()));
        if let Err(e) = write {
            tracing::warn!("human-chat: cannot write log: {e}");
        }
    }
}

struct Lint {
    ok: bool,
    hint: String,
    rules: Vec<String>,
}

fn iso_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

type Inner = Arc<tokio::sync::Mutex<Option<Box<dyn ChatSession>>>>;

/// How the shaper treats one accepted run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// Interactive human turn: hold, lint, maybe ask for one rewrite.
    Human,
    /// The one rewrite the shaper asked for: hold, lint, never rewrite again.
    Rewrite,
    /// Everything else (agent, background, command): pass through untouched.
    Skip,
}

/// One entry per accepted submitted turn, in the order the backend runs them.
/// Pushed while holding the `inner` lock, so it matches backend order.
type Queue = Arc<Mutex<VecDeque<Kind>>>;

/// Session wrapper: tags each turn with its [`Kind`], answers `/topics`
/// locally, and cancels a pending lint on abort.
pub(crate) struct ShapedSession {
    inner: Inner,
    queue: Queue,
    shape: HumanChat,
    out: tokio::sync::mpsc::WeakSender<ChatEvent>,
    abort: watch::Sender<u64>,
}

impl ShapedSession {
    /// `human`: the caller declared an interactive human source.
    fn submit(
        &mut self,
        prompt: String,
        sender: Option<crate::AgentSender>,
        human: bool,
    ) -> BoxFuture<'_, anyhow::Result<()>> {
        let topics = human && prompt.trim() == "/topics";
        // Pi runs `/...` prompts as commands and `/compact` has its own
        // command: a marker prefix would break both, so they stay unshaped.
        let shaped = human && !prompt.starts_with('/') && parse_compact_command(&prompt).is_none();
        let (kind, prompt) = if shaped {
            (Kind::Human, format!("{HUMAN_MARKER}{prompt}"))
        } else {
            (Kind::Skip, prompt)
        };
        let (inner, queue, shape, out) = (
            self.inner.clone(),
            self.queue.clone(),
            self.shape.clone(),
            self.out.upgrade(),
        );
        Box::pin(async move {
            if topics {
                if let Some(out) = out {
                    let text = shape.topics().await;
                    let _ = out
                        .send(ChatEvent::Delta {
                            role: ChatRole::Assistant,
                            text,
                        })
                        .await;
                    let _ = out
                        .send(ChatEvent::TurnFinished {
                            ok: true,
                            error: None,
                        })
                        .await;
                }
                return Ok(());
            }
            let mut guard = inner.lock().await;
            let session = guard
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("chat session closed"))?;
            queue.lock().unwrap().push_back(kind);
            let result = session.send_turn_from(prompt, sender).await;
            if result.is_err() {
                queue.lock().unwrap().pop_back();
            }
            result
        })
    }
}

impl ChatSession for ShapedSession {
    fn send_turn(&mut self, prompt: String) -> BoxFuture<'_, anyhow::Result<()>> {
        self.submit(prompt, None, false)
    }

    fn send_turn_from(
        &mut self,
        prompt: String,
        sender: Option<crate::AgentSender>,
    ) -> BoxFuture<'_, anyhow::Result<()>> {
        self.submit(prompt, sender, false)
    }

    fn send_human_turn(&mut self, prompt: String) -> BoxFuture<'_, anyhow::Result<()>> {
        self.submit(prompt, None, true)
    }

    fn abort(&mut self) -> BoxFuture<'_, anyhow::Result<()>> {
        // Before reaching the backend: a pending lint must not start a rewrite.
        self.abort.send_modify(|n| *n += 1);
        let inner = self.inner.clone();
        Box::pin(async move {
            match inner.lock().await.as_mut() {
                Some(s) => s.abort().await,
                None => Ok(()),
            }
        })
    }

    fn close(self: Box<Self>) -> BoxFuture<'static, anyhow::Result<()>> {
        Box::pin(async move {
            let session = self.inner.lock().await.take();
            match session {
                Some(s) => s.close().await,
                None => Ok(()),
            }
        })
    }

    fn answer_question(
        &mut self,
        request_id: String,
        answers: Vec<Vec<String>>,
    ) -> BoxFuture<'_, anyhow::Result<()>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            match inner.lock().await.as_mut() {
                Some(s) => s.answer_question(request_id, answers).await,
                None => anyhow::bail!("chat session closed"),
            }
        })
    }
}

/// Output of the run in progress, held until its turn ends.
#[derive(Default)]
struct Held {
    thinking: String,
    /// Assistant text segments, split at tool and thinking boundaries.
    segs: Vec<String>,
    seg_open: bool,
    /// `NO_REPLY` markers, released after the text like the unshaped order.
    silent: Vec<ChatEvent>,
}

impl Held {
    fn text(&self) -> String {
        self.segs.join("\n\n")
    }
}

/// The run being processed.
struct Run {
    kind: Kind,
    /// Abort count when the run started; a change means the user aborted.
    abort_gen: u64,
    /// Started while an earlier rewrite was pending: output queues behind it.
    buffered: bool,
    held: Held,
    decl: Option<(String, String)>,
}

/// Output of one run (or one pending rewrite), delivered in conversation order.
#[derive(Default)]
struct Section {
    /// The run failed lint and its rewrite has not finished yet.
    awaiting: bool,
    events: Vec<ChatEvent>,
}

/// Event-stream stage: holds shaped-run output, lints at turn end, and keeps
/// the caller's event order when a rewrite runs after other queued turns.
pub(crate) struct Shaper {
    shape: HumanChat,
    /// Weak: the backend owns the sender this task reads, so a strong handle
    /// here would keep the session (and this task) alive after the caller
    /// drops it.
    inner: Weak<tokio::sync::Mutex<Option<Box<dyn ChatSession>>>>,
    queue: Queue,
    aborts: watch::Receiver<u64>,
    session: String,
    run: Option<Run>,
    deferred: VecDeque<Section>,
}

impl Shaper {
    fn begin(&mut self, kind: Kind) {
        let buffered = kind != Kind::Rewrite && !self.deferred.is_empty();
        if buffered {
            self.deferred.push_back(Section::default());
        }
        self.run = Some(Run {
            kind,
            abort_gen: *self.aborts.borrow(),
            buffered,
            held: Held::default(),
            decl: None,
        });
    }

    fn pop_kind(&self) -> Kind {
        self.queue.lock().unwrap().pop_front().unwrap_or(Kind::Skip)
    }

    /// The current run; starts one (a submitted turn) if the backend sent no
    /// `TurnStarted`.
    fn run_mut(&mut self) -> &mut Run {
        if self.run.is_none() {
            let kind = self.pop_kind();
            self.begin(kind);
        }
        self.run.as_mut().expect("run begun above")
    }

    /// Send a turn-stream event in conversation order: live when nothing is
    /// pending, else into the pending section it belongs to.
    async fn route(&mut self, event: ChatEvent, tx: &Sender<ChatEvent>) -> bool {
        let rewrite = self.run.as_ref().is_some_and(|r| r.kind == Kind::Rewrite);
        let slot = if rewrite {
            self.deferred.iter_mut().find(|s| s.awaiting)
        } else {
            self.deferred.back_mut()
        };
        match slot {
            Some(section) => {
                section.events.push(event);
                true
            }
            None => tx.send(event).await.is_ok(),
        }
    }

    /// Send every finished leading section (`all`: even pending ones).
    async fn flush(&mut self, all: bool, tx: &Sender<ChatEvent>) -> bool {
        while self.deferred.front().is_some_and(|s| all || !s.awaiting) {
            for event in self.deferred.pop_front().expect("front checked").events {
                if tx.send(event).await.is_err() {
                    return false;
                }
            }
        }
        true
    }

    /// Release the held output of the current run.
    async fn release(&mut self, tx: &Sender<ChatEvent>) -> bool {
        let held = self
            .run
            .as_mut()
            .map(|r| std::mem::take(&mut r.held))
            .unwrap_or_default();
        if !held.thinking.is_empty() {
            let thinking = ChatEvent::Delta {
                role: ChatRole::Thinking,
                text: held.thinking.clone(),
            };
            if !self.route(thinking, tx).await {
                return false;
            }
        }
        if !held.segs.is_empty() {
            let text = ChatEvent::Delta {
                role: ChatRole::Assistant,
                text: held.text(),
            };
            if !self.route(text, tx).await {
                return false;
            }
        }
        for event in held.silent {
            if !self.route(event, tx).await {
                return false;
            }
        }
        true
    }

    /// Process one event; `false` when `tx` is closed.
    pub(crate) async fn push(&mut self, event: ChatEvent, tx: &Sender<ChatEvent>) -> bool {
        match event {
            ChatEvent::SessionIdentity { ref id } => {
                self.session = id.clone();
                tx.send(event).await.is_ok()
            }
            ChatEvent::TurnStarted { origin } => {
                let kind = match origin {
                    TurnOrigin::Submitted => self.pop_kind(),
                    TurnOrigin::Autonomous => Kind::Skip,
                };
                self.begin(kind);
                // The rewrite continues the failed turn the caller saw start.
                kind == Kind::Rewrite || self.route(event, tx).await
            }
            ChatEvent::Delta { role, text } => {
                let run = self.run_mut();
                if run.kind == Kind::Skip {
                    return self.route(ChatEvent::Delta { role, text }, tx).await;
                }
                let held = &mut run.held;
                match role {
                    ChatRole::Assistant => {
                        if !held.seg_open {
                            held.segs.push(String::new());
                            held.seg_open = true;
                        }
                        held.segs.last_mut().expect("segment").push_str(&text);
                    }
                    ChatRole::Thinking => {
                        held.seg_open = false;
                        held.thinking.push_str(&text);
                    }
                }
                true
            }
            ChatEvent::ToolCall {
                ref name, ref args, ..
            } => {
                let run = self.run_mut();
                run.held.seg_open = false;
                if name == "reply_mode" && run.kind != Kind::Skip {
                    if let Ok(v) = serde_json::from_str::<Value>(args) {
                        if v["mode"] == "long" {
                            let field = |k: &str| v[k].as_str().unwrap_or_default().to_string();
                            run.decl = Some((field("type"), field("note")));
                        }
                    }
                }
                self.route(event, tx).await
            }
            ChatEvent::ToolOutput { .. } => {
                self.run_mut().held.seg_open = false;
                self.route(event, tx).await
            }
            ChatEvent::Silent { .. } => {
                let run = self.run_mut();
                if run.kind == Kind::Skip {
                    return self.route(event, tx).await;
                }
                run.held.silent.push(event);
                true
            }
            ChatEvent::Compacted { .. } => self.route(event, tx).await,
            ChatEvent::SessionClosed { .. } => {
                // No more events will come: release everything as is.
                let sent = self.release(tx).await && self.flush(true, tx).await;
                self.run = None;
                sent && tx.send(event).await.is_ok()
            }
            ChatEvent::TurnFinished { ok, .. } => self.finish(event, ok, tx).await,
            // Live: errors and notices (not turn ends), questions, usage, ...
            other => tx.send(other).await.is_ok(),
        }
    }

    /// Lint `text`; `None` = linter failed (fail open) or the user aborted.
    async fn lint(&self, text: &str) -> Option<Lint> {
        let run = self.run.as_ref()?;
        let mut aborts = self.aborts.clone();
        let abort_gen = run.abort_gen;
        let cancelled = async move {
            loop {
                if *aborts.borrow_and_update() != abort_gen {
                    return;
                }
                if aborts.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
        };
        tokio::select! {
            lint = self.shape.lint(text, run.decl.as_ref(), &self.session) => lint,
            () = cancelled => None,
        }
    }

    /// Queue the one rewrite turn behind everything already accepted.
    async fn request_rewrite(&self, hint: &str) -> bool {
        let Some(inner) = self.inner.upgrade() else {
            return false;
        };
        let mut guard = inner.lock().await;
        let Some(session) = guard.as_mut() else {
            return false;
        };
        self.queue.lock().unwrap().push_back(Kind::Rewrite);
        let sent = session
            .send_turn(format!("{REWRITE_MARKER}{REWRITE_PREFIX}{hint}"))
            .await
            .is_ok();
        if !sent {
            self.queue.lock().unwrap().pop_back();
        }
        sent
    }

    async fn finish(&mut self, event: ChatEvent, ok: bool, tx: &Sender<ChatEvent>) -> bool {
        let run = self.run_mut();
        let (kind, abort_gen, text) = (run.kind, run.abort_gen, run.held.text());
        if kind != Kind::Skip && ok && !text.trim().is_empty() {
            match self.lint(&text).await {
                None => tracing::warn!("human-chat: lint failed or aborted; sending reply as is"),
                Some(lint) if !lint.ok && kind == Kind::Rewrite => {
                    self.shape.log_rewrite_failed(&self.session, &lint.rules);
                }
                Some(lint) if !lint.ok && *self.aborts.borrow() == abort_gen => {
                    if self.request_rewrite(&lint.hint).await {
                        // Draft dropped; its finish comes with the rewrite's.
                        let own = self.run.as_ref().is_some_and(|r| r.buffered);
                        match self.deferred.back_mut().filter(|_| own) {
                            Some(section) => section.awaiting = true,
                            None => self.deferred.push_back(Section {
                                awaiting: true,
                                events: Vec::new(),
                            }),
                        }
                        self.run = None;
                        return true;
                    }
                }
                Some(_) => {}
            }
        }
        let mut sent = self.release(tx).await && self.route(event, tx).await;
        if kind == Kind::Rewrite {
            if let Some(section) = self.deferred.iter_mut().find(|s| s.awaiting) {
                section.awaiting = false;
            }
            sent = sent && self.flush(false, tx).await;
        }
        self.run = None;
        sent
    }
}

/// Shared state between the stream stage and the session wrapper.
pub(crate) struct Handles {
    shape: HumanChat,
    inner: Inner,
    queue: Queue,
    abort: watch::Sender<u64>,
}

impl Handles {
    /// Install the guarded session and return the wrapper callers drive.
    pub(crate) async fn attach(
        self,
        guarded: Box<dyn ChatSession>,
        tx: &Sender<ChatEvent>,
    ) -> Box<dyn ChatSession> {
        *self.inner.lock().await = Some(guarded);
        Box::new(ShapedSession {
            inner: self.inner,
            queue: self.queue,
            shape: self.shape,
            out: tx.downgrade(),
            abort: self.abort,
        })
    }
}

impl Shaper {
    pub(crate) fn new(shape: HumanChat) -> (Self, Handles) {
        let inner = Inner::default();
        let queue = Queue::default();
        let (abort, aborts) = watch::channel(0);
        let shaper = Self {
            shape: shape.clone(),
            inner: Arc::downgrade(&inner),
            queue: queue.clone(),
            aborts,
            session: String::new(),
            run: None,
            deferred: VecDeque::new(),
        };
        (
            shaper,
            Handles {
                shape,
                inner,
                queue,
                abort,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{open_shaped, AgentLoopConfig, AgentSender};
    use tokio::sync::mpsc;

    const STUB: &str = r#"const fs = require("fs");
const a = process.argv.slice(2);
if (a[0] === "topics") {
  console.log(a[1] === "--dir" ? "Plan A (1 open): /x/a.md" : "> First");
  process.exit(0);
}
let t = "";
process.stdin.on("data", (d) => (t += d)).on("end", () => {
  const bad = t.includes("BAD") && !a.includes("--sentence-leeway"), long = a.includes("--mode");
  const log = a[a.indexOf("--log") + 1];
  if (log && long) fs.appendFileSync(log, JSON.stringify({ type: a[a.indexOf("--type") + 1] }) + "\n");
  setTimeout(() => {
    console.log(JSON.stringify({ ok: !bad || long, hint: "HINT", violations: bad ? [{ rule: "too_many_sentences" }] : [] }));
    process.exit(bad && !long ? 1 : 0);
  }, t.includes("SLOW") ? 400 : 0);
});
"#;

    fn setup(yaml: &str) -> (tempfile::TempDir, HumanChat) {
        let dir = tempfile::tempdir().unwrap();
        let core = dir.path().join("core");
        std::fs::create_dir_all(core.join("dist")).unwrap();
        std::fs::write(core.join("dist/cli.js"), STUB).unwrap();
        std::fs::write(
            dir.path().join("agent.yaml"),
            format!(
                "extensions:\n  human-chat:\n    core: {}\n{yaml}",
                core.display()
            ),
        )
        .unwrap();
        let shape = HumanChat::load(dir.path()).unwrap();
        (dir, shape)
    }

    fn text(s: &str) -> ChatEvent {
        ChatEvent::Delta {
            role: ChatRole::Assistant,
            text: s.into(),
        }
    }

    fn think(s: &str) -> ChatEvent {
        ChatEvent::Delta {
            role: ChatRole::Thinking,
            text: s.into(),
        }
    }

    fn done() -> ChatEvent {
        ChatEvent::TurnFinished {
            ok: true,
            error: None,
        }
    }

    fn started() -> ChatEvent {
        ChatEvent::TurnStarted {
            origin: TurnOrigin::Submitted,
        }
    }

    fn tool(name: &str) -> ChatEvent {
        ChatEvent::ToolCall {
            id: "1".into(),
            name: name.into(),
            args: String::new(),
        }
    }

    /// Replays one scripted event list per `send_turn`; records prompts.
    struct Model {
        tx: Sender<ChatEvent>,
        script: VecDeque<Vec<ChatEvent>>,
        prompts: Arc<Mutex<Vec<String>>>,
    }

    impl ChatSession for Model {
        fn send_turn(&mut self, prompt: String) -> BoxFuture<'_, anyhow::Result<()>> {
            self.prompts.lock().unwrap().push(prompt);
            let events = self.script.pop_front().unwrap_or_default();
            let tx = self.tx.clone();
            Box::pin(async move {
                tx.send(started()).await?;
                for e in events {
                    tx.send(e).await?;
                }
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

    type Opened = (
        Box<dyn ChatSession>,
        mpsc::Receiver<ChatEvent>,
        Arc<Mutex<Vec<String>>>,
    );

    async fn open(shape: Option<HumanChat>, script: Vec<Vec<ChatEvent>>) -> Opened {
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::channel(64);
        let p = prompts.clone();
        let session = open_shaped(AgentLoopConfig::default(), shape, tx, |tx| async move {
            Ok(Box::new(Model {
                tx,
                script: script.into(),
                prompts: p,
            }) as Box<dyn ChatSession>)
        })
        .await
        .unwrap();
        (session, rx, prompts)
    }

    /// Prompts the model saw, markers shown as `H|` (human) and `R|` (rewrite).
    fn seen(prompts: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        prompts
            .lock()
            .unwrap()
            .iter()
            .map(|p| p.replace(HUMAN_MARKER, "H|").replace(REWRITE_MARKER, "R|"))
            .collect()
    }

    fn label(e: &ChatEvent) -> String {
        match e {
            ChatEvent::Delta {
                role: ChatRole::Assistant,
                text,
            } => format!("text:{text}"),
            ChatEvent::Delta { text, .. } => format!("think:{text}"),
            ChatEvent::TurnFinished { .. } => "done".into(),
            ChatEvent::TurnStarted { .. } => "start".into(),
            ChatEvent::ToolCall { name, .. } => format!("tool:{name}"),
            other => format!("{other:?}"),
        }
    }

    /// Events of one turn as short strings.
    async fn turn(rx: &mut mpsc::Receiver<ChatEvent>) -> Vec<String> {
        let mut out = Vec::new();
        loop {
            let next = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
            let Ok(Some(e)) = next else {
                panic!("turn did not finish; got {out:?}")
            };
            out.push(label(&e));
            if matches!(e, ChatEvent::TurnFinished { .. }) {
                break;
            }
        }
        out
    }

    /// Everything already delivered, without waiting.
    fn drained(rx: &mut mpsc::Receiver<ChatEvent>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(e) = rx.try_recv() {
            out.push(label(&e));
        }
        out
    }

    /// A bare shaper plus its output channel, for event-level tests.
    fn bare(
        shape: HumanChat,
        kind: Kind,
    ) -> (Shaper, Sender<ChatEvent>, mpsc::Receiver<ChatEvent>) {
        let (shaper, handles) = Shaper::new(shape);
        handles.queue.lock().unwrap().push_back(kind);
        let (tx, rx) = mpsc::channel(32);
        (shaper, tx, rx)
    }

    #[test]
    fn config_switch() {
        let dir = tempfile::tempdir().unwrap();
        let load = |yaml: &str| {
            std::fs::write(dir.path().join("agent.yaml"), yaml).unwrap();
            HumanChat::load(dir.path())
        };
        assert!(load("runner: {}\n").is_none());
        assert!(load("extensions:\n  other: {}\n").is_none());
        assert!(load("extensions:\n  human-chat: {}\n").is_some());
        assert!(load("extensions:\n  human-chat:\n").is_some());
        assert!(load("extensions:\n  human-chat:\n    enabled: false\n").is_none());
        let on = load("extensions:\n  human-chat:\n    pi_extension: /x/e.ts\n").unwrap();
        assert_eq!(on.pi_extension, Path::new("/x/e.ts"));
        assert_eq!(on.log, dir.path().join("data/human-chat/long-mode.jsonl"));
    }

    #[test]
    fn limit_keys_parse_and_invalid_fall_back() {
        let load = |yaml: &str| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("agent.yaml"), yaml).unwrap();
            HumanChat::load(dir.path()).unwrap()
        };
        let on = load(
            "extensions:\n  human-chat:\n    max_sentences: 7\n    sentence_leeway: 1\n    \
             max_sentence_words: x\n    word_leeway: -2\n",
        );
        assert_eq!(on.limits, [Some(7), Some(1), None, None]);
        assert_eq!(
            on.limit_env(),
            [
                ("HUMAN_CHAT_MAX_SENTENCES", "7".to_string()),
                ("HUMAN_CHAT_SENTENCE_LEEWAY", "1".to_string())
            ]
        );
        assert_eq!(load("extensions:\n  human-chat: {}\n").limits, [None; 4]);
    }

    #[tokio::test]
    async fn lint_passes_the_configured_limits() {
        let (_d, plain) = setup("");
        assert!(!plain.lint("BAD", None, "s").await.unwrap().ok);
        let (_d, tuned) = setup("    sentence_leeway: 2\n");
        assert!(tuned.lint("BAD", None, "s").await.unwrap().ok);
    }

    #[tokio::test]
    async fn off_streams_deltas_before_turn_end() {
        let (mut s, mut rx, _) =
            open(None, vec![vec![text("BAD one"), text(" two"), done()]]).await;
        s.send_human_turn("hi".into()).await.unwrap();
        assert_eq!(
            turn(&mut rx).await,
            ["start", "text:BAD one", "text: two", "done"]
        );
    }

    #[tokio::test]
    async fn on_holds_text_then_passes() {
        let (_dir, shape) = setup("");
        let (mut s, mut rx, prompts) =
            open(Some(shape), vec![vec![text("Hi "), text("there."), done()]]).await;
        s.send_human_turn("hi".into()).await.unwrap();
        assert_eq!(turn(&mut rx).await, ["start", "text:Hi there.", "done"]);
        assert_eq!(seen(&prompts), ["H|hi"]);
    }

    #[tokio::test]
    async fn fail_sends_one_hint_and_delivers_rewrite() {
        let (_dir, shape) = setup("");
        let script = vec![
            vec![text("BAD draft"), done()],
            vec![text("Short."), done()],
        ];
        let (mut s, mut rx, prompts) = open(Some(shape), script).await;
        s.send_human_turn("hi".into()).await.unwrap();
        assert_eq!(turn(&mut rx).await, ["start", "text:Short.", "done"]);
        assert_eq!(seen(&prompts), ["H|hi", "R|[human-chat] HINT"]);
    }

    #[tokio::test]
    async fn failed_rewrite_is_sent_and_logged() {
        let (dir, shape) = setup("");
        let script = vec![vec![text("BAD one"), done()], vec![text("BAD two"), done()]];
        let (mut s, mut rx, prompts) = open(Some(shape), script).await;
        s.send_human_turn("hi".into()).await.unwrap();
        assert_eq!(turn(&mut rx).await, ["start", "text:BAD two", "done"]);
        assert_eq!(prompts.lock().unwrap().len(), 2);
        let log =
            std::fs::read_to_string(dir.path().join("data/human-chat/long-mode.jsonl")).unwrap();
        let line: Value = serde_json::from_str(log.trim()).unwrap();
        assert_eq!(line["event"], "rewrite_failed");
        assert_eq!(line["rules"][0], "too_many_sentences");
    }

    #[tokio::test]
    async fn reply_mode_tool_declares_long_mode_and_logs() {
        let (dir, shape) = setup("");
        let call = ChatEvent::ToolCall {
            id: "1".into(),
            name: "reply_mode".into(),
            args: r#"{"mode":"long","type":"steps","note":"user wants steps"}"#.into(),
        };
        let (mut s, mut rx, prompts) =
            open(Some(shape), vec![vec![call, text("BAD long"), done()]]).await;
        s.send_human_turn("hi".into()).await.unwrap();
        assert_eq!(
            turn(&mut rx).await,
            ["start", "tool:reply_mode", "text:BAD long", "done"]
        );
        assert_eq!(prompts.lock().unwrap().len(), 1);
        let log =
            std::fs::read_to_string(dir.path().join("data/human-chat/long-mode.jsonl")).unwrap();
        assert!(log.contains(r#""type":"steps""#));
    }

    #[tokio::test]
    async fn agent_sender_is_not_shaped() {
        let (_dir, shape) = setup("");
        let (mut s, mut rx, prompts) =
            open(Some(shape), vec![vec![text("BAD "), text("live"), done()]]).await;
        let sender = AgentSender {
            agent_id: "discord:1".into(),
            hops: None,
        };
        s.send_turn_from("hi".into(), Some(sender)).await.unwrap();
        assert_eq!(
            turn(&mut rx).await,
            ["start", "text:BAD ", "text:live", "done"]
        );
        assert_eq!(seen(&prompts), ["hi"]);
    }

    #[tokio::test]
    async fn turn_without_a_human_source_is_not_shaped() {
        let (_dir, shape) = setup("");
        let (mut s, mut rx, prompts) =
            open(Some(shape), vec![vec![text("BAD "), text("live"), done()]]).await;
        s.send_turn("background".into()).await.unwrap();
        assert_eq!(
            turn(&mut rx).await,
            ["start", "text:BAD ", "text:live", "done"]
        );
        assert_eq!(seen(&prompts), ["background"]);
    }

    #[tokio::test]
    async fn slash_commands_are_not_shaped_or_marked() {
        let (_dir, shape) = setup("");
        let script = vec![vec![text("BAD "), done()], vec![text("BAD "), done()]];
        let (mut s, mut rx, prompts) = open(Some(shape), script).await;
        s.send_human_turn("/skill:x go".into()).await.unwrap();
        assert_eq!(turn(&mut rx).await, ["start", "text:BAD ", "done"]);
        s.send_human_turn("/compact keep".into()).await.unwrap();
        assert_eq!(turn(&mut rx).await, ["start", "text:BAD ", "done"]);
        assert_eq!(seen(&prompts), ["/skill:x go", "/compact keep"]);
    }

    #[tokio::test]
    async fn autonomous_run_is_not_shaped() {
        let (dir, shape) = setup("");
        let (mut shaper, tx, mut rx) = bare(shape, Kind::Human);
        let origin = TurnOrigin::Autonomous;
        for e in [
            ChatEvent::TurnStarted { origin },
            text("BAD "),
            text("live"),
            done(),
        ] {
            assert!(shaper.push(e, &tx).await);
        }
        assert_eq!(
            turn(&mut rx).await,
            ["start", "text:BAD ", "text:live", "done"]
        );
        drop(dir);
    }

    #[tokio::test]
    async fn topics_command_skips_the_model() {
        let (_dir, shape) = setup("");
        let (mut s, mut rx, prompts) = open(Some(shape), vec![]).await;
        s.send_human_turn(" /topics ".into()).await.unwrap();
        assert_eq!(
            turn(&mut rx).await,
            ["text:Plan A (1 open): /x/a.md\n> First", "done"]
        );
        assert!(prompts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn lint_failure_fails_open() {
        let (_dir, mut shape) = setup("");
        shape.core = PathBuf::from("/nonexistent");
        let (mut s, mut rx, prompts) = open(Some(shape), vec![vec![text("BAD"), done()]]).await;
        s.send_human_turn("hi".into()).await.unwrap();
        assert_eq!(turn(&mut rx).await, ["start", "text:BAD", "done"]);
        assert_eq!(prompts.lock().unwrap().len(), 1);
    }

    fn rewrite_failed_lines(dir: &tempfile::TempDir) -> usize {
        std::fs::read_to_string(dir.path().join("data/human-chat/long-mode.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains("rewrite_failed"))
            .count()
    }

    #[tokio::test]
    async fn queued_turn_is_not_taken_for_the_rewrite() {
        let (dir, shape) = setup("");
        let script = vec![
            vec![text("BAD a1"), done()],
            vec![text("ok b1"), done()],
            vec![text("BAD a2"), done()],
        ];
        let (mut s, mut rx, prompts) = open(Some(shape), script).await;
        s.send_human_turn("A".into()).await.unwrap();
        s.send_human_turn("B".into()).await.unwrap();
        assert_eq!(turn(&mut rx).await, ["start", "text:BAD a2", "done"]);
        assert_eq!(turn(&mut rx).await, ["start", "text:ok b1", "done"]);
        assert_eq!(seen(&prompts), ["H|A", "H|B", "R|[human-chat] HINT"]);
        assert_eq!(rewrite_failed_lines(&dir), 1);
    }

    #[tokio::test]
    async fn each_queued_human_turn_gets_one_rewrite_in_order() {
        let (dir, shape) = setup("");
        let script = vec![
            vec![text("BAD a1"), done()],
            vec![text("BAD b1"), done()],
            vec![text("BAD a2"), done()],
            vec![text("Short b"), done()],
        ];
        let (mut s, mut rx, prompts) = open(Some(shape), script).await;
        s.send_human_turn("A".into()).await.unwrap();
        s.send_human_turn("B".into()).await.unwrap();
        assert_eq!(turn(&mut rx).await, ["start", "text:BAD a2", "done"]);
        assert_eq!(turn(&mut rx).await, ["start", "text:Short b", "done"]);
        assert_eq!(prompts.lock().unwrap().len(), 4);
        assert_eq!(rewrite_failed_lines(&dir), 1);
    }

    #[tokio::test]
    async fn abort_cancels_a_pending_lint_and_its_rewrite() {
        let (_dir, shape) = setup("");
        let script = vec![
            vec![text("BAD SLOW draft"), done()],
            vec![text("Short."), done()],
        ];
        let (mut s, mut rx, prompts) = open(Some(shape), script).await;
        s.send_human_turn("hi".into()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        s.abort().await.unwrap();
        assert_eq!(
            turn(&mut rx).await,
            ["start", "text:BAD SLOW draft", "done"]
        );
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(seen(&prompts), ["H|hi"]);
        assert!(drained(&mut rx).is_empty());
    }

    #[tokio::test]
    async fn nonterminal_error_keeps_text_held() {
        let (_dir, shape) = setup("");
        let (mut shaper, tx, mut rx) = bare(shape.clone(), Kind::Human);
        for e in [
            started(),
            text("BAD"),
            ChatEvent::Error("stderr notice".into()),
            text(" more"),
        ] {
            assert!(shaper.push(e, &tx).await);
        }
        assert_eq!(drained(&mut rx), ["start", "Error(\"stderr notice\")"]);
        let (mut s, mut rx, _) = open(
            Some(shape),
            vec![
                vec![
                    text("BAD"),
                    ChatEvent::Error("stderr".into()),
                    text(" more"),
                    done(),
                ],
                vec![text("Short."), done()],
            ],
        )
        .await;
        s.send_human_turn("hi".into()).await.unwrap();
        assert_eq!(
            turn(&mut rx).await,
            ["start", "Error(\"stderr\")", "text:Short.", "done"]
        );
    }

    #[tokio::test]
    async fn thinking_is_held_with_the_text() {
        let (_dir, shape) = setup("");
        let (mut shaper, tx, mut rx) = bare(shape, Kind::Human);
        for e in [started(), think("secret"), text("Hi.")] {
            assert!(shaper.push(e, &tx).await);
        }
        assert_eq!(drained(&mut rx), ["start"]);
        assert!(shaper.push(done(), &tx).await);
        assert_eq!(drained(&mut rx), ["think:secret", "text:Hi.", "done"]);
    }

    #[tokio::test]
    async fn failed_draft_thinking_is_never_shown() {
        let (_dir, shape) = setup("");
        let script = vec![
            vec![think("secret"), text("BAD draft"), done()],
            vec![think("calm"), text("Short."), done()],
        ];
        let (mut s, mut rx, _) = open(Some(shape), script).await;
        s.send_human_turn("hi".into()).await.unwrap();
        assert_eq!(
            turn(&mut rx).await,
            ["start", "think:calm", "text:Short.", "done"]
        );
    }

    #[tokio::test]
    async fn final_no_reply_after_a_tool_keeps_earlier_text() {
        let (_dir, shape) = setup("");
        let (mut shaper, tx, mut rx) = bare(shape, Kind::Human);
        let silent = ChatEvent::Silent {
            reason: None,
            text: "NO_REPLY".into(),
        };
        for e in [started(), text("Checked."), tool("t"), silent, done()] {
            assert!(shaper.push(e, &tx).await);
        }
        let mut got = drained(&mut rx);
        got.sort();
        let mut want = vec![
            "start".to_string(),
            "tool:t".into(),
            "text:Checked.".into(),
            "Silent { reason: None, text: \"NO_REPLY\" }".into(),
            "done".into(),
        ];
        want.sort();
        assert_eq!(got, want);
    }

    #[tokio::test]
    async fn blocked_turn_without_start_keeps_the_queue_aligned() {
        let (_dir, shape) = setup("");
        let (mut shaper, tx, mut rx) = bare(shape.clone(), Kind::Skip);
        let (_, handles) = Shaper::new(shape);
        drop(handles);
        shaper.queue.lock().unwrap().push_back(Kind::Human);
        let blocked = ChatEvent::Silent {
            reason: Some(crate::SilentReason::MaxHops),
            text: String::new(),
        };
        for e in [blocked, done(), started(), text("BAD one"), done()] {
            assert!(shaper.push(e, &tx).await);
        }
        // Second turn is the human one: held, linted, rewrite asked (no
        // session here, so the draft is delivered).
        assert_eq!(
            drained(&mut rx),
            [
                "Silent { reason: Some(MaxHops), text: \"\" }",
                "done",
                "start",
                "text:BAD one",
                "done"
            ]
        );
    }

    #[tokio::test]
    async fn dropping_the_session_closes_the_caller_channel() {
        let (_dir, shape) = setup("");
        let (s, mut rx, _) = open(Some(shape), vec![]).await;
        drop(s);
        let closed = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;
        assert!(
            matches!(closed, Ok(None)),
            "channel stayed open: {closed:?}"
        );
    }
}
