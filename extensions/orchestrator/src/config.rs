//! Deserialize and validate `agent.yaml` into a typed config tree.

use std::collections::HashMap;
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
    #[serde(default)]
    pub hitl: HitlConfig,
    pub workspace: WorkspaceConfig,
    pub dashboard: DashboardConfig,
    /// Host-level foreground slot selection: the id of the extension that owns
    /// the terminal. Absent → "logs" (the frontend-log extension).
    #[serde(default = "default_foreground")]
    pub foreground: String,
    /// Per-extension config, keyed by extension id. Each value is handed to the
    /// matching extension via the host `ConfigStore`. Missing section = empty.
    #[serde(default)]
    pub extensions: HashMap<String, serde_yaml::Value>,
}

fn default_foreground() -> String {
    "logs".to_string()
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
    #[serde(default = "default_stall_timeout_ms")]
    pub stall_timeout_ms: u64,
}

fn default_turn_timeout_ms() -> u64 {
    60 * 60 * 1000
}

fn default_stall_timeout_ms() -> u64 {
    5 * 60 * 1000
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrchestratorConfig {
    pub poll_interval_ms: u64,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    #[serde(default = "default_max_active_runs")]
    pub max_active_runs: u32,
    pub max_retries: u32,
    #[serde(default = "default_retry_backoff_ms")]
    pub retry_backoff_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct HitlConfig {
    #[serde(default)]
    pub notifier: HitlNotifierConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HitlNotifierConfig {
    #[serde(rename = "use", default = "default_hitl_notifier_use")]
    pub use_: String,
    #[serde(default = "default_hitl_window_secs")]
    pub window_secs: u64,
    #[serde(default = "default_hitl_max_items")]
    pub max_items: usize,
    #[serde(default)]
    pub webhook_url: Option<String>,
    #[serde(default)]
    pub command: Vec<String>,
}

impl Default for HitlNotifierConfig {
    fn default() -> Self {
        Self {
            use_: default_hitl_notifier_use(),
            window_secs: default_hitl_window_secs(),
            max_items: default_hitl_max_items(),
            webhook_url: None,
            command: Vec::new(),
        }
    }
}

fn default_hitl_notifier_use() -> String {
    "stdout".to_string()
}

fn default_hitl_window_secs() -> u64 {
    60
}

fn default_hitl_max_items() -> usize {
    5
}

fn default_max_concurrent() -> usize {
    3
}

fn default_max_active_runs() -> u32 {
    3
}

fn default_retry_backoff_ms() -> u64 {
    30 * 1000
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceConfig {
    pub root: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DashboardConfig {
    pub bind: IpAddr,
    pub port: u16,
    #[serde(default)]
    pub webhook_secret: Option<String>,
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
    /// Per-extension config keyed by extension id, with each value converted to a
    /// `serde_json::Value` for the host `ConfigStore`. Unknown ids are passed
    /// through untouched (tolerated by the host).
    pub fn extension_configs(&self) -> Result<HashMap<String, serde_json::Value>> {
        self.extensions
            .iter()
            .map(|(id, value)| {
                let json = serde_json::to_value(value)
                    .with_context(|| format!("converting extensions.{id} config to JSON"))?;
                Ok((id.clone(), json))
            })
            .collect()
    }

    /// Validate invariants the loop relies on. Best-effort, called at startup
    /// and by `doctor`.
    pub fn validate(&self) -> Result<()> {
        if self.tracker.use_.trim().is_empty() {
            bail!("tracker.use must be non-empty");
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
        if !matches!(
            self.hitl.notifier.use_.as_str(),
            "none" | "stdout" | "webhook" | "cli"
        ) {
            bail!(
                "hitl.notifier.use must be one of none, stdout, webhook, cli (got {:?})",
                self.hitl.notifier.use_
            );
        }
        if self.hitl.notifier.window_secs == 0 {
            bail!("hitl.notifier.window_secs must be > 0");
        }
        if self.hitl.notifier.max_items == 0 {
            bail!("hitl.notifier.max_items must be > 0");
        }
        if self.hitl.notifier.use_ == "webhook"
            && self
                .hitl
                .notifier
                .webhook_url
                .as_deref()
                .is_none_or(str::is_empty)
        {
            bail!("hitl.notifier.webhook_url is required when hitl.notifier.use is \"webhook\"");
        }
        if self.hitl.notifier.use_ == "cli"
            && self
                .hitl
                .notifier
                .command
                .first()
                .is_none_or(|program| program.is_empty())
        {
            bail!("hitl.notifier.command is required when hitl.notifier.use is \"cli\"");
        }
        if self.runner.max_run_timeout_ms == 0 {
            bail!("runner.max_run_timeout_ms must be > 0");
        }
        if self.runner.stall_timeout_ms == 0 {
            bail!("runner.stall_timeout_ms must be > 0");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "id: a\nname: A\ntracker:\n  use: files\n  config:\n    path: ./issues\n  active_states: [todo]\n  terminal_states: [done]\nrunner:\n  use: claude-code\norchestrator:\n  poll_interval_ms: 1000\n  max_retries: 3\nworkspace:\n  root: ./workspaces\ndashboard:\n  bind: 127.0.0.1\n  port: 7878\n";

    #[test]
    fn extension_configs_extracts_per_extension_section() {
        let raw = format!("{BASE}extensions:\n  dashboard:\n    port: 9000\n  example:\n    greeting: hi\n");
        let cfg: AgentConfig = serde_yaml::from_str(&raw).unwrap();
        let configs = cfg.extension_configs().unwrap();
        assert_eq!(configs["dashboard"], serde_json::json!({ "port": 9000 }));
        assert_eq!(configs["example"], serde_json::json!({ "greeting": "hi" }));
    }

    #[test]
    fn extension_configs_empty_when_section_missing() {
        let cfg: AgentConfig = serde_yaml::from_str(BASE).unwrap();
        assert!(cfg.extension_configs().unwrap().is_empty());
    }

    #[test]
    fn foreground_defaults_to_logs_when_absent() {
        let cfg: AgentConfig = serde_yaml::from_str(BASE).unwrap();
        assert_eq!(cfg.foreground, "logs");
    }

    #[test]
    fn foreground_parses_explicit_value() {
        let raw = format!("{BASE}foreground: tui\n");
        let cfg: AgentConfig = serde_yaml::from_str(&raw).unwrap();
        assert_eq!(cfg.foreground, "tui");
    }
}
