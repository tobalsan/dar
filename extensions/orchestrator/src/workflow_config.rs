//! WORKFLOW.md frontmatter structs, parser, and effective-loop-config merge.
//!
//! WORKFLOW.md = optional YAML frontmatter + Markdown prompt body.
//! Frontmatter sections: tracker / polling / workspace / agent / hooks / server / linear.
//!
//! The effective loop config is resolved from WORKFLOW.md frontmatter alone
//! (tracker/polling/workspace), with inline defaults for absent fields;
//! agent.yaml contributes only runner/dashboard config. The frontmatter
//! `agent:` section still overrides runner fields.
//!
//! Tracker state names support two forms:
//!
//!   Flat (preferred):
//!     tracker:
//!       active_states: [todo, in_progress]
//!       terminal_states: [done, cancelled]
//!       needs_human: needs_human
//!
//!   Legacy nested (tracker.states.*):
//!     tracker:
//!       states:
//!         active: [todo, in_progress]
//!         terminal: [done, cancelled]
//!         needs_human: needs_human
//!
//! Flat fields take precedence over the nested form when both are present.

use std::net::IpAddr;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use cap_runner::DEFAULT_RUNNER_KIND;
use serde::{Deserialize, Serialize};

use crate::config::{AgentConfig, StringOrVec};

/// Loop-config defaults, applied when the WORKFLOW.md frontmatter omits a field.
const DEFAULT_POLL_INTERVAL_MS: u64 = 1000;
const DEFAULT_MAX_CONCURRENT: usize = 3;
const DEFAULT_MAX_ACTIVE_RUNS: u32 = 3;
const DEFAULT_MAX_RETRIES: u32 = 3;
const DEFAULT_RETRY_BACKOFF_MS: u64 = 30_000;
const DEFAULT_WORKSPACE_ROOT: &str = "workspaces";
const DEFAULT_LINEAR_ENDPOINT: &str = "https://api.linear.app/graphql";

// ---------------------------------------------------------------------------
// Frontmatter structs
// ---------------------------------------------------------------------------

/// All sections of the WORKFLOW.md YAML frontmatter.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct WorkflowFrontmatter {
    pub tracker: Option<WfTrackerConfig>,
    pub polling: Option<WfPollingConfig>,
    pub workspace: Option<WfWorkspaceConfig>,
    pub agent: Option<WfAgentConfig>,
    pub hooks: Option<WfHooksConfig>,
    pub server: Option<WfServerConfig>,
    pub linear: Option<WfLinearConfig>,
}

/// Tracker overrides from WORKFLOW.md.
/// Flat fields (`active_states`, `terminal_states`, `needs_human`) take
/// precedence over the legacy nested form (`states.active` / `states.terminal`
/// / `states.needs_human`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WfTrackerConfig {
    // --- Flat form (preferred) ---
    pub active_states: Option<Vec<String>>,
    pub terminal_states: Option<Vec<String>>,
    pub needs_human: Option<String>,
    // --- Legacy nested form ---
    pub states: Option<WfTrackerStates>,
    // --- Tracker kind + tracker-specific ---
    /// `"files"`, `"linear"`, or `"plane"`. The tracker backend to run.
    pub kind: Option<String>,
    /// Tracker-agnostic project scope (scalar or list): Linear slugIds, Plane
    /// project UUIDs, etc. Empty/absent ⇒ unconstrained.
    pub projects: Option<StringOrVec>,
    /// Files-tracker issues directory, relative to the WORKFLOW.md dir
    /// (default `issues`). Was agent.yaml-only `tracker.config.path`.
    pub path: Option<PathBuf>,
    /// Plane workspace slug.
    pub workspace: Option<String>,
    /// Linear GraphQL endpoint override (default `https://api.linear.app/graphql`).
    pub endpoint: Option<String>,
    /// Linear team key override.
    pub team: Option<String>,
    /// Linear assignee override (UUID / @displayName / name / email).
    pub assignee: Option<String>,
    /// Linear delegate/app agent override (UUID / @displayName / name / email).
    pub delegate: Option<String>,
    /// Plane bot display name to target by work-item description @mention.
    pub mention: Option<String>,
    /// Linear label override (scalar or list).
    pub label: Option<crate::config::StringOrVec>,
}

/// Legacy `tracker.states.*` nesting.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WfTrackerStates {
    pub active: Option<Vec<String>>,
    pub terminal: Option<Vec<String>>,
    pub needs_human: Option<String>,
}

/// Polling / scheduling overrides from WORKFLOW.md.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct WfPollingConfig {
    /// Accepts both `interval_ms` and the agent.yaml-style `poll_interval_ms`.
    #[serde(alias = "poll_interval_ms")]
    pub interval_ms: Option<u64>,
    pub jitter_ms: Option<u64>,
    pub max_concurrent: Option<usize>,
    pub max_retries: Option<u32>,
    pub retry_backoff_ms: Option<u64>,
    /// When true (the default), a WORKFLOW.md parse error on a live-reload
    /// keeps the last-good snapshot. When false, the error is surfaced.
    /// Accepts both `allow_stale` and `allowStale` in YAML.
    #[serde(alias = "allowStale")]
    pub allow_stale: Option<bool>,
}

/// Workspace-root override from WORKFLOW.md.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WfWorkspaceConfig {
    pub root: Option<PathBuf>,
    pub reuse: Option<bool>,
    pub cleanup_on_terminal: Option<bool>,
}

/// Runner / model overrides from WORKFLOW.md.
///
/// `runner`/`kind` are the canonical runner-kind overrides; `sdk` is accepted
/// for compatibility with agent.yaml-era configs.
/// Falls back to agent.yaml `runner.use` / `runner.command`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WfAgentConfig {
    /// Runner kind override (canonical). Equivalent to agent.yaml `runner.use`.
    pub sdk: Option<String>,
    /// Runner kind override (alias for `sdk`).
    pub runner: Option<String>,
    pub kind: Option<String>,
    /// Runner command override. Equivalent to agent.yaml `runner.command`.
    pub command: Option<String>,
    /// Model identifier (not present in v0 agent.yaml; new in WORKFLOW.md).
    pub model: Option<String>,
    /// Model provider override (e.g. "openai", "anthropic"). Forwarded to runners that accept one.
    pub provider: Option<String>,
    /// Canonical reasoning level on the scale `none | minimal | low | medium |
    /// high | xhigh`. Overrides agent.yaml `runner.thinking`. Accepts `thinking`
    /// (canonical) or `effort` (alias). Validated against the resolved runner's
    /// supported subset at config-load time.
    #[serde(alias = "effort")]
    pub thinking: Option<String>,
    /// Per-attempt timeout override (ms).
    pub max_run_timeout_ms: Option<u64>,
    pub turn_timeout_ms: Option<u64>,
    pub stall_timeout_ms: Option<u64>,
    pub max_active_runs: Option<u32>,
    /// Max turns a turn-capable run may take before the orchestrator stops
    /// asking it to continue. Overrides agent.yaml `runner.max_turns`.
    pub max_turns: Option<u32>,
}

impl WfAgentConfig {
    /// WORKFLOW.md runner/kind overrides win over agent.yaml sdk/use fallback.
    pub fn effective_runner(&self) -> Option<&str> {
        self.runner
            .as_deref()
            .or(self.kind.as_deref())
            .or(self.sdk.as_deref())
    }
}

/// Lifecycle hook scripts from WORKFLOW.md.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct WfHooksConfig {
    pub after_create: Option<String>,
    pub before_run: Option<String>,
    pub after_run: Option<String>,
    pub before_remove: Option<String>,
    // Legacy names retained as parsed no-ops for backwards compatibility.
    pub before_dispatch: Option<String>,
    pub after_success: Option<String>,
    pub after_failure: Option<String>,
    pub on_needs_human: Option<String>,
}

/// Dashboard / HTTP server overrides from WORKFLOW.md.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WfServerConfig {
    pub bind: Option<IpAddr>,
    pub port: Option<u16>,
}

/// Linear integration settings from WORKFLOW.md.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct WfLinearConfig {
    pub project: Option<String>,
    pub team: Option<String>,
    /// Deprecated/no-op: the `linear_graphql` tool is now an
    /// extension-registered host tool, always available to wired agents through
    /// the host MCP bridge (it no longer needs a per-workflow opt-in). Parsed
    /// for backward-compatibility with existing frontmatter.
    #[serde(alias = "exposeGraphqlTool")]
    pub worker_tool: Option<bool>,
    /// HMAC-SHA256 secret used to verify Linear webhook requests.
    pub webhook_secret: Option<String>,
}

// ---------------------------------------------------------------------------
// WorkflowSnapshot
// ---------------------------------------------------------------------------

/// Parsed WORKFLOW.md = frontmatter + Markdown prompt body (template source).
#[derive(Debug, Clone)]
pub struct WorkflowSnapshot {
    pub frontmatter: WorkflowFrontmatter,
    /// The Markdown body after stripping frontmatter; used as the minijinja template.
    pub body: String,
}

/// Parse raw WORKFLOW.md content into a `WorkflowSnapshot`.
pub fn parse_workflow_md(raw: &str) -> Result<WorkflowSnapshot> {
    let (fm_raw, body) = split_frontmatter(raw);
    let frontmatter: WorkflowFrontmatter = match fm_raw {
        Some(fm) => {
            warn_unknown_polling_keys(fm);
            serde_yaml::from_str(fm).context("parsing WORKFLOW.md frontmatter YAML")?
        }
        None => WorkflowFrontmatter::default(),
    };
    Ok(WorkflowSnapshot {
        frontmatter,
        body: body.to_string(),
    })
}

/// Keys accepted in the `polling:` frontmatter section, including serde aliases.
const KNOWN_POLLING_KEYS: &[&str] = &[
    "interval_ms",
    "poll_interval_ms",
    "jitter_ms",
    "max_concurrent",
    "max_retries",
    "retry_backoff_ms",
    "allow_stale",
    "allowStale",
];

/// Unknown `polling:` keys are silently dropped by serde, which turns a typo
/// into a wrong-default footgun (e.g. a misnamed interval key falling back to
/// the 1 s poll default). Warn so the mistake is visible in logs and doctor.
fn warn_unknown_polling_keys(fm_raw: &str) {
    let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(fm_raw) else {
        return;
    };
    let Some(map) = value.get("polling").and_then(serde_yaml::Value::as_mapping) else {
        return;
    };
    for key in map.keys().filter_map(serde_yaml::Value::as_str) {
        if !KNOWN_POLLING_KEYS.contains(&key) {
            tracing::warn!("WORKFLOW.md `polling.{key}` is not a recognized key; ignoring it");
        }
    }
}

/// Split `---\n…\n---` frontmatter from the rest of the document.
/// Returns `(Option<frontmatter_str>, body_str)`.
fn split_frontmatter(src: &str) -> (Option<&str>, &str) {
    let rest = if let Some(r) = src.strip_prefix("---\n") {
        r
    } else if let Some(r) = src.strip_prefix("---\r\n") {
        r
    } else {
        return (None, src);
    };
    match find_closing_delim(rest) {
        Some((fm, body)) => (Some(fm), body),
        // Unterminated frontmatter: treat the whole input as body.
        None => (None, src),
    }
}

/// Given the content after the opening `---`, find the closing `---` line.
/// Returns `(frontmatter_content, trimmed_body_after_delimiter)`.
fn find_closing_delim(s: &str) -> Option<(&str, &str)> {
    let mut offset = 0usize;
    for line in s.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" {
            let fm = &s[..offset];
            let body = s[offset + line.len()..].trim_start_matches(['\r', '\n']);
            return Some((fm, body));
        }
        offset += line.len();
    }
    // Final line with no trailing newline.
    if s[offset..].trim_end() == "---" {
        Some((&s[..offset], ""))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Effective loop config
// ---------------------------------------------------------------------------

/// The resolved config the orchestrator uses for its loop logic.
///
/// Derived at startup (and re-derived on every successful WORKFLOW.md reload)
/// from WORKFLOW.md frontmatter, which is the sole source of tracker/polling/
/// workspace config; agent.yaml contributes only runner/dashboard fields.
#[derive(Debug, Clone)]
pub struct EffectiveLoopConfig {
    // Tracker
    pub tracker_kind: String,
    pub active_states: Vec<String>,
    pub terminal_states: Vec<String>,
    /// Optional "needs human" state name; when a child sets this state the issue
    /// is treated as active (not terminal) so the orchestrator won't re-dispatch
    /// it automatically.
    pub needs_human: Option<String>,
    /// Tracker-agnostic project scope (Linear slugIds, Plane project UUIDs).
    /// Empty ⇒ unconstrained.
    pub tracker_projects: Vec<String>,
    /// Files-tracker issues dir (relative to the WORKFLOW.md dir); tracker
    /// backends that ignore a config path leave this `None`.
    pub tracker_config_path: Option<PathBuf>,
    /// Plane workspace slug (only relevant when tracker_kind == "plane").
    pub tracker_workspace: Option<String>,
    /// Linear GraphQL endpoint (only relevant when tracker_kind == "linear").
    pub tracker_endpoint: String,
    /// Linear team key filter (only relevant when tracker_kind == "linear").
    pub tracker_team: Option<String>,
    /// Linear assignee filter (raw config value; resolved by the tracker at boot).
    pub tracker_assignee: Option<String>,
    /// Linear delegate/app agent filter (raw config value; resolved by the tracker at boot).
    pub tracker_delegate: Option<String>,
    /// Plane bot display name filter (resolved by the tracker at boot).
    pub tracker_mention: Option<String>,
    /// Linear label filters (OR within; empty = unconstrained).
    pub tracker_labels: Vec<String>,
    // Polling
    pub poll_interval_ms: u64,
    pub poll_jitter_ms: u64,
    pub max_concurrent: usize,
    pub max_active_runs: u32,
    pub max_retries: u32,
    pub retry_backoff_ms: u64,
    /// Whether a WORKFLOW.md parse error on live-reload keeps the stale snapshot.
    #[allow(dead_code)]
    pub allow_stale: bool,
    // Workspace (relative to agent root)
    pub workspace_root: PathBuf,
    pub workspace_reuse: bool,
    pub cleanup_on_terminal: bool,
    // Runner / model
    pub runner_kind: String,
    pub runner_command: String,
    /// Model identifier passed to the runner (None = runner default).
    pub model: Option<String>,
    /// Model provider passed to the runner (None = runner default).
    pub provider: Option<String>,
    /// Canonical reasoning level passed to the runner (None = runner default).
    /// Validated against the runner's supported subset; see `thinking` module.
    pub thinking: Option<String>,
    pub max_run_timeout_ms: u64,
    pub stall_timeout_ms: u64,
    /// Max turns a turn-capable run may take before the orchestrator finishes it
    /// (the turn-loop backstop).
    pub max_turns: u32,
    // Dashboard
    pub dashboard_bind: IpAddr,
    pub dashboard_port: u16,
    pub webhook_secret: Option<String>,
    // Extension points (parsed; not yet acted on in v0)
    #[allow(dead_code)]
    pub hooks: WfHooksConfig,
    #[allow(dead_code)]
    pub linear: WfLinearConfig,
}

impl WfTrackerConfig {
    /// Active states, flat form beating the legacy nested `states.active`.
    fn resolved_active_states(&self) -> Vec<String> {
        self.active_states
            .clone()
            .or_else(|| self.states.as_ref().and_then(|s| s.active.clone()))
            .unwrap_or_default()
    }

    /// Terminal states, flat form beating the legacy nested `states.terminal`.
    fn resolved_terminal_states(&self) -> Vec<String> {
        self.terminal_states
            .clone()
            .or_else(|| self.states.as_ref().and_then(|s| s.terminal.clone()))
            .unwrap_or_default()
    }

    /// needs-human state, flat form beating the legacy nested form. No default:
    /// absent means the orchestrator has no dedicated parking state.
    fn resolved_needs_human(&self) -> Option<String> {
        self.needs_human
            .clone()
            .or_else(|| self.states.as_ref().and_then(|s| s.needs_human.clone()))
    }
}

impl WorkflowFrontmatter {
    /// Validate the frontmatter carries a runnable loop config: a tracker
    /// `kind` plus non-empty active and terminal state lists. Called by the
    /// orchestrator at boot and by `doctor`; the error names the missing
    /// frontmatter key so operators can fix WORKFLOW.md directly.
    pub fn validate_loop(&self) -> Result<()> {
        let Some(t) = self.tracker.as_ref() else {
            bail!("WORKFLOW.md frontmatter is missing the `tracker` section (needs `tracker.kind`, `tracker.active_states`, `tracker.terminal_states`)");
        };
        if t.kind.as_deref().map(str::trim).unwrap_or("").is_empty() {
            bail!("WORKFLOW.md frontmatter `tracker.kind` is required (e.g. files, linear, plane)");
        }
        if t.resolved_active_states().is_empty() {
            bail!("WORKFLOW.md frontmatter `tracker.active_states` must be non-empty");
        }
        if t.resolved_terminal_states().is_empty() {
            bail!("WORKFLOW.md frontmatter `tracker.terminal_states` must be non-empty");
        }
        if let Some(polling) = &self.polling {
            if polling.interval_ms == Some(0) {
                bail!("WORKFLOW.md frontmatter `polling.interval_ms` must be > 0");
            }
            if polling.max_concurrent == Some(0) {
                bail!("WORKFLOW.md frontmatter `polling.max_concurrent` must be > 0");
            }
        }
        if let Some(agent) = &self.agent {
            if agent.max_run_timeout_ms == Some(0) {
                bail!("WORKFLOW.md frontmatter `agent.max_run_timeout_ms` must be > 0");
            }
            if agent.turn_timeout_ms == Some(0) {
                bail!("WORKFLOW.md frontmatter `agent.turn_timeout_ms` must be > 0");
            }
            if agent.stall_timeout_ms == Some(0) {
                bail!("WORKFLOW.md frontmatter `agent.stall_timeout_ms` must be > 0");
            }
            if agent.max_turns == Some(0) {
                bail!("WORKFLOW.md frontmatter `agent.max_turns` must be > 0");
            }
        }
        Ok(())
    }
}

impl EffectiveLoopConfig {
    /// Resolve the loop config. WORKFLOW.md frontmatter is the SOLE source of
    /// tracker/polling/workspace config (with the inline defaults documented on
    /// each field). `agent` contributes only runner/dashboard config; the
    /// frontmatter `agent:` section still overrides runner fields — that
    /// precedence is unchanged.
    pub fn resolve(agent: &AgentConfig, wf: &WorkflowFrontmatter) -> Self {
        let t = wf.tracker.as_ref();

        // --- Tracker (frontmatter is the sole source) ---
        let tracker_kind = t.and_then(|t| t.kind.clone()).unwrap_or_default();
        let active_states = t
            .map(WfTrackerConfig::resolved_active_states)
            .unwrap_or_default();
        let terminal_states = t
            .map(WfTrackerConfig::resolved_terminal_states)
            .unwrap_or_default();
        let needs_human = t.and_then(WfTrackerConfig::resolved_needs_human);
        let tracker_projects = t
            .and_then(|t| t.projects.as_ref().map(StringOrVec::to_vec))
            .unwrap_or_default();
        let tracker_config_path = t.and_then(|t| t.path.clone());
        let tracker_workspace = t.and_then(|t| t.workspace.clone());
        let tracker_endpoint = t
            .and_then(|t| t.endpoint.clone())
            .unwrap_or_else(|| DEFAULT_LINEAR_ENDPOINT.to_string());
        let tracker_team = t.and_then(|t| t.team.clone());
        let tracker_assignee = t.and_then(|t| t.assignee.clone());
        let tracker_delegate = t.and_then(|t| t.delegate.clone());
        let tracker_mention = t.and_then(|t| t.mention.clone());
        let tracker_labels = t
            .and_then(|t| t.label.as_ref().map(StringOrVec::to_vec))
            .unwrap_or_default();

        // --- Polling ---
        let p = wf.polling.as_ref();
        let poll_interval_ms = p
            .and_then(|p| p.interval_ms)
            .unwrap_or(DEFAULT_POLL_INTERVAL_MS);
        let poll_jitter_ms = p.and_then(|p| p.jitter_ms).unwrap_or(0);
        let max_concurrent = p
            .and_then(|p| p.max_concurrent)
            .unwrap_or(DEFAULT_MAX_CONCURRENT);
        let max_active_runs = wf
            .agent
            .as_ref()
            .and_then(|a| a.max_active_runs)
            .unwrap_or(DEFAULT_MAX_ACTIVE_RUNS);
        let max_retries = p.and_then(|p| p.max_retries).unwrap_or(DEFAULT_MAX_RETRIES);
        let retry_backoff_ms = p
            .and_then(|p| p.retry_backoff_ms)
            .unwrap_or(DEFAULT_RETRY_BACKOFF_MS);
        let allow_stale = p.and_then(|p| p.allow_stale).unwrap_or(true);

        // --- Workspace ---
        let workspace_root = wf
            .workspace
            .as_ref()
            .and_then(|w| w.root.clone())
            .unwrap_or_else(|| PathBuf::from(DEFAULT_WORKSPACE_ROOT));
        let workspace_reuse = wf.workspace.as_ref().and_then(|w| w.reuse).unwrap_or(true);
        let cleanup_on_terminal = wf
            .workspace
            .as_ref()
            .and_then(|w| w.cleanup_on_terminal)
            .unwrap_or(false);

        // --- Runner / model (agent.yaml base + frontmatter `agent:` override) ---
        let a = wf.agent.as_ref();
        let runner_override = a.and_then(|a| a.effective_runner());
        let runner_kind = runner_override.map(str::to_string).unwrap_or_else(|| {
            if agent.runner.use_.trim().is_empty() {
                DEFAULT_RUNNER_KIND.to_string()
            } else {
                agent.runner.use_.clone()
            }
        });
        let runner_command = a
            .and_then(|a| a.command.as_deref())
            .map(str::to_string)
            .unwrap_or_else(|| {
                if runner_override.is_some() {
                    String::new()
                } else {
                    agent.runner.command.clone()
                }
            });
        let model = a
            .and_then(|a| a.model.clone())
            .or_else(|| agent.runner.model.clone());
        let provider = a
            .and_then(|a| a.provider.clone())
            .or_else(|| agent.runner.provider.clone());
        // Canonical reasoning level: WORKFLOW.md `agent.thinking`/`agent.effort`
        // overrides agent.yaml `runner.thinking`/`runner.effort`.
        let thinking = a
            .and_then(|a| a.thinking.clone())
            .or_else(|| agent.runner.thinking.clone());
        let max_run_timeout_ms = a
            .and_then(|a| a.turn_timeout_ms.or(a.max_run_timeout_ms))
            .unwrap_or(agent.runner.max_run_timeout_ms);
        let stall_timeout_ms = a
            .and_then(|a| a.stall_timeout_ms)
            .unwrap_or(agent.runner.stall_timeout_ms);
        let max_turns = a
            .and_then(|a| a.max_turns)
            .unwrap_or(agent.runner.max_turns);

        // --- Dashboard ---
        let dashboard_bind = wf
            .server
            .as_ref()
            .and_then(|s| s.bind)
            .unwrap_or(agent.dashboard.bind);
        let dashboard_port = wf
            .server
            .as_ref()
            .and_then(|s| s.port)
            .unwrap_or(agent.dashboard.port);
        let webhook_secret = wf
            .linear
            .as_ref()
            .and_then(|l| l.webhook_secret.clone())
            .or_else(|| agent.dashboard.webhook_secret.clone());

        Self {
            tracker_kind,
            active_states,
            terminal_states,
            needs_human,
            tracker_projects,
            tracker_config_path,
            tracker_workspace,
            tracker_endpoint,
            tracker_team,
            tracker_assignee,
            tracker_delegate,
            tracker_mention,
            tracker_labels,
            poll_interval_ms,
            poll_jitter_ms,
            max_concurrent,
            max_active_runs,
            max_retries,
            retry_backoff_ms,
            allow_stale,
            workspace_root,
            workspace_reuse,
            cleanup_on_terminal,
            runner_kind,
            runner_command,
            model,
            provider,
            thinking,
            max_run_timeout_ms,
            stall_timeout_ms,
            max_turns,
            dashboard_bind,
            dashboard_port,
            webhook_secret,
            hooks: wf.hooks.clone().unwrap_or_default(),
            linear: wf.linear.clone().unwrap_or_default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentConfig, DashboardConfig, HitlConfig, RunnerConfig};
    use std::net::Ipv4Addr;

    /// Identity + runner + dashboard only. Loop config now lives entirely in
    /// WORKFLOW.md frontmatter, so the base agent config carries none of it.
    fn base_config() -> AgentConfig {
        AgentConfig {
            id: "test".into(),
            name: "Test".into(),
            runner: RunnerConfig {
                use_: "fake".into(),
                command: "fake".into(),
                model: None,
                provider: None,
                thinking: None,
                max_run_timeout_ms: 1_800_000,
                stall_timeout_ms: 300_000,
                max_turns: 20,
            },
            hitl: HitlConfig::default(),
            dashboard: DashboardConfig {
                bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 7878,
                webhook_secret: None,
            },
            foreground: "logs".to_string(),
            providers: Default::default(),
            extensions: Default::default(),
            system_files: None,
        }
    }

    // --- parse_workflow_md ---

    #[test]
    fn parse_no_frontmatter() {
        let snap = parse_workflow_md("Hello {{ issue.title }}").unwrap();
        assert!(snap.frontmatter.tracker.is_none());
        assert_eq!(snap.body, "Hello {{ issue.title }}");
    }

    #[test]
    fn parse_empty_frontmatter() {
        let raw = "---\n---\nBody here";
        let snap = parse_workflow_md(raw).unwrap();
        assert!(snap.frontmatter.tracker.is_none());
        assert_eq!(snap.body, "Body here");
    }

    #[test]
    fn parse_tracker_flat() {
        let raw = "---\ntracker:\n  active_states: [wip]\n  terminal_states: [done]\n  needs_human: stuck\n---\nbody";
        let snap = parse_workflow_md(raw).unwrap();
        let tc = snap.frontmatter.tracker.unwrap();
        assert_eq!(tc.active_states, Some(vec!["wip".into()]));
        assert_eq!(tc.terminal_states, Some(vec!["done".into()]));
        assert_eq!(tc.needs_human, Some("stuck".into()));
        assert_eq!(snap.body, "body");
    }

    #[test]
    fn parse_tracker_legacy_nested() {
        let raw =
            "---\ntracker:\n  states:\n    active: [todo]\n    terminal: [closed]\n    needs_human: blocked\n---\nbody";
        let snap = parse_workflow_md(raw).unwrap();
        let tc = snap.frontmatter.tracker.unwrap();
        assert!(tc.active_states.is_none(), "flat form absent");
        let states = tc.states.unwrap();
        assert_eq!(states.active, Some(vec!["todo".into()]));
        assert_eq!(states.terminal, Some(vec!["closed".into()]));
        assert_eq!(states.needs_human, Some("blocked".into()));
    }

    #[test]
    fn parse_all_sections() {
        let raw = r#"---
tracker:
  active_states: [open]
  terminal_states: [closed]
polling:
  interval_ms: 5000
  jitter_ms: 250
  max_concurrent: 2
  allow_stale: false
workspace:
  root: ./ws
  reuse: false
  cleanup_on_terminal: true
agent:
  sdk: fake
  command: fake
  model: fake-model
  max_run_timeout_ms: 900000
hooks:
  after_create: ./after-create.sh
  before_run: ./before-run.sh
  after_run: ./after-run.sh
  before_remove: ./before-remove.sh
server:
  port: 9090
linear:
  project: my-project
  team: eng
  worker_tool: true
---
body"#;
        let snap = parse_workflow_md(raw).unwrap();
        let fm = &snap.frontmatter;
        assert_eq!(
            fm.tracker.as_ref().unwrap().active_states,
            Some(vec!["open".into()])
        );
        assert_eq!(fm.polling.as_ref().unwrap().interval_ms, Some(5000));
        assert_eq!(fm.polling.as_ref().unwrap().jitter_ms, Some(250));
        assert_eq!(fm.polling.as_ref().unwrap().allow_stale, Some(false));
        assert_eq!(
            fm.workspace.as_ref().unwrap().root,
            Some(PathBuf::from("./ws"))
        );
        assert_eq!(fm.workspace.as_ref().unwrap().reuse, Some(false));
        assert_eq!(
            fm.workspace.as_ref().unwrap().cleanup_on_terminal,
            Some(true)
        );
        assert_eq!(fm.agent.as_ref().unwrap().sdk, Some("fake".into()));
        assert_eq!(fm.agent.as_ref().unwrap().model, Some("fake-model".into()));
        assert_eq!(
            fm.hooks.as_ref().unwrap().after_create,
            Some("./after-create.sh".into())
        );
        assert_eq!(
            fm.hooks.as_ref().unwrap().before_run,
            Some("./before-run.sh".into())
        );
        assert_eq!(
            fm.hooks.as_ref().unwrap().after_run,
            Some("./after-run.sh".into())
        );
        assert_eq!(
            fm.hooks.as_ref().unwrap().before_remove,
            Some("./before-remove.sh".into())
        );
        assert_eq!(fm.server.as_ref().unwrap().port, Some(9090));
        assert_eq!(
            fm.linear.as_ref().unwrap().project,
            Some("my-project".into())
        );
        assert_eq!(snap.body, "body");
    }

    #[test]
    fn parse_linear_expose_graphql_tool_alias() {
        let snap = parse_workflow_md("---\nlinear:\n  exposeGraphqlTool: true\n---\nbody").unwrap();
        assert_eq!(snap.frontmatter.linear.unwrap().worker_tool, Some(true));
    }

    // --- EffectiveLoopConfig::resolve ---

    /// No frontmatter ⇒ loop config falls to the inline defaults (no agent.yaml
    /// trio to fall back to), while runner/dashboard come from agent.yaml.
    #[test]
    fn resolve_no_frontmatter_uses_defaults() {
        let base = base_config();
        let wf = WorkflowFrontmatter::default();
        let eff = EffectiveLoopConfig::resolve(&base, &wf);
        assert!(eff.active_states.is_empty());
        assert!(eff.terminal_states.is_empty());
        assert_eq!(eff.needs_human, None);
        assert_eq!(eff.poll_interval_ms, 1000);
        assert_eq!(eff.max_concurrent, 3);
        assert_eq!(eff.max_active_runs, 3);
        assert_eq!(eff.max_retries, 3);
        assert_eq!(eff.retry_backoff_ms, 30_000);
        assert_eq!(eff.poll_jitter_ms, 0);
        assert_eq!(eff.workspace_root, PathBuf::from("workspaces"));
        assert_eq!(eff.runner_kind, "fake");
        assert_eq!(eff.runner_command, "fake");
        assert_eq!(eff.model, None);
        assert!(eff.allow_stale);
        assert!(eff.workspace_reuse);
        assert!(!eff.cleanup_on_terminal);
        assert_eq!(eff.webhook_secret, None);
    }

    /// Tracker projects: a scalar `projects` yields a single-element list.
    #[test]
    fn resolve_tracker_projects_scalar() {
        let base = base_config();
        let snap = parse_workflow_md("---\ntracker:\n  projects: alpha\n---").unwrap();
        let eff = EffectiveLoopConfig::resolve(&base, &snap.frontmatter);
        assert_eq!(eff.tracker_projects, vec!["alpha"]);
    }

    /// Tracker projects: a list `projects` is preserved in order.
    #[test]
    fn resolve_tracker_projects_list() {
        let base = base_config();
        let snap = parse_workflow_md("---\ntracker:\n  projects: [alpha, beta]\n---").unwrap();
        let eff = EffectiveLoopConfig::resolve(&base, &snap.frontmatter);
        assert_eq!(eff.tracker_projects, vec!["alpha", "beta"]);
    }

    /// `tracker.path` (files-tracker issues dir) flows into the resolved config.
    #[test]
    fn resolve_tracker_path() {
        let base = base_config();
        let snap = parse_workflow_md("---\ntracker:\n  path: my-issues\n---").unwrap();
        let eff = EffectiveLoopConfig::resolve(&base, &snap.frontmatter);
        assert_eq!(eff.tracker_config_path, Some(PathBuf::from("my-issues")));
    }

    /// `validate_loop` accepts a complete tracker section and rejects each
    /// missing piece with a message naming the frontmatter key.
    #[test]
    fn validate_loop_reports_missing_pieces() {
        let ok = parse_workflow_md(
            "---\ntracker:\n  kind: files\n  active_states: [todo]\n  terminal_states: [done]\n---",
        )
        .unwrap();
        ok.frontmatter.validate_loop().unwrap();

        let no_tracker = WorkflowFrontmatter::default();
        let err = no_tracker.validate_loop().unwrap_err().to_string();
        assert!(err.contains("tracker"), "{err}");

        let no_kind = parse_workflow_md(
            "---\ntracker:\n  active_states: [todo]\n  terminal_states: [done]\n---",
        )
        .unwrap();
        let err = no_kind.frontmatter.validate_loop().unwrap_err().to_string();
        assert!(err.contains("tracker.kind"), "{err}");

        let no_active =
            parse_workflow_md("---\ntracker:\n  kind: files\n  terminal_states: [done]\n---")
                .unwrap();
        let err = no_active
            .frontmatter
            .validate_loop()
            .unwrap_err()
            .to_string();
        assert!(err.contains("tracker.active_states"), "{err}");

        let no_terminal =
            parse_workflow_md("---\ntracker:\n  kind: files\n  active_states: [todo]\n---")
                .unwrap();
        let err = no_terminal
            .frontmatter
            .validate_loop()
            .unwrap_err()
            .to_string();
        assert!(err.contains("tracker.terminal_states"), "{err}");
    }

    #[test]
    fn validate_loop_rejects_zero_runtime_limits() {
        let cases = [
            ("polling:\n  interval_ms: 0", "polling.interval_ms"),
            ("polling:\n  max_concurrent: 0", "polling.max_concurrent"),
            (
                "agent:\n  max_run_timeout_ms: 0",
                "agent.max_run_timeout_ms",
            ),
            ("agent:\n  turn_timeout_ms: 0", "agent.turn_timeout_ms"),
            ("agent:\n  stall_timeout_ms: 0", "agent.stall_timeout_ms"),
            ("agent:\n  max_turns: 0", "agent.max_turns"),
        ];

        for (section, field) in cases {
            let raw = format!(
                "---\ntracker:\n  kind: files\n  active_states: [todo]\n  terminal_states: [done]\n{section}\n---"
            );
            let workflow = parse_workflow_md(&raw).unwrap();
            let error = workflow
                .frontmatter
                .validate_loop()
                .unwrap_err()
                .to_string();
            assert!(error.contains(field), "{field}: {error}");
        }
    }

    #[test]
    fn validate_loop_accepts_positive_runtime_limits() {
        let workflow = parse_workflow_md(
            "---\ntracker:\n  kind: files\n  active_states: [todo]\n  terminal_states: [done]\npolling:\n  interval_ms: 1\n  max_concurrent: 1\nagent:\n  max_run_timeout_ms: 1\n  turn_timeout_ms: 1\n  stall_timeout_ms: 1\n  max_turns: 1\n---",
        )
        .unwrap();

        workflow.frontmatter.validate_loop().unwrap();
    }

    #[test]
    fn merge_webhook_secret_falls_back_to_agent_yaml() {
        let mut base = base_config();
        base.dashboard.webhook_secret = Some("agent-secret".to_string());

        let eff = EffectiveLoopConfig::resolve(&base, &WorkflowFrontmatter::default());

        assert_eq!(eff.webhook_secret, Some("agent-secret".to_string()));
    }

    #[test]
    fn merge_webhook_secret_frontmatter_overrides_agent_yaml() {
        let mut base = base_config();
        base.dashboard.webhook_secret = Some("agent-secret".to_string());
        let raw = "---\nlinear:\n  webhook_secret: workflow-secret\n---";
        let snap = parse_workflow_md(raw).unwrap();

        let eff = EffectiveLoopConfig::resolve(&base, &snap.frontmatter);

        assert_eq!(eff.webhook_secret, Some("workflow-secret".to_string()));
    }

    #[test]
    fn merge_workspace_policy_overrides_defaults() {
        let base = base_config();
        let raw =
            "---\nworkspace:\n  root: ./custom\n  reuse: false\n  cleanup_on_terminal: true\n---";
        let snap = parse_workflow_md(raw).unwrap();
        let eff = EffectiveLoopConfig::resolve(&base, &snap.frontmatter);
        assert_eq!(eff.workspace_root, PathBuf::from("./custom"));
        assert!(!eff.workspace_reuse);
        assert!(eff.cleanup_on_terminal);
    }

    #[test]
    fn merge_tracker_flat_overrides_base() {
        let base = base_config();
        let raw = "---\ntracker:\n  active_states: [wip]\n  terminal_states: [closed]\n  needs_human: blocked\n---";
        let snap = parse_workflow_md(raw).unwrap();
        let eff = EffectiveLoopConfig::resolve(&base, &snap.frontmatter);
        assert_eq!(eff.active_states, vec!["wip"]);
        assert_eq!(eff.terminal_states, vec!["closed"]);
        assert_eq!(eff.needs_human, Some("blocked".into()));
    }

    #[test]
    fn merge_tracker_legacy_nested_overrides_base() {
        let base = base_config();
        let raw =
            "---\ntracker:\n  states:\n    active: [pending]\n    terminal: [resolved]\n    needs_human: waiting\n---";
        let snap = parse_workflow_md(raw).unwrap();
        let eff = EffectiveLoopConfig::resolve(&base, &snap.frontmatter);
        assert_eq!(eff.active_states, vec!["pending"]);
        assert_eq!(eff.terminal_states, vec!["resolved"]);
        assert_eq!(eff.needs_human, Some("waiting".into()));
    }

    #[test]
    fn merge_flat_beats_nested_when_both_present() {
        let base = base_config();
        let raw = "---\ntracker:\n  active_states: [flat]\n  states:\n    active: [nested]\n---";
        let snap = parse_workflow_md(raw).unwrap();
        let eff = EffectiveLoopConfig::resolve(&base, &snap.frontmatter);
        assert_eq!(eff.active_states, vec!["flat"]);
    }

    #[test]
    fn resolve_partial_tracker_leaves_absent_states_empty() {
        let base = base_config();
        // Only active_states present; terminal_states has no agent.yaml
        // fallback anymore, so it stays empty (validate_loop would reject it).
        let raw = "---\ntracker:\n  active_states: [open]\n---";
        let snap = parse_workflow_md(raw).unwrap();
        let eff = EffectiveLoopConfig::resolve(&base, &snap.frontmatter);
        assert_eq!(eff.active_states, vec!["open"]);
        assert!(eff.terminal_states.is_empty());
    }

    #[test]
    fn merge_runner_sdk_overrides_base() {
        let base = base_config();
        let raw = "---\nagent:\n  sdk: gemini-code\n  command: gemini\n  model: gemini-2.0\n---";
        let snap = parse_workflow_md(raw).unwrap();
        let eff = EffectiveLoopConfig::resolve(&base, &snap.frontmatter);
        assert_eq!(eff.runner_kind, "gemini-code");
        assert_eq!(eff.runner_command, "gemini");
        assert_eq!(eff.model, Some("gemini-2.0".into()));
    }

    #[test]
    fn unknown_runner_without_command_falls_back_to_base_command() {
        // WORKFLOW.md names an unknown runner but omits `command:`.
        // The base agent.yaml command should be used, not "pi".
        let base = base_config(); // base.runner.command = "fake"
        let raw = "---\nagent:\n  sdk: gemini-code\n---";
        let snap = parse_workflow_md(raw).unwrap();
        let eff = EffectiveLoopConfig::resolve(&base, &snap.frontmatter);
        assert_eq!(eff.runner_kind, "gemini-code");
        assert_eq!(eff.runner_command, "");
    }

    #[test]
    fn merge_runner_alias_falls_back() {
        let base = base_config();
        // `runner` alias (no sdk) should be picked up.
        let raw = "---\nagent:\n  runner: cli\n---";
        let snap = parse_workflow_md(raw).unwrap();
        let eff = EffectiveLoopConfig::resolve(&base, &snap.frontmatter);
        assert_eq!(eff.runner_kind, "cli");
        assert_eq!(eff.runner_command, "");
    }

    #[test]
    fn merge_runner_kind_beats_sdk_and_turn_timeout_alias_wins() {
        let base = base_config();
        let raw = "---\nagent:\n  sdk: fake\n  kind: codex\n  turn_timeout_ms: 2500\n  max_run_timeout_ms: 9000\n---";
        let snap = parse_workflow_md(raw).unwrap();
        let eff = EffectiveLoopConfig::resolve(&base, &snap.frontmatter);
        assert_eq!(eff.runner_kind, "codex");
        assert_eq!(eff.runner_command, "");
        assert_eq!(eff.max_run_timeout_ms, 2500);
        assert_eq!(eff.stall_timeout_ms, 300_000);
    }

    #[test]
    fn merge_agent_stall_timeout_overrides_base() {
        let base = base_config();
        let raw = "---\nagent:\n  stall_timeout_ms: 12345\n---";
        let snap = parse_workflow_md(raw).unwrap();
        let eff = EffectiveLoopConfig::resolve(&base, &snap.frontmatter);
        assert_eq!(eff.stall_timeout_ms, 12_345);
    }

    #[test]
    fn merge_allow_stale_false() {
        let base = base_config();
        let raw = "---\npolling:\n  allow_stale: false\n---";
        let snap = parse_workflow_md(raw).unwrap();
        let eff = EffectiveLoopConfig::resolve(&base, &snap.frontmatter);
        assert!(!eff.allow_stale);
    }

    #[test]
    fn parse_allow_stale_camel_case_alias() {
        // `allowStale` (camelCase) must deserialize the same as `allow_stale`.
        let raw = "---\npolling:\n  allowStale: false\n---\nbody";
        let snap = parse_workflow_md(raw).unwrap();
        assert_eq!(
            snap.frontmatter.polling.as_ref().unwrap().allow_stale,
            Some(false),
            "allowStale alias must deserialize to allow_stale=false"
        );
    }

    #[test]
    fn parse_poll_interval_ms_alias() {
        // `poll_interval_ms` (agent.yaml-style) must deserialize as `interval_ms`.
        let raw = "---\npolling:\n  poll_interval_ms: 10000\n---\nbody";
        let snap = parse_workflow_md(raw).unwrap();
        assert_eq!(
            snap.frontmatter.polling.as_ref().unwrap().interval_ms,
            Some(10_000),
            "poll_interval_ms alias must deserialize to interval_ms"
        );
    }

    #[test]
    fn parse_unknown_polling_key_is_ignored_not_fatal() {
        let raw = "---\npolling:\n  not_a_real_key: 5\n  interval_ms: 2000\n---\nbody";
        let snap = parse_workflow_md(raw).unwrap();
        assert_eq!(
            snap.frontmatter.polling.as_ref().unwrap().interval_ms,
            Some(2000)
        );
    }

    #[test]
    fn merge_allow_stale_camel_case() {
        let base = base_config();
        let raw = "---\npolling:\n  allowStale: false\n---";
        let snap = parse_workflow_md(raw).unwrap();
        let eff = EffectiveLoopConfig::resolve(&base, &snap.frontmatter);
        assert!(
            !eff.allow_stale,
            "allowStale camelCase should map to allow_stale=false"
        );
    }

    #[test]
    fn merge_needs_human_from_flat() {
        let base = base_config();
        let raw = "---\ntracker:\n  needs_human: needs-review\n---";
        let snap = parse_workflow_md(raw).unwrap();
        let eff = EffectiveLoopConfig::resolve(&base, &snap.frontmatter);
        assert_eq!(eff.needs_human, Some("needs-review".into()));
    }

    #[test]
    fn merge_needs_human_from_legacy_nested() {
        let base = base_config();
        let raw = "---\ntracker:\n  states:\n    needs_human: waiting-for-human\n---";
        let snap = parse_workflow_md(raw).unwrap();
        let eff = EffectiveLoopConfig::resolve(&base, &snap.frontmatter);
        assert_eq!(eff.needs_human, Some("waiting-for-human".into()));
    }

    #[test]
    fn resolve_needs_human_absent_is_none() {
        let base = base_config();
        let wf = WorkflowFrontmatter::default();
        let eff = EffectiveLoopConfig::resolve(&base, &wf);
        assert_eq!(eff.needs_human, None);
    }

    #[test]
    fn agent_yaml_sdk_field_is_used_as_runner_kind() {
        use std::net::Ipv4Addr;
        let mut base = base_config();
        base.runner = RunnerConfig {
            use_: "pi".into(),
            command: String::new(),
            model: None,
            provider: None,
            thinking: None,
            max_run_timeout_ms: 3_600_000,
            stall_timeout_ms: 300_000,
            max_turns: 20,
        };
        // sdk field in agent.yaml should map to runner kind via the `use_` alias
        let wf = WorkflowFrontmatter::default();
        let eff = EffectiveLoopConfig::resolve(&base, &wf);
        assert_eq!(eff.runner_kind, "pi");
        assert_eq!(eff.model, None);
        let _ = Ipv4Addr::LOCALHOST; // keep import used
    }

    #[test]
    fn agent_yaml_model_falls_back_when_workflow_absent() {
        let mut base = base_config();
        base.runner.model = Some("fake-model".into());
        let wf = WorkflowFrontmatter::default();
        let eff = EffectiveLoopConfig::resolve(&base, &wf);
        assert_eq!(eff.model, Some("fake-model".into()));
    }

    #[test]
    fn thinking_falls_back_to_agent_yaml_runner_thinking() {
        let mut base = base_config();
        base.runner.thinking = Some("medium".into());
        let eff = EffectiveLoopConfig::resolve(&base, &WorkflowFrontmatter::default());
        assert_eq!(eff.thinking, Some("medium".into()));
    }

    #[test]
    fn workflow_thinking_overrides_agent_yaml() {
        let mut base = base_config();
        base.runner.thinking = Some("low".into());
        let raw = "---\nagent:\n  thinking: high\n---";
        let snap = parse_workflow_md(raw).unwrap();
        let eff = EffectiveLoopConfig::resolve(&base, &snap.frontmatter);
        assert_eq!(eff.thinking, Some("high".into()));
    }

    #[test]
    fn workflow_effort_alias_is_accepted_as_thinking() {
        let base = base_config();
        let raw = "---\nagent:\n  effort: medium\n---";
        let snap = parse_workflow_md(raw).unwrap();
        let eff = EffectiveLoopConfig::resolve(&base, &snap.frontmatter);
        assert_eq!(eff.thinking, Some("medium".into()));
    }

    #[test]
    fn agent_yaml_effort_alias_parses_as_thinking() {
        let raw = "id: a\nname: A\nrunner:\n  use: pi\n  effort: high\ndashboard:\n  bind: 127.0.0.1\n  port: 7878\n";
        let cfg: AgentConfig = serde_yaml::from_str(raw).unwrap();
        assert_eq!(cfg.runner.thinking, Some("high".into()));
    }

    #[test]
    fn thinking_absent_yields_none() {
        let base = base_config();
        let eff = EffectiveLoopConfig::resolve(&base, &WorkflowFrontmatter::default());
        assert_eq!(eff.thinking, None);
    }

    #[test]
    fn resolve_tracker_dimensions_from_frontmatter() {
        let base = base_config();
        let raw = "---\ntracker:\n  team: ALG\n  assignee: \"@thinh\"\n  delegate: \"@workeragent\"\n  label: [bug, urgent]\n---";
        let snap = parse_workflow_md(raw).unwrap();
        let eff = EffectiveLoopConfig::resolve(&base, &snap.frontmatter);
        assert_eq!(eff.tracker_team, Some("ALG".into()));
        assert_eq!(eff.tracker_assignee, Some("@thinh".into()));
        assert_eq!(eff.tracker_delegate, Some("@workeragent".into()));
        assert_eq!(eff.tracker_labels, vec!["bug", "urgent"]);
    }

    #[test]
    fn resolve_tracker_label_scalar_in_frontmatter() {
        let base = base_config();
        let raw = "---\ntracker:\n  label: bug\n---";
        let snap = parse_workflow_md(raw).unwrap();
        let eff = EffectiveLoopConfig::resolve(&base, &snap.frontmatter);
        assert_eq!(eff.tracker_labels, vec!["bug"]);
    }

    #[test]
    fn resolve_tracker_dimensions_absent_are_empty_or_none() {
        let base = base_config();
        let eff = EffectiveLoopConfig::resolve(&base, &WorkflowFrontmatter::default());
        assert_eq!(eff.tracker_team, None);
        assert_eq!(eff.tracker_assignee, None);
        assert_eq!(eff.tracker_delegate, None);
        assert!(eff.tracker_labels.is_empty());
    }

    #[test]
    fn workflow_model_overrides_agent_yaml_model() {
        let mut base = base_config();
        base.runner.model = Some("base-model".into());
        let raw = "---\nagent:\n  model: override-model\n---";
        let snap = parse_workflow_md(raw).unwrap();
        let eff = EffectiveLoopConfig::resolve(&base, &snap.frontmatter);
        assert_eq!(eff.model, Some("override-model".into()));
    }
}
