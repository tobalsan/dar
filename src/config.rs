//! Deserialize and validate `agent.yaml` into a typed config tree.

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    pub id: String,
    pub name: String,
    pub tracker: TrackerConfig,
    pub runner: RunnerConfig,
    pub orchestrator: OrchestratorConfig,
    pub workspace: WorkspaceConfig,
    pub dashboard: DashboardConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TrackerConfig {
    #[serde(rename = "use")]
    pub use_: String,
    pub config: TrackerInner,
    pub active_states: Vec<String>,
    pub terminal_states: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TrackerInner {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RunnerConfig {
    #[serde(rename = "use")]
    pub use_: String,
    pub command: String,
    pub max_run_timeout_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrchestratorConfig {
    pub poll_interval_ms: u64,
    pub max_concurrent: usize,
    pub max_retries: u32,
    pub retry_backoff_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceConfig {
    pub root: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DashboardConfig {
    pub bind: IpAddr,
    pub port: u16,
}

/// Reads `<root>/agent.yaml` and deserializes it.
pub fn load(root: &Path) -> Result<AgentConfig> {
    let path = root.join("agent.yaml");
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading agent.yaml at {}", path.display()))?;
    let cfg: AgentConfig = serde_yaml::from_str(&raw)
        .with_context(|| format!("parsing agent.yaml at {}", path.display()))?;
    Ok(cfg)
}

impl AgentConfig {
    /// Validate invariants the loop relies on. Best-effort, called at startup
    /// and by `doctor`.
    pub fn validate(&self) -> Result<()> {
        if self.tracker.use_ != "files" {
            bail!(
                "tracker.use must be \"files\" in v0 (got {:?})",
                self.tracker.use_
            );
        }
        if self.runner.use_ != "claude-code" {
            bail!(
                "runner.use must be \"claude-code\" in v0 (got {:?})",
                self.runner.use_
            );
        }
        if self.tracker.active_states.is_empty() {
            bail!("tracker.active_states must be non-empty");
        }
        if self.tracker.terminal_states.is_empty() {
            bail!("tracker.terminal_states must be non-empty");
        }
        if self.orchestrator.max_concurrent < 1 {
            bail!("orchestrator.max_concurrent must be >= 1");
        }
        if self.runner.max_run_timeout_ms == 0 {
            bail!("runner.max_run_timeout_ms must be > 0");
        }
        Ok(())
    }
}
