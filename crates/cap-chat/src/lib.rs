//! Chat capability contract: interactive operator chat sessions backed by an
//! agent CLI (pi, claude, ...). Backends register `dyn ChatBackend` in the
//! typed service registry under their runner id (e.g. `"pi"`); the foreground
//! resolves one and drives turns through `ChatSession`.

use std::future::Future;
use std::path::{Path, PathBuf};

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
}

impl ChatSessionParams {
    pub fn builder(command: &str, agent_root: &Path, session_dir: &Path) -> ChatSessionParamsBuilder {
        ChatSessionParamsBuilder {
            command: command.to_string(),
            agent_root: agent_root.to_path_buf(),
            session_dir: session_dir.to_path_buf(),
            model: None,
        }
    }
}

pub struct ChatSessionParamsBuilder {
    command: String,
    agent_root: PathBuf,
    session_dir: PathBuf,
    model: Option<String>,
}

impl ChatSessionParamsBuilder {
    pub fn model(mut self, value: Option<String>) -> Self {
        self.model = value;
        self
    }

    pub fn build(self) -> ChatSessionParams {
        ChatSessionParams {
            command: self.command,
            agent_root: self.agent_root,
            session_dir: self.session_dir,
            model: self.model,
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
    /// One turn at a time (caller enforces). Returns once the turn is ACCEPTED;
    /// completion arrives as ChatEvent::TurnFinished on tx.
    fn send_turn(&mut self, prompt: String) -> BoxFuture<'_, anyhow::Result<()>>;
    /// Graceful cancel of the in-flight turn. Session stays usable.
    fn abort(&mut self) -> BoxFuture<'_, anyhow::Result<()>>;
    /// Close stdin, wait briefly, term-then-kill the process group on overrun.
    fn close(self: Box<Self>) -> BoxFuture<'static, anyhow::Result<()>>;
}
