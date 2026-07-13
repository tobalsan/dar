use anyhow::Result;
use cap_tracker::{TrackerBuildConfig, TrackerFactory};
use host_api::ServiceRegistry;

use crate::workflow_config::EffectiveLoopConfig;

pub use cap_tracker::Tracker;

/// Build the configured tracker from the resolved loop config. The loop config
/// is the single source of truth for tracker dimensions (WORKFLOW.md
/// frontmatter). `root` is the **workflow root** (the WORKFLOW.md dir, equal to
/// the agent folder for the default workflow): the files tracker resolves its
/// relative `tracker.path` issues dir against it, so a `--workflow <path>` run
/// reads that workflow's own issues, not the agent folder's.
pub fn build_configured(
    services: &ServiceRegistry,
    cfg: &EffectiveLoopConfig,
    root: std::path::PathBuf,
) -> Result<std::sync::Arc<dyn Tracker>> {
    let factory = services.get_named::<dyn TrackerFactory>(&cfg.tracker_kind)?;
    factory.build(TrackerBuildConfig {
        root,
        config_path: cfg.tracker_config_path.clone(),
        active_states: cfg.active_states.clone(),
        terminal_states: cfg.terminal_states.clone(),
        projects: cfg.tracker_projects.clone(),
        workspace: cfg.tracker_workspace.clone(),
        endpoint: Some(cfg.tracker_endpoint.clone()),
        needs_human: cfg.needs_human.clone(),
        team: cfg.tracker_team.clone(),
        assignee: cfg.tracker_assignee.clone(),
        delegate: cfg.tracker_delegate.clone(),
        mention: cfg.tracker_mention.clone(),
        labels: cfg.tracker_labels.clone(),
    })
}
