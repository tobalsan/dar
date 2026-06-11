//! Tracker abstraction. The orchestrator reads issue state normally, and has a
//! deliberately narrow write surface for safety/parking only.

mod files;
pub mod linear;

use std::sync::Arc;

use anyhow::{bail, Result};

use crate::config::TrackerConfig;
use crate::paths::AgentPaths;

pub use cap_tracker::Tracker;
pub use files::FileTracker;
pub use linear::LinearTracker;

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
                needs_human: cfg.needs_human.clone(),
            })?;
            Ok(Arc::new(tracker))
        }
        other => bail!(
            "unsupported tracker.use {:?} (supports \"files\" and \"linear\")",
            other
        ),
    }
}
