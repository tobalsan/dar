use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use tokio::sync::oneshot;

pub const AGENT_ISSUE_IDENTIFIER: &str = "AGENT_ISSUE_IDENTIFIER";
pub const AGENT_ISSUE_ID: &str = "AGENT_ISSUE_ID";
pub const AGENT_RUN_ID: &str = "AGENT_RUN_ID";
pub const AGENT_PROJECT_ID: &str = "AGENT_PROJECT_ID";
pub const AGENT_WORKSPACE: &str = "AGENT_WORKSPACE";
pub const AGENT_WORKSPACE_ROOT: &str = "AGENT_WORKSPACE_ROOT";
pub const AGENT_PROMPT: &str = "AGENT_PROMPT";
pub const AGENT_WORKER_PROMPT: &str = "AGENT_WORKER_PROMPT";
pub const AGENT_MODEL: &str = "AGENT_MODEL";
pub const AGENT_WORKER_MODEL: &str = "AGENT_WORKER_MODEL";
pub const AGENT_LINEAR_GRAPHQL_TOOL: &str = "AGENT_LINEAR_GRAPHQL_TOOL";
pub const AGENT_SESSION_DIR: &str = "AGENT_SESSION_DIR";

/// How an attempt finished, from the orchestrator's point of view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExitKind {
    /// Process exited with code 0.
    Normal,
    /// Non-zero exit, killed by signal, or timed out. Carries the OS exit code
    /// when the process exited on its own (non-zero status), or `None` when
    /// killed by signal, timed out, or the wait call failed.
    Abnormal(Option<i32>),
    /// The runner was interrupted by an orchestrator-level condition.
    Interrupted { reason: &'static str },
}

/// Why the orchestrator asked to kill a running child.
pub enum KillReason {
    Timeout,
    OperatorStop,
    Reconcile,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NormalizedOutputEvent {
    pub stream: String,
    pub row_type: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LogRow {
    pub row_type: String,
    pub text: String,
}

/// Parameters for spawning one agent run.
#[non_exhaustive]
pub struct SpawnParams<'a> {
    pub command: &'a str,
    /// Runner kind (e.g. "pi", "claude"). Selects which backend is used.
    pub runner_kind: &'a str,
    /// Model override; passed to runners that support a model flag.
    pub model: Option<String>,
    /// Model provider override (e.g. "openai", "anthropic"); forwarded to pi runner as `--provider`.
    pub provider: Option<String>,
    /// Thinking/reasoning budget; forwarded to pi runner as `--thinking`.
    pub thinking: Option<String>,
    /// Reasoning effort level (e.g. "low", "medium", "high"); forwarded to codex runner.
    pub effort: Option<String>,
    pub workspace: &'a Path,
    pub workspace_root: &'a Path,
    pub agent_root: &'a Path,
    pub prompt: String,
    pub issue_id: String,
    /// SQLite run_id for this dispatch attempt. Used to tag event rows.
    pub run_id: String,
    pub max_run_timeout_ms: u64,
    /// Expose the optional Linear GraphQL worker tool to compatible protocol runners.
    pub expose_linear_graphql_tool: bool,
    pub events: Arc<dyn RunnerEventSink>,
    pub store: Arc<dyn RunnerEventStore>,
    pub last_event_at: Arc<Mutex<DateTime<Utc>>>,
}

pub struct SpawnParamsBuilder<'a> {
    command: &'a str,
    runner_kind: &'a str,
    workspace: &'a Path,
    workspace_root: &'a Path,
    agent_root: &'a Path,
    prompt: String,
    issue_id: String,
    run_id: String,
    max_run_timeout_ms: u64,
    events: Arc<dyn RunnerEventSink>,
    store: Arc<dyn RunnerEventStore>,
    last_event_at: Arc<Mutex<DateTime<Utc>>>,
    model: Option<String>,
    provider: Option<String>,
    thinking: Option<String>,
    effort: Option<String>,
    expose_linear_graphql_tool: bool,
}

impl<'a> SpawnParams<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn builder(
        command: &'a str,
        runner_kind: &'a str,
        workspace: &'a Path,
        workspace_root: &'a Path,
        agent_root: &'a Path,
        prompt: String,
        issue_id: String,
        run_id: String,
        max_run_timeout_ms: u64,
        events: Arc<dyn RunnerEventSink>,
        store: Arc<dyn RunnerEventStore>,
        last_event_at: Arc<Mutex<DateTime<Utc>>>,
    ) -> SpawnParamsBuilder<'a> {
        SpawnParamsBuilder {
            command,
            runner_kind,
            workspace,
            workspace_root,
            agent_root,
            prompt,
            issue_id,
            run_id,
            max_run_timeout_ms,
            events,
            store,
            last_event_at,
            model: None,
            provider: None,
            thinking: None,
            effort: None,
            expose_linear_graphql_tool: false,
        }
    }
}

impl<'a> SpawnParamsBuilder<'a> {
    pub fn model(mut self, value: Option<String>) -> Self {
        self.model = value;
        self
    }

    pub fn provider(mut self, value: Option<String>) -> Self {
        self.provider = value;
        self
    }

    pub fn thinking(mut self, value: Option<String>) -> Self {
        self.thinking = value;
        self
    }

    pub fn effort(mut self, value: Option<String>) -> Self {
        self.effort = value;
        self
    }

    pub fn expose_linear_graphql_tool(mut self, value: bool) -> Self {
        self.expose_linear_graphql_tool = value;
        self
    }

    pub fn build(self) -> SpawnParams<'a> {
        SpawnParams {
            command: self.command,
            runner_kind: self.runner_kind,
            model: self.model,
            provider: self.provider,
            thinking: self.thinking,
            effort: self.effort,
            workspace: self.workspace,
            workspace_root: self.workspace_root,
            agent_root: self.agent_root,
            prompt: self.prompt,
            issue_id: self.issue_id,
            run_id: self.run_id,
            max_run_timeout_ms: self.max_run_timeout_ms,
            expose_linear_graphql_tool: self.expose_linear_graphql_tool,
            events: self.events,
            store: self.store,
            last_event_at: self.last_event_at,
        }
    }
}

pub trait RunnerEventSink: Send + Sync {
    fn push(&self, line: String);
}

pub trait RunnerEventStore: Send + Sync {
    fn insert_event(
        &self,
        run_id: Option<&str>,
        issue_identifier: &str,
        kind: &'static str,
        payload: &str,
        ts: DateTime<Utc>,
    );
}

pub trait Runner: Send + Sync {
    fn spawn<'a>(
        &self,
        params: SpawnParams<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<RunnerHandle>> + Send + 'a>,
    >;
}

/// Handle to a running child. Owns the kill channel and the supervising task's
/// join handle. `wait` / `request_kill` consume the handle; the orchestrator
/// stores it via `Option::take`.
pub struct RunnerHandle {
    pub pid: u32,
    kill_tx: oneshot::Sender<KillReason>,
    done: tokio::task::JoinHandle<ExitKind>,
}

impl RunnerHandle {
    pub fn new(
        pid: u32,
        kill_tx: oneshot::Sender<KillReason>,
        done: tokio::task::JoinHandle<ExitKind>,
    ) -> Self {
        Self { pid, kill_tx, done }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Non-consuming completion check so the orchestrator can poll a stored
    /// handle each tick, then `take()` + `wait()` to collect the `ExitKind`.
    pub fn is_finished(&self) -> bool {
        self.done.is_finished()
    }

    /// Await the run to completion and return its classified exit.
    pub async fn wait(self) -> ExitKind {
        self.done.await.unwrap_or(ExitKind::Abnormal(None))
    }

    /// Ask the supervising task to terminate the child for the given reason.
    pub fn request_kill(self, why: KillReason) {
        let _ = self.kill_tx.send(why);
        drop(self.done);
    }

    pub async fn request_kill_and_wait(self, why: KillReason) -> ExitKind {
        let Self { kill_tx, done, .. } = self;
        let _ = kill_tx.send(why);
        done.await.unwrap_or(ExitKind::Abnormal(None))
    }
}
