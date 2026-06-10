//! Shared async `AppState` the dashboard reads and the orchestrator/runner
//! write. Holds the run-status enum, the active-run snapshot, the queue and
//! retry snapshots, the pause flag, and the recent-events ring.
//!
//! Single-writer discipline: the dashboard only SENDS `ControlMsg` over
//! `control_tx`; the orchestrator is the sole mutator of run state (including
//! the `paused` flag). Dashboard handlers take read locks for rendering.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use tokio::sync::{mpsc, RwLock};

use crate::store::Store;

/// Run state: the orchestrator's in-memory view of one dispatch attempt.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum RunStatus {
    Running,
    RetryQueued,
    Cancelled,
    Failed,
    Succeeded,
    /// Run was interrupted by an expected orchestrator condition.
    Interrupted,
    /// Run was in-progress when the gateway process exited without a clean
    /// shutdown; marked at startup via `Store::mark_crashed_runs`.
    Crashed,
    /// Issue was moved to the configured needs-human state; released without retry.
    NeedsHuman,
}

/// Snapshot of the currently-active run for the dashboard.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ActiveRun {
    pub identifier: String,
    pub state: String,
    pub workspace: String,
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    pub last_event: String,
    pub status: RunStatus,
}

/// One queued candidate, in dispatch order.
#[derive(Debug, Clone, serde::Serialize)]
pub struct QueueItem {
    pub identifier: String,
    pub title: String,
    pub state: String,
    pub priority: Option<i32>,
    pub created_at: Option<DateTime<Utc>>,
}

/// One pending retry.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RetryItem {
    pub identifier: String,
    pub attempt: u32,
    pub due_at: DateTime<Utc>,
    pub last_error: String,
}

/// One finished run, recorded for the dashboard history list (newest-first).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HistoryEntry {
    pub identifier: String,
    pub status: RunStatus,
    pub pid: u32,
    pub ended_at: DateTime<Utc>,
    pub note: String,
}

/// Fixed-capacity ring of finished runs, newest-first. The in-memory ring feeds
/// the dashboard; durable persistence is handled by `Store` (SQLite). Startup
/// seeds the ring from `Store::load_recent_runs` instead of a JSONL file.
pub struct HistoryRing {
    inner: Mutex<VecDeque<HistoryEntry>>,
}

impl HistoryRing {
    pub const CAP: usize = 50;

    /// Create an empty ring.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(VecDeque::with_capacity(Self::CAP)),
        }
    }

    /// Create a ring pre-seeded from the given entries (newest-first, capped).
    /// Used at startup after loading recent runs from SQLite.
    pub fn from_seed(seed: Vec<HistoryEntry>) -> Self {
        let mut q: VecDeque<HistoryEntry> = VecDeque::with_capacity(Self::CAP);
        for e in seed.into_iter().take(Self::CAP) {
            q.push_back(e);
        }
        Self {
            inner: Mutex::new(q),
        }
    }

    /// Record a finished run at the front; evict the oldest if full.
    /// Durable write to SQLite is the caller's responsibility (orchestrator).
    pub fn push(&self, entry: HistoryEntry) {
        let mut q = self.inner.lock().unwrap();
        if q.len() == Self::CAP {
            q.pop_back();
        }
        q.push_front(entry);
    }

    /// Newest-to-oldest copy of the current ring contents.
    pub fn snapshot(&self) -> Vec<HistoryEntry> {
        let q = self.inner.lock().unwrap();
        q.iter().cloned().collect()
    }
}

impl Default for HistoryRing {
    fn default() -> Self {
        Self::new()
    }
}

/// Static agent identity for the dashboard "Agent" section.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentInfo {
    pub id: String,
    pub folder: String,
    pub tracker: String,
    pub runner: String,
}

/// Fixed-capacity ring of recent event lines (child stdout/stderr + lifecycle).
pub struct EventRing {
    inner: Mutex<VecDeque<String>>,
}

impl EventRing {
    const CAP: usize = 50;

    pub fn new() -> Self {
        Self {
            inner: Mutex::new(VecDeque::with_capacity(Self::CAP)),
        }
    }

    /// Push a line, evicting the oldest if the ring is full.
    pub fn push(&self, line: String) {
        let mut q = self.inner.lock().unwrap();
        if q.len() == Self::CAP {
            q.pop_front();
        }
        q.push_back(line);
    }

    /// Oldest-to-newest copy of the current ring contents.
    pub fn snapshot(&self) -> Vec<String> {
        let q = self.inner.lock().unwrap();
        q.iter().cloned().collect()
    }
}

impl Default for EventRing {
    fn default() -> Self {
        Self::new()
    }
}

/// Control messages from the dashboard to the orchestrator. These mutate RUN
/// state only, never issue state.
pub enum ControlMsg {
    Stop,
    Pause,
    Resume,
}

/// Cheap-to-clone shared state (all fields are `Arc`). Held owned by the
/// orchestrator (which writes snapshots each tick) and cloned into the axum
/// dashboard (which reads).
#[derive(Clone)]
pub struct AppState {
    pub agent: AgentInfo,
    pub paused: Arc<AtomicBool>,
    pub active: Arc<RwLock<Option<ActiveRun>>>,
    pub queue: Arc<RwLock<Vec<QueueItem>>>,
    pub retry: Arc<RwLock<Vec<RetryItem>>>,
    pub events: Arc<EventRing>,
    pub history: Arc<HistoryRing>,
    /// SQLite persistence store (runs, events, claims, heartbeats).
    pub store: Arc<Store>,
    /// Dashboard -> orchestrator control channel.
    pub control_tx: mpsc::UnboundedSender<ControlMsg>,
    /// Minimum Linear rate-limit requests remaining observed since startup.
    /// `i64::MAX` = no observation yet (tracker doesn't emit rate-limit info).
    pub rate_limit_min_remaining: Arc<AtomicI64>,
}

impl AppState {
    /// Create shared state. `history_seed` is the recent run history loaded from
    /// SQLite at startup; it seeds the in-memory `HistoryRing`.
    pub fn new(
        agent: AgentInfo,
        control_tx: mpsc::UnboundedSender<ControlMsg>,
        store: Arc<Store>,
        history_seed: Vec<HistoryEntry>,
    ) -> Self {
        Self {
            agent,
            paused: Arc::new(AtomicBool::new(false)),
            active: Arc::new(RwLock::new(None)),
            queue: Arc::new(RwLock::new(Vec::new())),
            retry: Arc::new(RwLock::new(Vec::new())),
            events: Arc::new(EventRing::new()),
            history: Arc::new(HistoryRing::from_seed(history_seed)),
            store,
            control_tx,
            rate_limit_min_remaining: Arc::new(AtomicI64::new(i64::MAX)),
        }
    }
}
