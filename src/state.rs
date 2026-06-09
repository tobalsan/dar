//! Shared async `AppState` the dashboard reads and the orchestrator/runner
//! write. Holds the run-status enum, the active-run snapshot, the queue and
//! retry snapshots, the pause flag, and the recent-events ring.
//!
//! Single-writer discipline: the dashboard only SENDS `ControlMsg` over
//! `control_tx`; the orchestrator is the sole mutator of run state (including
//! the `paused` flag). Dashboard handlers take read locks for rendering.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use tokio::sync::{mpsc, RwLock};

/// Run state: the orchestrator's in-memory view of one dispatch attempt.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum RunStatus {
    Running,
    RetryQueued,
    Cancelled,
    Failed,
    Succeeded,
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

/// Fixed-capacity ring of finished runs, newest-first. Optionally persisted to a
/// JSONL file (one entry per line, appended on push, loaded on startup).
pub struct HistoryRing {
    inner: Mutex<VecDeque<HistoryEntry>>,
    path: Option<PathBuf>,
}

impl HistoryRing {
    const CAP: usize = 50;

    pub fn new() -> Self {
        Self {
            inner: Mutex::new(VecDeque::with_capacity(Self::CAP)),
            path: None,
        }
    }

    /// Open a ring backed by `path`, loading any existing entries (newest-first,
    /// capped). Subsequent `push`es append to the file. A missing file is fine
    /// (starts empty); malformed lines are skipped.
    pub fn with_persistence(path: PathBuf) -> Self {
        let mut entries: VecDeque<HistoryEntry> = VecDeque::with_capacity(Self::CAP);
        if let Ok(contents) = std::fs::read_to_string(&path) {
            // File is append-order (oldest first). Parse all valid lines, then
            // take the last CAP and reverse into newest-first order.
            let parsed: Vec<HistoryEntry> = contents
                .lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect();
            let start = parsed.len().saturating_sub(Self::CAP);
            for e in parsed[start..].iter().rev().cloned() {
                entries.push_back(e);
            }
        }
        Self {
            inner: Mutex::new(entries),
            path: Some(path),
        }
    }

    /// Record a finished run at the front; evict the oldest if full. If the ring
    /// is persisted, append the entry as one JSON line to the backing file.
    pub fn push(&self, entry: HistoryEntry) {
        if let Some(path) = &self.path {
            if let Err(e) = Self::append_line(path, &entry) {
                tracing::warn!("history persist failed: {e:#}");
            }
        }
        let mut q = self.inner.lock().unwrap();
        if q.len() == Self::CAP {
            q.pop_back();
        }
        q.push_front(entry);
    }

    /// Append one `HistoryEntry` as a JSON line to `path`, creating it if needed.
    fn append_line(path: &std::path::Path, entry: &HistoryEntry) -> std::io::Result<()> {
        use std::io::Write;
        let line = serde_json::to_string(entry)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(f, "{line}")
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
    /// Dashboard -> orchestrator control channel.
    pub control_tx: mpsc::UnboundedSender<ControlMsg>,
}

impl AppState {
    pub fn new(
        agent: AgentInfo,
        control_tx: mpsc::UnboundedSender<ControlMsg>,
        history_path: std::path::PathBuf,
    ) -> Self {
        Self {
            agent,
            paused: Arc::new(AtomicBool::new(false)),
            active: Arc::new(RwLock::new(None)),
            queue: Arc::new(RwLock::new(Vec::new())),
            retry: Arc::new(RwLock::new(Vec::new())),
            events: Arc::new(EventRing::new()),
            history: Arc::new(HistoryRing::with_persistence(history_path)),
            control_tx,
        }
    }
}
