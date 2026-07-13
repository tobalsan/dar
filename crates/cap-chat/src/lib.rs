//! Chat capability contract: interactive operator chat sessions backed by an
//! agent CLI (pi, codex, ...). Backends register `dyn ChatBackend` in the
//! typed service registry under their runner id (e.g. `"pi"`); the foreground
//! resolves one and drives turns through `ChatSession`.

use std::future::Future;
use std::path::{Path, PathBuf};

pub use cap_runner::HostToolBridge;
pub use dar_artifacts::ArtifactId;
use dar_artifacts::{ArtifactMetadata, ArtifactStore};
use tokio::sync::mpsc::Sender;

/// Backend id used when no configured/followed backend is available.
pub const CHAT_FALLBACK_BACKEND: &str = "pi";

/// Safe artifact metadata delivered out-of-band by a chat backend. Never
/// contains local paths or untrusted assistant/tool text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactReady {
    pub id: ArtifactId,
    pub uri: String,
    pub name: String,
    pub mime_type: Option<String>,
    pub bytes: u64,
    pub sha256: String,
    pub caption: Option<String>,
}

impl ArtifactReady {
    /// Parse only exact successful `artifact.publish` resource-link output.
    /// All assistant text, raw tool text, malformed links, and other tools fail
    /// closed as `None`.
    /// Return canonical vault metadata only when every link field matches.
    /// Delivery surfaces must use returned metadata, never this untrusted link.
    pub fn validate(&self, store: &ArtifactStore) -> Option<ArtifactMetadata> {
        store
            .validate_resource_link(
                self.id,
                &self.name,
                self.mime_type.as_deref(),
                self.bytes,
                &self.sha256,
                self.caption.as_deref(),
            )
            .ok()
    }

    pub fn from_publish_resource(tool_name: &str, value: &serde_json::Value) -> Option<Self> {
        if tool_name != "artifact.publish" || value.get("type")?.as_str()? != "resource_link" {
            return None;
        }
        let uri = value.get("uri")?.as_str()?.to_string();
        let id = uri.strip_prefix("dar-artifact://")?.parse().ok()?;
        Some(Self {
            id,
            uri,
            name: value.get("name")?.as_str()?.to_string(),
            mime_type: value
                .get("mime_type")
                .or_else(|| value.get("mimeType"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            bytes: value.get("bytes")?.as_u64()?,
            sha256: value.get("sha256")?.as_str()?.to_string(),
            caption: value
                .get("caption")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
        })
    }
}

#[cfg(test)]
mod artifact_tests {
    use super::*;
    use dar_artifacts::{ArtifactMetadataInput, ExportRoot};

    #[test]
    fn artifact_ready_requires_exact_vault_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let exports = dir.path().join("exports");
        std::fs::create_dir(&exports).unwrap();
        std::fs::write(exports.join("report.txt"), "hello").unwrap();
        let store = ArtifactStore::open(dir.path().join("vault"), 1024).unwrap();
        let metadata = store
            .stage_from_export_root(
                &ExportRoot::open(&exports).unwrap(),
                "report.txt",
                ArtifactMetadataInput {
                    filename: "report.txt".to_string(),
                    media_type: Some("text/plain".to_string()),
                    caption: Some("report".to_string()),
                },
            )
            .unwrap();
        let ready = ArtifactReady {
            id: metadata.id,
            uri: format!("dar-artifact://{}", metadata.id),
            name: "report.txt".to_string(),
            mime_type: Some("text/plain".to_string()),
            bytes: 5,
            sha256: metadata.sha256_hex(),
            caption: Some("report".to_string()),
        };
        assert_eq!(ready.validate(&store), Some(metadata));

        let forged = ArtifactReady {
            name: "forged.txt".to_string(),
            ..ready
        };
        assert_eq!(forged.validate(&store), None);
    }
}

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
    /// When set, the backend resumes this prior session id instead of opening
    /// a fresh one (e.g. `pi --resume <id>`). `None` opens fresh exactly as
    /// before. The caller resolves the id (newest archived session) and is
    /// responsible for honoring the fall-back-to-fresh contract when no prior
    /// session is resolvable.
    pub resume_session_id: Option<String>,
    /// Optional side-channel for exact `artifact.publish` resource links.
    pub artifact_ready: Option<Sender<ArtifactReady>>,
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
            resume_session_id: None,
            artifact_ready: None,
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
    resume_session_id: Option<String>,
    artifact_ready: Option<Sender<ArtifactReady>>,
}

impl ChatSessionParamsBuilder {
    /// Override the backend command. Defaults to whatever was passed to
    /// [`ChatSessionParams::builder`]; the shared SDK helper opens with an empty
    /// command and lets a surface that needs an explicit one set it here.
    pub fn command(mut self, value: impl Into<String>) -> Self {
        self.command = value.into();
        self
    }

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

    pub fn resume_session_id(mut self, value: Option<String>) -> Self {
        self.resume_session_id = value;
        self
    }

    pub fn artifact_ready(mut self, value: Option<Sender<ArtifactReady>>) -> Self {
        self.artifact_ready = value;
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
            resume_session_id: self.resume_session_id,
            artifact_ready: self.artifact_ready,
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
