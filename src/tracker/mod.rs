//! Read-only tracker abstraction. The trait locks the read verb set (no write
//! surface in v0: the orchestrator never writes issue state). A factory selects
//! the implementation (`FileTracker` or `LinearTracker`) based on config.

mod files;
mod linear;

use std::sync::Arc;

use anyhow::{bail, Result};

use crate::config::TrackerConfig;
use crate::domain::Issue;
use crate::paths::AgentPaths;

pub use files::FileTracker;
pub use linear::LinearTracker;

/// Read-only view over issues. Sync because fs reads are cheap; for network-backed
/// implementations the sync methods use `block_in_place` internally.
pub trait Tracker: Send + Sync {
    /// All issues whose state is in `active_states`. Implementations MUST skip
    /// issues that are blocked by any non-terminal issue.
    fn poll_candidates(&self) -> Result<Vec<Issue>>;
    /// Current state of the given issue ids (by id or identifier). Missing ids
    /// are simply omitted from the result.
    #[allow(dead_code)]
    fn fetch_states(&self, ids: &[String]) -> Result<Vec<Issue>>;
    /// All issues whose state is in `terminal_states`.
    #[allow(dead_code)]
    fn fetch_terminal(&self) -> Result<Vec<Issue>>;
    /// One issue by id or identifier; `None` if not found.
    fn fetch_one(&self, id: &str) -> Result<Option<Issue>>;
    /// Minimum rate-limit requests remaining seen since startup.
    /// Returns `None` when rate-limit tracking is not applicable (e.g. FileTracker).
    fn rate_limit_remaining(&self) -> Option<i64> {
        None
    }
    /// Whether the orchestrator should apply the local v0 candidate sort.
    /// Linear preserves API/native order.
    fn sort_candidates_locally(&self) -> bool {
        false
    }
}

/// Build the configured tracker from `cfg`. Supports `use: files` and `use: linear`.
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
        "linear" => {
            let api_key = match std::env::var("LINEAR_API_KEY") {
                Ok(k) if !k.is_empty() => k,
                _ => {
                    tracing::warn!(
                        "LINEAR_API_KEY is not set; Linear API requests will fail with 401"
                    );
                    String::new()
                }
            };
            let project_slug = cfg.project_slug.clone().unwrap_or_default();
            let endpoint = cfg
                .endpoint
                .clone()
                .unwrap_or_else(|| "https://api.linear.app/graphql".to_string());
            let tracker = LinearTracker::new(linear::LinearTrackerConfig {
                endpoint,
                api_key,
                project_slug,
                active_states: cfg.active_states.clone(),
                terminal_states: cfg.terminal_states.clone(),
            })?;
            Ok(Arc::new(tracker))
        }
        other => bail!(
            "unsupported tracker.use {:?} (supports \"files\" and \"linear\")",
            other
        ),
    }
}
