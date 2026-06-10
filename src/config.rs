//! Deserialize and validate `agent.yaml` into a typed config tree.

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    pub id: String,
    #[allow(dead_code)]
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
    /// Required for `use: files`; ignored for `use: linear`.
    #[serde(default)]
    pub config: Option<TrackerInner>,
    pub active_states: Vec<String>,
    pub terminal_states: Vec<String>,
    /// Linear project slugId (used when `use: linear`).
    #[serde(default)]
    pub project_slug: Option<String>,
    /// Linear GraphQL endpoint override (default `https://api.linear.app/graphql`).
    #[serde(default)]
    pub endpoint: Option<String>,
    /// State name used by orchestrator safety/parking writes.
    #[serde(default)]
    pub needs_human: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TrackerInner {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RunnerConfig {
    /// Runner kind. Accepts `use` (canonical), `sdk` (alias). Empty / absent → "pi".
    #[serde(rename = "use", alias = "sdk", default)]
    pub use_: String,
    #[serde(default)]
    pub command: String,
    /// Optional model identifier forwarded to runners that accept one.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(
        default = "default_turn_timeout_ms",
        alias = "turn_timeout_ms",
        alias = "max_run_timeout_ms"
    )]
    pub max_run_timeout_ms: u64,
}

fn default_turn_timeout_ms() -> u64 {
    60 * 60 * 1000
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrchestratorConfig {
    pub poll_interval_ms: u64,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    #[serde(default = "default_max_active_runs")]
    pub max_active_runs: u32,
    pub max_retries: u32,
    pub retry_backoff_ms: u64,
}

fn default_max_concurrent() -> usize {
    3
}

fn default_max_active_runs() -> u32 {
    3
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
        if !matches!(self.tracker.use_.as_str(), "files" | "linear") {
            bail!(
                "tracker.use must be \"files\" or \"linear\" (got {:?})",
                self.tracker.use_
            );
        }
        if self.tracker.use_ == "files" && self.tracker.config.is_none() {
            bail!("tracker.config.path is required when tracker.use is \"files\"");
        }
        if !matches!(
            self.runner.use_.as_str(),
            "" | "pi" | "claude" | "claude-code" | "codex" | "cli" | "fake"
        ) {
            bail!(
                "runner.use must be one of pi, claude, codex, cli, fake (got {:?})",
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
