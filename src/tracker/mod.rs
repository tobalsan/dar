//! Read-only tracker abstraction. The trait locks the read verb set (no write
//! surface in v0: the orchestrator never writes issue state). A factory selects
//! the only v0 implementation, `FileTracker`, based on config.

mod files;

use std::sync::Arc;

use anyhow::{bail, Result};

use crate::config::TrackerConfig;
use crate::domain::Issue;
use crate::paths::AgentPaths;

pub use files::FileTracker;

/// Read-only view over issues. Sync because fs reads are cheap; the orchestrator
/// may call these directly or under `spawn_blocking`.
pub trait Tracker: Send + Sync {
    /// All issues whose state is in `active_states`.
    fn poll_candidates(&self) -> Result<Vec<Issue>>;
    /// Current state of the given issue ids (by id or identifier). Missing ids
    /// are simply omitted from the result.
    fn fetch_states(&self, ids: &[String]) -> Result<Vec<Issue>>;
    /// All issues whose state is in `terminal_states`.
    fn fetch_terminal(&self) -> Result<Vec<Issue>>;
    /// One issue by id or identifier; `None` if not found.
    fn fetch_one(&self, id: &str) -> Result<Option<Issue>>;
}

/// Build the configured tracker. v0 only supports `use: files`.
pub fn build(cfg: &TrackerConfig, paths: &AgentPaths) -> Result<Arc<dyn Tracker>> {
    match cfg.use_.as_str() {
        "files" => {
            let issues_dir = paths.issues_dir(cfg);
            let tracker = FileTracker::new(
                issues_dir,
                cfg.active_states.clone(),
                cfg.terminal_states.clone(),
            );
            Ok(Arc::new(tracker))
        }
        other => bail!("unsupported tracker.use {:?} (v0 supports only \"files\")", other),
    }
}
