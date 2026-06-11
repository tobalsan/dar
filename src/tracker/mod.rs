//! Tracker abstraction. The orchestrator reads issue state normally, and has a
//! deliberately narrow write surface for safety/parking only.

mod files;
pub mod linear;

use std::sync::Arc;

use anyhow::Result;
use host_api::ServiceRegistry;

use crate::config::TrackerConfig;
use crate::paths::AgentPaths;

pub use cap_tracker::Tracker;
pub use files::FileTracker;
pub use linear::LinearTracker;

pub fn register_configured(
    services: &mut ServiceRegistry,
    cfg: &TrackerConfig,
    paths: &AgentPaths,
) -> Result<()> {
    for provider in TRACKER_PROVIDERS {
        if provider.id == cfg.use_ {
            return (provider.register)(services, cfg, paths);
        }
    }
    Ok(())
}

struct TrackerProvider {
    id: &'static str,
    register: fn(&mut ServiceRegistry, &TrackerConfig, &AgentPaths) -> Result<()>,
}

const TRACKER_PROVIDERS: &[TrackerProvider] = &[
    TrackerProvider {
        id: "files",
        register: register_files,
    },
    TrackerProvider {
        id: "linear",
        register: register_linear,
    },
];

fn register_files(
    services: &mut ServiceRegistry,
    cfg: &TrackerConfig,
    paths: &AgentPaths,
) -> Result<()> {
    let issues_dir = paths.issues_dir(cfg);
    services.service::<dyn Tracker>(
        "files",
        Arc::new(FileTracker::new(
            issues_dir,
            cfg.active_states.clone(),
            cfg.terminal_states.clone(),
        )),
    )?;
    Ok(())
}

fn register_linear(
    services: &mut ServiceRegistry,
    cfg: &TrackerConfig,
    _paths: &AgentPaths,
) -> Result<()> {
    let api_key = match std::env::var("LINEAR_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => {
            tracing::warn!("LINEAR_API_KEY is not set; Linear API requests will fail with 401");
            String::new()
        }
    };
    let project_slug = cfg.project_slug.clone().unwrap_or_default();
    let endpoint = cfg
        .endpoint
        .clone()
        .unwrap_or_else(|| "https://api.linear.app/graphql".to_string());
    services.service::<dyn Tracker>(
        "linear",
        Arc::new(LinearTracker::new(linear::LinearTrackerConfig {
            endpoint,
            api_key,
            project_slug,
            active_states: cfg.active_states.clone(),
            terminal_states: cfg.terminal_states.clone(),
            needs_human: cfg.needs_human.clone(),
        })?),
    )?;
    Ok(())
}
