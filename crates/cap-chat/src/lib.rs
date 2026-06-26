//! Chat capability contract: interactive operator chat sessions backed by an
//! agent CLI (pi, codex, ...). Backends register `dyn ChatBackend` in the
//! typed service registry under their runner id (e.g. `"pi"`); the foreground
//! resolves one and drives turns through `ChatSession`.

use std::future::Future;
use std::path::{Path, PathBuf};

pub use cap_runner::HostToolBridge;

/// Backend id used when no configured/followed backend is available.
pub const CHAT_FALLBACK_BACKEND: &str = "pi";

pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatRole {
    Assistant,
    Thinking,
}

#[derive(Clone, Debug)]
pub enum ChatEvent {
    /// Streamed text; UI appends to current block of this role (new block on role change).
    Delta { role: ChatRole, text: String },
    /// Completed tool call (name + rendered args).
    ToolCall {
        id: String,
        name: String,
        args: String,
    },
    /// Tool output keyed by id. `text` REPLACES prior output for the same id
    /// (pi streams accumulated partialResult).
    ToolOutput {
        id: String,
        text: String,
        is_error: bool,
        done: bool,
    },
    /// Backend-side error line (stderr, protocol error).
    Error(String),
    /// Best-effort context-usage report for the status line. `tokens_used`
    /// is the prompt+response tokens the last turn occupied; `context_window`
    /// is the model's window when the backend can resolve it, else `None`
    /// (the UI then shows the raw token count without a percentage). Backends
    /// that cannot report usage simply never emit this.
    ContextUsage {
        tokens_used: u64,
        context_window: Option<u64>,
    },
    /// Exactly one per turn. aborted turns: ok=false, error=Some("aborted").
    TurnFinished { ok: bool, error: Option<String> },
    /// Backend process died outside a clean close; session is unusable.
    SessionClosed { error: Option<String> },
}

#[non_exhaustive]
pub struct ChatSessionParams {
    /// Backend binary override; `""` -> backend default binary.
    pub command: String,
    /// Child cwd.
    pub agent_root: PathBuf,
    /// Persistence home, caller-owned & pre-created.
    pub session_dir: PathBuf,
    pub model: Option<String>,
    pub provider: Option<String>,
    /// Assembled cross-surface system context (the agent's identity files).
    /// When set and non-empty, the backend delivers it as the session's
    /// initial system context to the child; `None`/empty leaves the child's
    /// own default system prompt untouched (chat opens exactly as before).
    pub system_prompt: Option<String>,
    /// When set, the chat backend advertises the host MCP bridge to its agent
    /// CLI so the operator's chat session sees the same host-registered
    /// registry tools an issue worker does. Carries the command + args the
    /// backend spawns to reach the bridge (the host binary's
    /// `__mcp-bridge --dir <agent>` invocation).
    pub host_tool_bridge: Option<HostToolBridge>,
}

impl ChatSessionParams {
    pub fn builder(
        command: &str,
        agent_root: &Path,
        session_dir: &Path,
    ) -> ChatSessionParamsBuilder {
        ChatSessionParamsBuilder {
            command: command.to_string(),
            agent_root: agent_root.to_path_buf(),
            session_dir: session_dir.to_path_buf(),
            model: None,
            provider: None,
            system_prompt: None,
            host_tool_bridge: None,
        }
    }
}

pub struct ChatSessionParamsBuilder {
    command: String,
    agent_root: PathBuf,
    session_dir: PathBuf,
    model: Option<String>,
    provider: Option<String>,
    system_prompt: Option<String>,
    host_tool_bridge: Option<HostToolBridge>,
}

impl ChatSessionParamsBuilder {
    pub fn model(mut self, value: Option<String>) -> Self {
        self.model = value;
        self
    }

    pub fn provider(mut self, value: Option<String>) -> Self {
        self.provider = value;
        self
    }

    pub fn system_prompt(mut self, value: Option<String>) -> Self {
        self.system_prompt = value;
        self
    }

    pub fn host_tool_bridge(mut self, value: Option<HostToolBridge>) -> Self {
        self.host_tool_bridge = value;
        self
    }

    pub fn build(self) -> ChatSessionParams {
        ChatSessionParams {
            command: self.command,
            agent_root: self.agent_root,
            session_dir: self.session_dir,
            model: self.model,
            provider: self.provider,
            system_prompt: self.system_prompt,
            host_tool_bridge: self.host_tool_bridge,
        }
    }
}

pub trait ChatBackend: Send + Sync {
    fn open<'a>(
        &'a self,
        params: ChatSessionParams,
        tx: tokio::sync::mpsc::Sender<ChatEvent>,
    ) -> BoxFuture<'a, anyhow::Result<Box<dyn ChatSession>>>;
}

pub trait ChatSession: Send {
    /// Accept a user message. This must also accept messages while a turn is
    /// already in flight: backends either inject immediately or queue until
    /// the next turn boundary. Completion arrives as one
    /// `ChatEvent::TurnFinished` per accepted message on tx. If `abort`
    /// cancels backend-held queued messages, those accepted messages finish
    /// as aborted.
    fn send_turn(&mut self, prompt: String) -> BoxFuture<'_, anyhow::Result<()>>;
    /// Graceful cancel of the in-flight turn. Session stays usable; queued
    /// accepted messages that cannot survive the abort finish as aborted.
    fn abort(&mut self) -> BoxFuture<'_, anyhow::Result<()>>;
    /// Close stdin, wait briefly, term-then-kill the process group on overrun.
    fn close(self: Box<Self>) -> BoxFuture<'static, anyhow::Result<()>>;
}
