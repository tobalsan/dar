//! Deserialize and validate `agent.yaml` into a typed config tree.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use cap_runner::DEFAULT_MAX_RUN_TIMEOUT_MS;
use serde::{Deserialize, Serialize};

/// A YAML scalar or list, normalised to `Vec<String>`. Used by tracker `label`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum StringOrVec {
    Scalar(String),
    List(Vec<String>),
}

impl StringOrVec {
    pub fn to_vec(&self) -> Vec<String> {
        match self {
            StringOrVec::Scalar(s) => vec![s.clone()],
            StringOrVec::List(v) => v.clone(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct AgentConfig {
    pub id: String,
    #[allow(dead_code)]
    pub name: String,
    /// Tracker config. Part of the orchestrator trio (tracker, orchestrator,
    /// workspace): all three present runs the loop, all three absent boots a
    /// passive agent (TUI / scheduled jobs / custom extensions only).
    #[serde(default)]
    pub tracker: Option<TrackerConfig>,
    pub runner: RunnerConfig,
    /// Orchestrator loop config. See `tracker` for the trio contract.
    #[serde(default)]
    pub orchestrator: Option<OrchestratorConfig>,
    #[serde(default)]
    pub hitl: HitlConfig,
    /// Workspace config. See `tracker` for the trio contract.
    #[serde(default)]
    pub workspace: Option<WorkspaceConfig>,
    #[serde(default)]
    pub dashboard: DashboardConfig,
    /// Host-level foreground slot selection: the id of the extension that owns
    /// the terminal. Absent → "logs" (the frontend-log extension).
    #[serde(default = "default_foreground")]
    pub foreground: String,
    /// Per-extension config, keyed by extension id. Each value is handed to the
    /// matching extension via the host `ConfigStore`. Missing section = empty.
    #[serde(default)]
    pub extensions: HashMap<String, serde_yaml::Value>,
    /// Optional agent-identity files assembled into the system context at boot,
    /// after `AGENTS.md`. Each entry is a bare path or `{ path, required? }`.
    /// Absent key ⇒ `AGENTS.md` only.
    #[serde(default)]
    pub system_files: Option<Vec<system_files::SystemFileEntry>>,
}

fn default_foreground() -> String {
    "logs".to_string()
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
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
    /// Linear team key (e.g. "ALG"); unconstrained when absent.
    #[serde(default)]
    pub team: Option<String>,
    /// Linear assignee: UUID, displayName (@thinh), name, or email; resolved at boot.
    #[serde(default)]
    pub assignee: Option<String>,
    /// Linear label name(s): a single string or a list (OR within labels).
    #[serde(default)]
    pub label: Option<StringOrVec>,
}

impl TrackerConfig {
    /// Configured label names, empty when unset.
    pub fn labels(&self) -> Vec<String> {
        self.label
            .as_ref()
            .map(StringOrVec::to_vec)
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct TrackerInner {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct RunnerConfig {
    /// Runner kind. Accepts `use` (canonical), `sdk` (alias). Empty / absent → "pi".
    #[serde(rename = "use", alias = "sdk", default)]
    pub use_: String,
    #[serde(default)]
    pub command: String,
    /// Optional model identifier forwarded to runners that accept one.
    #[serde(default)]
    pub model: Option<String>,
    /// Optional provider identifier forwarded to runners that accept one.
    #[serde(default)]
    pub provider: Option<String>,
    /// Canonical reasoning level on the scale `none | minimal | low | medium |
    /// high | xhigh`. Accepts `thinking` (canonical) or `effort` (alias).
    /// Validated against the resolved runner's supported subset at boot.
    #[serde(default, alias = "effort")]
    pub thinking: Option<String>,
    #[serde(
        default = "default_turn_timeout_ms",
        alias = "turn_timeout_ms",
        alias = "max_run_timeout_ms"
    )]
    pub max_run_timeout_ms: u64,
    #[serde(default = "default_stall_timeout_ms")]
    pub stall_timeout_ms: u64,
    /// Max turns a single run may take before the orchestrator stops asking a
    /// turn-capable runner to continue (turn-loop backstop). Default 20.
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
}

fn default_turn_timeout_ms() -> u64 {
    DEFAULT_MAX_RUN_TIMEOUT_MS
}

fn default_max_turns() -> u32 {
    20
}

fn default_stall_timeout_ms() -> u64 {
    5 * 60 * 1000
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
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

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            poll_interval_ms: 1000,
            max_concurrent: default_max_concurrent(),
            max_active_runs: default_max_active_runs(),
            max_retries: 3,
            retry_backoff_ms: default_retry_backoff_ms(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
pub struct HitlConfig {
    #[serde(default)]
    pub notifier: HitlNotifierConfig,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
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

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct WorkspaceConfig {
    pub root: PathBuf,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct DashboardConfig {
    /// Bind address. Defaults to `0.0.0.0` so a single `agentropy dash`
    /// aggregator (and, over a tailnet, a remote browser) can reach the
    /// agent's own dashboard.
    #[serde(default = "default_dashboard_bind")]
    pub bind: IpAddr,
    /// Bind port. Defaults to `0` (OS-assigned ephemeral) so running more than
    /// one agent on a host never collides on a fixed port; the aggregator
    /// discovers the real port from the presence registry.
    #[serde(default = "default_dashboard_port")]
    pub port: u16,
    #[serde(default)]
    pub webhook_secret: Option<String>,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            bind: default_dashboard_bind(),
            port: default_dashboard_port(),
            webhook_secret: None,
        }
    }
}

fn default_dashboard_bind() -> IpAddr {
    IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)
}

fn default_dashboard_port() -> u16 {
    0
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

    /// True when the orchestrator trio (`tracker` + `orchestrator` +
    /// `workspace`) is fully configured, i.e. the loop should run. False for a
    /// passive agent (all three absent).
    pub fn loop_enabled(&self) -> bool {
        self.trio().is_some()
    }

    /// Tracker config or a neutral default. Used by `EffectiveLoopConfig::merge`
    /// so it stays total even for a passive agent (whose effective config is
    /// never consumed by a loop). Real loops always have the trio present.
    pub fn tracker_or_default(&self) -> TrackerConfig {
        self.tracker.clone().unwrap_or_default()
    }

    /// Orchestrator config or a neutral default. See `tracker_or_default`.
    pub fn orchestrator_or_default(&self) -> OrchestratorConfig {
        self.orchestrator.clone().unwrap_or_default()
    }

    /// Workspace config or a neutral default. See `tracker_or_default`.
    pub fn workspace_or_default(&self) -> WorkspaceConfig {
        self.workspace.clone().unwrap_or_default()
    }

    /// Borrow the orchestrator trio when all three are present; `None` for a
    /// passive agent. Partial configs are rejected by `validate`, so callers
    /// reaching this after validation see all-or-nothing.
    pub fn trio(&self) -> Option<(&TrackerConfig, &OrchestratorConfig, &WorkspaceConfig)> {
        match (&self.tracker, &self.orchestrator, &self.workspace) {
            (Some(t), Some(o), Some(w)) => Some((t, o, w)),
            _ => None,
        }
    }

    /// Validate invariants the loop relies on. Best-effort, called at startup
    /// and by `doctor`.
    pub fn validate(&self) -> Result<()> {
        // The orchestrator trio is all-or-nothing: either all three configure a
        // running loop, or none do for a passive agent. A partial trio is a
        // misconfiguration.
        let present = self.tracker.is_some() as u8
            + self.orchestrator.is_some() as u8
            + self.workspace.is_some() as u8;
        if present != 0 && present != 3 {
            bail!("tracker, orchestrator, and workspace must be configured together (or all omitted for a passive agent)");
        }

        if let Some((tracker, orchestrator, _workspace)) = self.trio() {
            if tracker.use_.trim().is_empty() {
                bail!("tracker.use must be non-empty");
            }
            if tracker.active_states.is_empty() {
                bail!("tracker.active_states must be non-empty");
            }
            if tracker.terminal_states.is_empty() {
                bail!("tracker.terminal_states must be non-empty");
            }
            if orchestrator.max_concurrent < 1 {
                bail!("orchestrator.max_concurrent must be >= 1");
            }
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

    const BASE: &str = "id: a\nname: A\ntracker:\n  use: files\n  config:\n    path: ./issues\n  active_states: [todo]\n  terminal_states: [done]\nrunner:\n  use: fake\norchestrator:\n  poll_interval_ms: 1000\n  max_retries: 3\nworkspace:\n  root: ./workspaces\ndashboard:\n  bind: 127.0.0.1\n  port: 7878\n";

    #[test]
    fn extension_configs_extracts_per_extension_section() {
        let raw = format!(
            "{BASE}extensions:\n  dashboard:\n    port: 9000\n  example:\n    greeting: hi\n"
        );
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

    #[test]
    fn tracker_dimensions_parse_with_scalar_label() {
        let raw = "id: a\nname: A\ntracker:\n  use: linear\n  active_states: [todo]\n  terminal_states: [done]\n  team: ALG\n  assignee: \"@thinh\"\n  label: bug\nrunner:\n  use: fake\norchestrator:\n  poll_interval_ms: 1000\n  max_retries: 3\nworkspace:\n  root: ./workspaces\ndashboard:\n  bind: 127.0.0.1\n  port: 7878\n";
        let cfg: AgentConfig = serde_yaml::from_str(raw).unwrap();
        let tracker = cfg.tracker.as_ref().unwrap();
        assert_eq!(tracker.team.as_deref(), Some("ALG"));
        assert_eq!(tracker.assignee.as_deref(), Some("@thinh"));
        assert_eq!(tracker.labels(), vec!["bug"]);
    }

    #[test]
    fn system_files_absent_is_none() {
        let cfg: AgentConfig = serde_yaml::from_str(BASE).unwrap();
        assert!(cfg.system_files.is_none());
    }

    #[test]
    fn system_files_round_trips_bare_and_detailed_forms() {
        use system_files::SystemFileEntry;
        let raw = format!(
            "{BASE}system_files:\n  - docs/style.md\n  - path: docs/policy.md\n    required: true\n  - path: docs/optional.md\n"
        );
        let cfg: AgentConfig = serde_yaml::from_str(&raw).unwrap();
        assert_eq!(
            cfg.system_files.unwrap(),
            vec![
                SystemFileEntry::Bare("docs/style.md".to_string()),
                SystemFileEntry::Detailed {
                    path: "docs/policy.md".to_string(),
                    required: true,
                },
                SystemFileEntry::Detailed {
                    path: "docs/optional.md".to_string(),
                    required: false,
                },
            ]
        );
    }

    #[test]
    fn tracker_label_parses_as_list() {
        let raw = "id: a\nname: A\ntracker:\n  use: linear\n  active_states: [todo]\n  terminal_states: [done]\n  label: [bug, urgent]\nrunner:\n  use: fake\norchestrator:\n  poll_interval_ms: 1000\n  max_retries: 3\nworkspace:\n  root: ./workspaces\ndashboard:\n  bind: 127.0.0.1\n  port: 7878\n";
        let cfg: AgentConfig = serde_yaml::from_str(raw).unwrap();
        assert_eq!(cfg.tracker.unwrap().labels(), vec!["bug", "urgent"]);
    }

    /// A passive agent (runner only, no trio) loads and validates OK.
    #[test]
    fn passive_agent_validates() {
        let raw = "id: a\nname: A\nrunner:\n  use: fake\n";
        let cfg: AgentConfig = serde_yaml::from_str(raw).unwrap();
        assert!(cfg.tracker.is_none());
        assert!(cfg.orchestrator.is_none());
        assert!(cfg.workspace.is_none());
        assert!(!cfg.loop_enabled());
        cfg.validate().unwrap();
    }

    /// A partial trio (tracker + orchestrator but no workspace) fails validation.
    #[test]
    fn partial_trio_fails_validation() {
        let raw = "id: a\nname: A\ntracker:\n  use: files\n  config:\n    path: ./issues\n  active_states: [todo]\n  terminal_states: [done]\nrunner:\n  use: fake\norchestrator:\n  poll_interval_ms: 1000\n  max_retries: 3\n";
        let cfg: AgentConfig = serde_yaml::from_str(raw).unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("tracker, orchestrator, and workspace must be configured together"),
            "{err}"
        );
    }

    /// The full trio still validates OK.
    #[test]
    fn full_trio_validates() {
        let cfg: AgentConfig = serde_yaml::from_str(BASE).unwrap();
        assert!(cfg.loop_enabled());
        cfg.validate().unwrap();
    }
}
