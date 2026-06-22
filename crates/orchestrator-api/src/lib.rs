//! Public orchestration bus contract.
//!
//! This crate contains the payload shapes plus the [`RunQuery`] service trait
//! shared by the orchestrator extension and dashboard extension without either
//! importing the other's implementation.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

pub type ControlReplyTx = std::sync::Arc<std::sync::Mutex<Option<oneshot::Sender<ControlReply>>>>;

pub const RUN_SNAPSHOT_TOPIC: &str = "orchestrator.run-snapshot";
pub const CONTROL_TOPIC: &str = "orchestrator.control";
pub const RUN_REQUESTED_TOPIC: &str = "orchestrator.run-requested";
pub const DISPATCH_REQUESTED_TOPIC: &str = "orchestrator.dispatch-requested";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunStatus {
    Running,
    RetryQueued,
    Cancelled,
    Failed,
    Succeeded,
    Interrupted,
    Crashed,
    NeedsHuman,
    Stalled,
    Terminal,
    HookFailed,
    DispatchFailed,
    Released,
    Orphaned,
    ParkBarrier,
    Killed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub id: String,
    pub folder: String,
    pub tracker: String,
    pub runner: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveRun {
    pub run_id: String,
    pub identifier: String,
    pub state: String,
    pub workspace: String,
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    pub last_event: String,
    pub status: RunStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueItem {
    pub identifier: String,
    pub title: String,
    pub state: String,
    pub priority: Option<i32>,
    pub created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryItem {
    pub identifier: String,
    pub attempt: u32,
    pub due_at: DateTime<Utc>,
    pub last_error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub identifier: String,
    pub status: RunStatus,
    pub pid: u32,
    pub ended_at: DateTime<Utc>,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRow {
    pub run_id: String,
    pub issue_id: String,
    pub issue_identifier: String,
    pub workspace: String,
    pub profile_json: Option<String>,
    pub workflow_path: Option<String>,
    pub workflow_sha: Option<String>,
    pub pid: u32,
    pub worker_id: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub outcome: Option<String>,
    pub exit_code: Option<i32>,
    pub process_alive: bool,
    /// Runner kind this run was dispatched with (e.g. `pi`, `opencode`,
    /// `codex`, `fake`). Persisted per-run for historical fidelity.
    #[serde(default)]
    pub runner: Option<String>,
    /// Model this run was dispatched with, or `None` when unset in config.
    #[serde(default)]
    pub model: Option<String>,
    /// Model provider this run was dispatched with (multi-provider runners only).
    #[serde(default)]
    pub provider: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRow {
    pub event_id: i64,
    pub run_id: Option<String>,
    pub issue_identifier: String,
    pub kind: String,
    pub payload: String,
    pub ts: String,
}

/// Read-only query surface over persisted runs and their events, exposed by the
/// orchestrator as a named service (`"orchestrator"`) so the dashboard can
/// render a run-detail drawer without importing the orchestrator crate.
pub trait RunQuery: Send + Sync {
    /// Fetch one persisted run by id, or `None` if unknown / store not ready.
    fn run(&self, run_id: &str) -> Option<RunRow>;
    /// List events for `run_id` with `event_id > since`, ascending, up to `limit`.
    fn events_for_run(&self, run_id: &str, since: i64, limit: usize) -> Vec<EventRow>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiLogRow {
    pub event_id: i64,
    pub run_id: Option<String>,
    pub issue_identifier: String,
    pub kind: String,
    pub payload: String,
    pub ts: String,
    pub row_type: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryRow {
    pub status: String,
    pub status_class: String,
    pub identifier: String,
    pub run: String,
    pub run_id: String,
    pub age: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSnapshot {
    pub agent: AgentInfo,
    pub paused: bool,
    pub active: Option<ActiveRun>,
    pub active_runs: Vec<ActiveRun>,
    pub queue: Vec<QueueItem>,
    pub retry: Vec<RetryItem>,
    pub events: Vec<String>,
    pub history: Vec<HistoryEntry>,
    pub runs: Vec<RunRow>,
    pub last_tick_at: Option<DateTime<Utc>>,
    pub rate_limit_min_remaining: Option<i64>,
    pub version: u64,
}

impl RunSnapshot {
    pub fn empty() -> Self {
        Self {
            agent: AgentInfo {
                id: String::new(),
                folder: String::new(),
                tracker: String::new(),
                runner: String::new(),
                model: None,
                provider: None,
            },
            paused: false,
            active: None,
            active_runs: Vec::new(),
            queue: Vec::new(),
            retry: Vec::new(),
            events: Vec::new(),
            history: Vec::new(),
            runs: Vec::new(),
            last_tick_at: None,
            rate_limit_min_remaining: None,
            version: 0,
        }
    }
}

#[derive(Clone)]
pub enum ControlMsg {
    Stop,
    Pause,
    Resume,
    Tick {
        reply: ControlReplyTx,
    },
    Claim {
        identifier: String,
        reply: ControlReplyTx,
    },
    Release {
        run_id: String,
        reply: ControlReplyTx,
    },
    Interrupt {
        run_id: String,
        reply: ControlReplyTx,
    },
    Kill {
        run_id: String,
        reply: ControlReplyTx,
    },
}

pub fn reply_channel() -> (ControlReplyTx, oneshot::Receiver<ControlReply>) {
    let (tx, rx) = oneshot::channel();
    (std::sync::Arc::new(std::sync::Mutex::new(Some(tx))), rx)
}

pub fn send_reply(reply: &ControlReplyTx, value: ControlReply) {
    if let Some(tx) = reply.lock().expect("control reply mutex poisoned").take() {
        let _ = tx.send(value);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlReply {
    pub ok: bool,
    pub message: String,
}

impl ControlReply {
    pub fn ok(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: message.into(),
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRequested {
    pub identifier: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchRequested {
    pub identifier: String,
}
