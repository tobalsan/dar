//! chat-shape adapter: short-turn replies for interactive human chat.
//!
//! Off unless `agent.yaml` has `extensions.chat-shape`. When on, the stream
//! holds assistant text until the turn ends, lints it with the chat-shape
//! core CLI (`node <core>/dist/cli.js lint`), and either delivers it or asks
//! the agent for one rewrite. Rules, nudge, and the `reply_mode` tool live in
//! the Pi extension (`pi_extension`). Lint failures open: the reply is sent
//! unchanged.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::mpsc::Sender;

use crate::{parse_compact_command, BoxFuture, ChatEvent, ChatRole, ChatSession, TurnOrigin};

const CLI_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_CORE: &str = "~/code/agentropy/chat-shape";

/// Resolved `extensions.chat-shape` block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatShape {
    pub pi_extension: PathBuf,
    core: PathBuf,
    log: PathBuf,
    topics_dir: PathBuf,
    agent: String,
}

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

impl ChatShape {
    /// `None` when the block is absent, unreadable, or `enabled: false`.
    pub fn load(root: &Path) -> Option<Self> {
        let yaml = std::fs::read_to_string(root.join("agent.yaml")).ok()?;
        let doc: Value = serde_yaml::from_str(&yaml).ok()?;
        let block = doc.get("extensions")?.get("chat-shape")?;
        let block: Block = if block.is_null() {
            Block::default()
        } else {
            serde_json::from_value(block.clone()).ok()?
        };
        if block.enabled == Some(false) {
            return None;
        }
        let core = expand(block.core.as_deref().unwrap_or(DEFAULT_CORE), root);
        Some(Self {
            pi_extension: block
                .pi_extension
                .map_or_else(|| core.join("pi/extension.ts"), |p| expand(&p, root)),
            log: block.log.map_or_else(
                || root.join("data/chat-shape/long-mode.jsonl"),
                |p| expand(&p, root),
            ),
            topics_dir: root.join("topics"),
            agent: root
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
            core,
        })
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
            tracing::warn!("chat-shape: cannot write log: {e}");
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
/// One entry per accepted turn: `true` = interactive human turn (shaped).
type Queue = Arc<Mutex<VecDeque<bool>>>;

/// Session wrapper: tags each turn human/not, answers `/topics` locally.
pub(crate) struct ShapedSession {
    inner: Inner,
    queue: Queue,
    shape: ChatShape,
    out: tokio::sync::mpsc::WeakSender<ChatEvent>,
}

impl ChatSession for ShapedSession {
    fn send_turn(&mut self, prompt: String) -> BoxFuture<'_, anyhow::Result<()>> {
        self.send_turn_from(prompt, None)
    }

    fn send_turn_from(
        &mut self,
        prompt: String,
        sender: Option<crate::AgentSender>,
    ) -> BoxFuture<'_, anyhow::Result<()>> {
        let human = sender.is_none() && parse_compact_command(&prompt).is_none();
        let topics = sender.is_none() && prompt.trim() == "/topics";
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
            queue.lock().unwrap().push_back(human);
            let mut guard = inner.lock().await;
            let session = guard
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("chat session closed"))?;
            let result = session.send_turn_from(prompt, sender).await;
            if result.is_err() {
                queue.lock().unwrap().pop_back();
            }
            result
        })
    }

    fn abort(&mut self) -> BoxFuture<'_, anyhow::Result<()>> {
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

/// Event-stream stage: holds human-turn text, lints at turn end.
pub(crate) struct Shaper {
    shape: ChatShape,
    inner: Inner,
    queue: Queue,
    session: String,
    /// Current run: `Some(true)` = shaped human turn.
    current: Option<bool>,
    /// Assistant text segments, split at tool boundaries.
    segs: Vec<String>,
    seg_open: bool,
    decl: Option<(String, String)>,
    rewriting: bool,
}

impl Shaper {
    fn shaped(&mut self) -> bool {
        let queue = &self.queue;
        *self
            .current
            .get_or_insert_with(|| queue.lock().unwrap().pop_front().unwrap_or(false))
    }

    fn reset(&mut self) {
        self.current = None;
        self.segs.clear();
        self.seg_open = false;
        self.decl = None;
        self.rewriting = false;
    }

    async fn emit_held(&mut self, tx: &Sender<ChatEvent>) -> bool {
        if self.segs.is_empty() {
            return true;
        }
        let text = std::mem::take(&mut self.segs).join("\n\n");
        self.seg_open = false;
        tx.send(ChatEvent::Delta {
            role: ChatRole::Assistant,
            text,
        })
        .await
        .is_ok()
    }

    /// Process one event; `false` when `tx` is closed.
    pub(crate) async fn push(&mut self, event: ChatEvent, tx: &Sender<ChatEvent>) -> bool {
        match event {
            ChatEvent::SessionIdentity { ref id } => self.session = id.clone(),
            ChatEvent::TurnStarted { origin } => {
                if self.rewriting {
                    return true;
                }
                self.current = Some(origin == TurnOrigin::Submitted && self.shaped_pop());
            }
            ChatEvent::Delta {
                role: ChatRole::Assistant,
                text,
            } if self.shaped() => {
                if !self.seg_open {
                    self.segs.push(String::new());
                    self.seg_open = true;
                }
                self.segs.last_mut().expect("segment").push_str(&text);
                return true;
            }
            ChatEvent::ToolCall {
                ref name, ref args, ..
            } => {
                self.seg_open = false;
                if name == "reply_mode" && self.shaped() {
                    if let Ok(v) = serde_json::from_str::<Value>(args) {
                        if v["mode"] == "long" {
                            let field = |k: &str| v[k].as_str().unwrap_or_default().to_string();
                            self.decl = Some((field("type"), field("note")));
                        }
                    }
                }
            }
            ChatEvent::ToolOutput { .. } => self.seg_open = false,
            ChatEvent::Silent { .. } => self.segs.clear(),
            ChatEvent::Error(_) | ChatEvent::SessionClosed { .. } => {
                if !self.emit_held(tx).await {
                    return false;
                }
            }
            ChatEvent::TurnFinished { ok, .. } => return self.finish(event, ok, tx).await,
            _ => {}
        }
        tx.send(event).await.is_ok()
    }

    fn shaped_pop(&mut self) -> bool {
        self.queue.lock().unwrap().pop_front().unwrap_or(false)
    }

    async fn finish(&mut self, event: ChatEvent, ok: bool, tx: &Sender<ChatEvent>) -> bool {
        let text = self.segs.join("\n\n");
        if !self.shaped() || !ok || text.trim().is_empty() {
            let sent = self.emit_held(tx).await && tx.send(event).await.is_ok();
            self.reset();
            return sent;
        }
        let Some(lint) = self
            .shape
            .lint(&text, self.decl.as_ref(), &self.session)
            .await
        else {
            tracing::warn!("chat-shape: lint failed; sending reply unchanged");
            let sent = self.emit_held(tx).await && tx.send(event).await.is_ok();
            self.reset();
            return sent;
        };
        if !lint.ok && !self.rewriting {
            self.segs.clear();
            self.seg_open = false;
            let hint = format!("[chat-shape] {}", lint.hint);
            let sent = match self.inner.lock().await.as_mut() {
                Some(s) => s.send_turn(hint).await.is_ok(),
                None => false,
            };
            if sent {
                self.rewriting = true;
                return true;
            }
            // Rewrite impossible: deliver the draft.
            self.segs.push(text);
        } else if !lint.ok {
            self.shape.log_rewrite_failed(&self.session, &lint.rules);
        }
        let sent = self.emit_held(tx).await && tx.send(event).await.is_ok();
        self.reset();
        sent
    }
}

/// Shared state between the stream stage and the session wrapper.
pub(crate) struct Handles {
    shape: ChatShape,
    inner: Inner,
    queue: Queue,
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
        })
    }
}

impl Shaper {
    pub(crate) fn new(shape: ChatShape) -> (Self, Handles) {
        let inner = Inner::default();
        let queue = Queue::default();
        let shaper = Self {
            shape: shape.clone(),
            inner: inner.clone(),
            queue: queue.clone(),
            session: String::new(),
            current: None,
            segs: Vec::new(),
            seg_open: false,
            decl: None,
            rewriting: false,
        };
        (
            shaper,
            Handles {
                shape,
                inner,
                queue,
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
  const bad = t.includes("BAD"), long = a.includes("--mode");
  const log = a[a.indexOf("--log") + 1];
  if (log && long) fs.appendFileSync(log, JSON.stringify({ type: a[a.indexOf("--type") + 1] }) + "\n");
  console.log(JSON.stringify({ ok: !bad || long, hint: "HINT", violations: bad ? [{ rule: "too_many_sentences" }] : [] }));
  process.exit(bad && !long ? 1 : 0);
});
"#;

    fn setup(yaml: &str) -> (tempfile::TempDir, ChatShape) {
        let dir = tempfile::tempdir().unwrap();
        let core = dir.path().join("core");
        std::fs::create_dir_all(core.join("dist")).unwrap();
        std::fs::write(core.join("dist/cli.js"), STUB).unwrap();
        std::fs::write(
            dir.path().join("agent.yaml"),
            format!(
                "extensions:\n  chat-shape:\n    core: {}\n{yaml}",
                core.display()
            ),
        )
        .unwrap();
        let shape = ChatShape::load(dir.path()).unwrap();
        (dir, shape)
    }

    fn text(s: &str) -> ChatEvent {
        ChatEvent::Delta {
            role: ChatRole::Assistant,
            text: s.into(),
        }
    }

    fn done() -> ChatEvent {
        ChatEvent::TurnFinished {
            ok: true,
            error: None,
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
                tx.send(ChatEvent::TurnStarted {
                    origin: TurnOrigin::Submitted,
                })
                .await?;
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

    async fn open(shape: Option<ChatShape>, script: Vec<Vec<ChatEvent>>) -> Opened {
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

    /// Events of one turn as short strings.
    async fn turn(rx: &mut mpsc::Receiver<ChatEvent>) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(e) = rx.recv().await {
            out.push(match &e {
                ChatEvent::Delta { text, .. } => format!("text:{text}"),
                ChatEvent::TurnFinished { .. } => "done".into(),
                ChatEvent::TurnStarted { .. } => "start".into(),
                ChatEvent::ToolCall { name, .. } => format!("tool:{name}"),
                other => format!("{other:?}"),
            });
            if matches!(e, ChatEvent::TurnFinished { .. }) {
                break;
            }
        }
        out
    }

    #[test]
    fn config_switch() {
        let dir = tempfile::tempdir().unwrap();
        let load = |yaml: &str| {
            std::fs::write(dir.path().join("agent.yaml"), yaml).unwrap();
            ChatShape::load(dir.path())
        };
        assert!(load("runner: {}\n").is_none());
        assert!(load("extensions:\n  other: {}\n").is_none());
        assert!(load("extensions:\n  chat-shape: {}\n").is_some());
        assert!(load("extensions:\n  chat-shape:\n").is_some());
        assert!(load("extensions:\n  chat-shape:\n    enabled: false\n").is_none());
        let on = load("extensions:\n  chat-shape:\n    pi_extension: /x/e.ts\n").unwrap();
        assert_eq!(on.pi_extension, Path::new("/x/e.ts"));
        assert_eq!(on.log, dir.path().join("data/chat-shape/long-mode.jsonl"));
    }

    #[tokio::test]
    async fn off_streams_deltas_before_turn_end() {
        let (mut s, mut rx, _) =
            open(None, vec![vec![text("BAD one"), text(" two"), done()]]).await;
        s.send_turn("hi".into()).await.unwrap();
        assert_eq!(
            turn(&mut rx).await,
            ["start", "text:BAD one", "text: two", "done"]
        );
    }

    #[tokio::test]
    async fn on_holds_text_then_passes() {
        let (_dir, shape) = setup("");
        let (mut s, mut rx, _) =
            open(Some(shape), vec![vec![text("Hi "), text("there."), done()]]).await;
        s.send_turn("hi".into()).await.unwrap();
        assert_eq!(turn(&mut rx).await, ["start", "text:Hi there.", "done"]);
    }

    #[tokio::test]
    async fn fail_sends_one_hint_and_delivers_rewrite() {
        let (_dir, shape) = setup("");
        let script = vec![
            vec![text("BAD draft"), done()],
            vec![text("Short."), done()],
        ];
        let (mut s, mut rx, prompts) = open(Some(shape), script).await;
        s.send_turn("hi".into()).await.unwrap();
        assert_eq!(turn(&mut rx).await, ["start", "text:Short.", "done"]);
        assert_eq!(*prompts.lock().unwrap(), ["hi", "[chat-shape] HINT"]);
    }

    #[tokio::test]
    async fn failed_rewrite_is_sent_and_logged() {
        let (dir, shape) = setup("");
        let script = vec![vec![text("BAD one"), done()], vec![text("BAD two"), done()]];
        let (mut s, mut rx, prompts) = open(Some(shape), script).await;
        s.send_turn("hi".into()).await.unwrap();
        assert_eq!(turn(&mut rx).await, ["start", "text:BAD two", "done"]);
        assert_eq!(prompts.lock().unwrap().len(), 2);
        let log =
            std::fs::read_to_string(dir.path().join("data/chat-shape/long-mode.jsonl")).unwrap();
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
        s.send_turn("hi".into()).await.unwrap();
        assert_eq!(
            turn(&mut rx).await,
            ["start", "tool:reply_mode", "text:BAD long", "done"]
        );
        assert_eq!(prompts.lock().unwrap().len(), 1);
        let log =
            std::fs::read_to_string(dir.path().join("data/chat-shape/long-mode.jsonl")).unwrap();
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
        assert_eq!(prompts.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn autonomous_run_is_not_shaped() {
        let (dir, shape) = setup("");
        let (mut shaper, _handles) = Shaper::new(shape);
        let (tx, mut rx) = mpsc::channel(8);
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
        s.send_turn(" /topics ".into()).await.unwrap();
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
        s.send_turn("hi".into()).await.unwrap();
        assert_eq!(turn(&mut rx).await, ["start", "text:BAD", "done"]);
        assert_eq!(prompts.lock().unwrap().len(), 1);
    }
}
