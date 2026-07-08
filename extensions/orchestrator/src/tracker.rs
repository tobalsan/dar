use anyhow::Result;
use cap_tracker::{TrackerBuildConfig, TrackerFactory};
use host_api::ServiceRegistry;

use crate::config::TrackerConfig;

pub use cap_tracker::Tracker;

pub fn build_configured(
    services: &ServiceRegistry,
    cfg: &TrackerConfig,
    root: std::path::PathBuf,
) -> Result<std::sync::Arc<dyn Tracker>> {
    let factory = services.get_named::<dyn TrackerFactory>(&cfg.use_)?;
    factory.build(TrackerBuildConfig {
        root,
        config_path: cfg.config.as_ref().map(|inner| inner.path.clone()),
        active_states: cfg.active_states.clone(),
        terminal_states: cfg.terminal_states.clone(),
        project_slug: cfg.project_slug.clone(),
        project: cfg.project.clone(),
        workspace: cfg.workspace.clone(),
        endpoint: cfg.endpoint.clone(),
        needs_human: cfg.needs_human.clone(),
        team: cfg.team.clone(),
        assignee: cfg.assignee.clone(),
        delegate: cfg.delegate.clone(),
        mention: cfg.mention.clone(),
        labels: cfg.labels(),
    })
}
