//! WORKFLOW.md frontmatter structs, parser, and effective-loop-config merge.
//!
//! WORKFLOW.md = optional YAML frontmatter + Markdown prompt body.
//! Frontmatter sections: tracker / polling / workspace / agent / hooks / server / linear.
//!
//! The effective loop config is produced by merging agent.yaml (base) with the
//! WORKFLOW.md frontmatter (override): every field present in WORKFLOW.md wins;
//! absent fields fall back to the corresponding agent.yaml value.
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

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::config::AgentConfig;

const DEFAULT_NEEDS_HUMAN_STATE: &str = "Needs Human";

// ---------------------------------------------------------------------------
// Frontmatter structs
// ---------------------------------------------------------------------------

/// All sections of the WORKFLOW.md YAML frontmatter.
#[derive(Debug, Clone, Default, Deserialize)]
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
#[derive(Debug, Clone, Deserialize)]
pub struct WfTrackerConfig {
    // --- Flat form (preferred) ---
    pub active_states: Option<Vec<String>>,
    pub terminal_states: Option<Vec<String>>,
    pub needs_human: Option<String>,
    // --- Legacy nested form ---
    pub states: Option<WfTrackerStates>,
    // --- Tracker kind + Linear-specific ---
    /// `"files"` or `"linear"`. Overrides agent.yaml `tracker.use`.
    pub kind: Option<String>,
    /// Linear project slugId to scope issue polling.
    pub project_slug: Option<String>,
    /// Linear GraphQL endpoint override (default `https://api.linear.app/graphql`).
    pub endpoint: Option<String>,
}

/// Legacy `tracker.states.*` nesting.
#[derive(Debug, Clone, Deserialize)]
pub struct WfTrackerStates {
    pub active: Option<Vec<String>>,
    pub terminal: Option<Vec<String>>,
    pub needs_human: Option<String>,
}

/// Polling / scheduling overrides from WORKFLOW.md.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct WfPollingConfig {
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
#[derive(Debug, Clone, Deserialize)]
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
#[derive(Debug, Clone, Deserialize)]
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
    /// Per-attempt timeout override (ms).
    pub max_run_timeout_ms: Option<u64>,
    pub turn_timeout_ms: Option<u64>,
    pub max_active_runs: Option<u32>,
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
#[derive(Debug, Clone, Default, Deserialize)]
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
#[derive(Debug, Clone, Deserialize)]
pub struct WfServerConfig {
    pub bind: Option<IpAddr>,
    pub port: Option<u16>,
}

/// Linear integration settings from WORKFLOW.md (parsed; active use is a future concern).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct WfLinearConfig {
    pub project: Option<String>,
    pub team: Option<String>,
    /// Enable the linear_graphql worker tool when the child runs.
    pub worker_tool: Option<bool>,
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
        Some(fm) => serde_yaml::from_str(fm).context("parsing WORKFLOW.md frontmatter YAML")?,
        None => WorkflowFrontmatter::default(),
    };
    Ok(WorkflowSnapshot {
        frontmatter,
        body: body.to_string(),
    })
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

/// The merged config the orchestrator uses for its loop logic.
///
/// Derived at startup (and re-derived on every successful WORKFLOW.md reload)
/// by layering WORKFLOW.md frontmatter over agent.yaml: frontmatter values win
/// for every field they specify; absent fields fall back to agent.yaml.
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
    /// Linear project slugId (only relevant when tracker_kind == "linear").
    pub tracker_project_slug: Option<String>,
    /// Linear GraphQL endpoint (only relevant when tracker_kind == "linear").
    pub tracker_endpoint: String,
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
    pub max_run_timeout_ms: u64,
    // Dashboard
    pub dashboard_bind: IpAddr,
    pub dashboard_port: u16,
    // Extension points (parsed; not yet acted on in v0)
    #[allow(dead_code)]
    pub hooks: WfHooksConfig,
    #[allow(dead_code)]
    pub linear: WfLinearConfig,
}

impl EffectiveLoopConfig {
    /// Build by layering WORKFLOW.md frontmatter over agent.yaml base config.
    /// Every field present in `wf` wins; absent fields fall back to `base`.
    pub fn merge(base: &AgentConfig, wf: &WorkflowFrontmatter) -> Self {
        // --- Tracker states + kind ---
        let (active_states, terminal_states, needs_human) = resolve_tracker(base, wf);
        let tracker_kind = wf
            .tracker
            .as_ref()
            .and_then(|t| t.kind.as_deref())
            .map(|s| s.to_string())
            .unwrap_or_else(|| base.tracker.use_.clone());
        let tracker_project_slug = wf
            .tracker
            .as_ref()
            .and_then(|t| t.project_slug.clone())
            .or_else(|| base.tracker.project_slug.clone());
        let tracker_endpoint = wf
            .tracker
            .as_ref()
            .and_then(|t| t.endpoint.clone())
            .or_else(|| base.tracker.endpoint.clone())
            .unwrap_or_else(|| "https://api.linear.app/graphql".to_string());

        // --- Polling ---
        let p = wf.polling.as_ref();
        let poll_interval_ms = p
            .and_then(|p| p.interval_ms)
            .unwrap_or(base.orchestrator.poll_interval_ms);
        let poll_jitter_ms = p.and_then(|p| p.jitter_ms).unwrap_or(0);
        let max_concurrent = p
            .and_then(|p| p.max_concurrent)
            .unwrap_or(base.orchestrator.max_concurrent);
        let max_active_runs = wf
            .agent
            .as_ref()
            .and_then(|a| a.max_active_runs)
            .unwrap_or(base.orchestrator.max_active_runs);
        let max_retries = p
            .and_then(|p| p.max_retries)
            .unwrap_or(base.orchestrator.max_retries);
        let retry_backoff_ms = p
            .and_then(|p| p.retry_backoff_ms)
            .unwrap_or(base.orchestrator.retry_backoff_ms);
        let allow_stale = p.and_then(|p| p.allow_stale).unwrap_or(true);

        // --- Workspace ---
        let workspace_root = wf
            .workspace
            .as_ref()
            .and_then(|w| w.root.clone())
            .unwrap_or_else(|| base.workspace.root.clone());
        let workspace_reuse = wf.workspace.as_ref().and_then(|w| w.reuse).unwrap_or(true);
        let cleanup_on_terminal = wf
            .workspace
            .as_ref()
            .and_then(|w| w.cleanup_on_terminal)
            .unwrap_or(false);

        // --- Runner / model ---
        let a = wf.agent.as_ref();
        let runner_override = a.and_then(|a| a.effective_runner());
        let runner_kind = runner_override
            .map(str::to_string)
            .unwrap_or_else(|| normalize_runner_kind(&base.runner.use_));
        let runner_command = a
            .and_then(|a| a.command.as_deref())
            .map(str::to_string)
            .unwrap_or_else(|| {
                if runner_override.is_some() {
                    // Use the well-known default for built-in runners; for custom
                    // runner kinds fall back to base.runner.command so the operator's
                    // configured command is honoured.
                    default_runner_command(&runner_kind)
                        .map(str::to_string)
                        .unwrap_or_else(|| base.runner.command.clone())
                } else {
                    base.runner.command.clone()
                }
            });
        let model = a
            .and_then(|a| a.model.clone())
            .or_else(|| base.runner.model.clone());
        let max_run_timeout_ms = a
            .and_then(|a| a.turn_timeout_ms.or(a.max_run_timeout_ms))
            .unwrap_or(base.runner.max_run_timeout_ms);

        // --- Dashboard ---
        let dashboard_bind = wf
            .server
            .as_ref()
            .and_then(|s| s.bind)
            .unwrap_or(base.dashboard.bind);
        let dashboard_port = wf
            .server
            .as_ref()
            .and_then(|s| s.port)
            .unwrap_or(base.dashboard.port);

        Self {
            tracker_kind,
            active_states,
            terminal_states,
            needs_human,
            tracker_project_slug,
            tracker_endpoint,
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
            max_run_timeout_ms,
            dashboard_bind,
            dashboard_port,
            hooks: wf.hooks.clone().unwrap_or_default(),
            linear: wf.linear.clone().unwrap_or_default(),
        }
    }
}

fn normalize_runner_kind(kind: &str) -> String {
    match kind {
        "claude-code" => "claude".to_string(),
        "" => "pi".to_string(),
        other => other.to_string(),
    }
}

/// Return the well-known default command for a built-in runner kind, or `None`
/// for custom/unrecognised kinds (caller should fall back to base command).
fn default_runner_command(kind: &str) -> Option<&'static str> {
    match kind {
        "pi" | "" => Some("pi"),
        "claude" | "claude-code" => Some("claude"),
        "codex" => Some("codex"),
        "cli" | "fake" => Some("sh"),
        _ => None,
    }
}

/// Resolve tracker state lists from WORKFLOW.md (with flat→nested fallback) or
/// fall back to agent.yaml values.
fn resolve_tracker(
    base: &AgentConfig,
    wf: &WorkflowFrontmatter,
) -> (Vec<String>, Vec<String>, Option<String>) {
    match &wf.tracker {
        None => (
            base.tracker.active_states.clone(),
            base.tracker.terminal_states.clone(),
            base.tracker
                .needs_human
                .clone()
                .or_else(|| Some(DEFAULT_NEEDS_HUMAN_STATE.to_string())),
        ),
        Some(tc) => {
            // Flat fields beat the legacy nested form.
            let active = tc
                .active_states
                .clone()
                .or_else(|| tc.states.as_ref().and_then(|s| s.active.clone()))
                .unwrap_or_else(|| base.tracker.active_states.clone());

            let terminal = tc
                .terminal_states
                .clone()
                .or_else(|| tc.states.as_ref().and_then(|s| s.terminal.clone()))
                .unwrap_or_else(|| base.tracker.terminal_states.clone());

            let needs_human = tc
                .needs_human
                .clone()
                .or_else(|| tc.states.as_ref().and_then(|s| s.needs_human.clone()))
                .or_else(|| base.tracker.needs_human.clone())
                .or_else(|| Some(DEFAULT_NEEDS_HUMAN_STATE.to_string()));

            (active, terminal, needs_human)
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AgentConfig, DashboardConfig, HitlConfig, OrchestratorConfig, RunnerConfig, TrackerConfig,
        TrackerInner, WorkspaceConfig,
    };
    use std::net::Ipv4Addr;

    fn base_config() -> AgentConfig {
        AgentConfig {
            id: "test".into(),
            name: "Test".into(),
            tracker: TrackerConfig {
                use_: "files".into(),
                config: Some(TrackerInner {
                    path: "./issues".into(),
                }),
                active_states: vec!["todo".into()],
                terminal_states: vec!["done".into()],
                project_slug: None,
                endpoint: None,
                needs_human: None,
            },
            runner: RunnerConfig {
                use_: "claude-code".into(),
                command: "claude".into(),
                model: None,
                max_run_timeout_ms: 1_800_000,
            },
            orchestrator: OrchestratorConfig {
                poll_interval_ms: 10_000,
                max_concurrent: 1,
                max_active_runs: 3,
                max_retries: 3,
                retry_backoff_ms: 10_000,
            },
            hitl: HitlConfig::default(),
            workspace: WorkspaceConfig {
                root: "./workspaces".into(),
            },
            dashboard: DashboardConfig {
                bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 7878,
            },
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
  sdk: claude-code
  command: claude
  model: claude-opus-4-6
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
        assert_eq!(fm.agent.as_ref().unwrap().sdk, Some("claude-code".into()));
        assert_eq!(
            fm.agent.as_ref().unwrap().model,
            Some("claude-opus-4-6".into())
        );
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

    // --- EffectiveLoopConfig::merge ---

    #[test]
    fn merge_no_frontmatter_uses_base() {
        let base = base_config();
        let wf = WorkflowFrontmatter::default();
        let eff = EffectiveLoopConfig::merge(&base, &wf);
        assert_eq!(eff.active_states, vec!["todo"]);
        assert_eq!(eff.terminal_states, vec!["done"]);
        assert_eq!(eff.needs_human, Some("Needs Human".into()));
        assert_eq!(eff.poll_interval_ms, 10_000);
        assert_eq!(eff.poll_jitter_ms, 0);
        assert_eq!(eff.runner_kind, "claude");
        assert_eq!(eff.runner_command, "claude");
        assert_eq!(eff.model, None);
        assert!(eff.allow_stale);
        assert!(eff.workspace_reuse);
        assert!(!eff.cleanup_on_terminal);
    }

    #[test]
    fn merge_workspace_policy_overrides_defaults() {
        let base = base_config();
        let raw =
            "---\nworkspace:\n  root: ./custom\n  reuse: false\n  cleanup_on_terminal: true\n---";
        let snap = parse_workflow_md(raw).unwrap();
        let eff = EffectiveLoopConfig::merge(&base, &snap.frontmatter);
        assert_eq!(eff.workspace_root, PathBuf::from("./custom"));
        assert!(!eff.workspace_reuse);
        assert!(eff.cleanup_on_terminal);
    }

    #[test]
    fn merge_tracker_flat_overrides_base() {
        let base = base_config();
        let raw = "---\ntracker:\n  active_states: [wip]\n  terminal_states: [closed]\n  needs_human: blocked\n---";
        let snap = parse_workflow_md(raw).unwrap();
        let eff = EffectiveLoopConfig::merge(&base, &snap.frontmatter);
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
        let eff = EffectiveLoopConfig::merge(&base, &snap.frontmatter);
        assert_eq!(eff.active_states, vec!["pending"]);
        assert_eq!(eff.terminal_states, vec!["resolved"]);
        assert_eq!(eff.needs_human, Some("waiting".into()));
    }

    #[test]
    fn merge_flat_beats_nested_when_both_present() {
        let base = base_config();
        let raw = "---\ntracker:\n  active_states: [flat]\n  states:\n    active: [nested]\n---";
        let snap = parse_workflow_md(raw).unwrap();
        let eff = EffectiveLoopConfig::merge(&base, &snap.frontmatter);
        assert_eq!(eff.active_states, vec!["flat"]);
    }

    #[test]
    fn merge_partial_tracker_falls_back_to_base() {
        let base = base_config();
        // Only active_states overridden; terminal_states falls back.
        let raw = "---\ntracker:\n  active_states: [open]\n---";
        let snap = parse_workflow_md(raw).unwrap();
        let eff = EffectiveLoopConfig::merge(&base, &snap.frontmatter);
        assert_eq!(eff.active_states, vec!["open"]);
        assert_eq!(eff.terminal_states, vec!["done"]); // from base
    }

    #[test]
    fn merge_runner_sdk_overrides_base() {
        let base = base_config();
        let raw = "---\nagent:\n  sdk: gemini-code\n  command: gemini\n  model: gemini-2.0\n---";
        let snap = parse_workflow_md(raw).unwrap();
        let eff = EffectiveLoopConfig::merge(&base, &snap.frontmatter);
        assert_eq!(eff.runner_kind, "gemini-code");
        assert_eq!(eff.runner_command, "gemini");
        assert_eq!(eff.model, Some("gemini-2.0".into()));
    }

    #[test]
    fn unknown_runner_without_command_falls_back_to_base_command() {
        // WORKFLOW.md names an unknown runner but omits `command:`.
        // The base agent.yaml command should be used, not "pi".
        let base = base_config(); // base.runner.command = "claude"
        let raw = "---\nagent:\n  sdk: gemini-code\n---";
        let snap = parse_workflow_md(raw).unwrap();
        let eff = EffectiveLoopConfig::merge(&base, &snap.frontmatter);
        assert_eq!(eff.runner_kind, "gemini-code");
        assert_eq!(eff.runner_command, "claude"); // falls back to base, not "pi"
    }

    #[test]
    fn merge_runner_alias_falls_back() {
        let base = base_config();
        // `runner` alias (no sdk) should be picked up.
        let raw = "---\nagent:\n  runner: cli\n---";
        let snap = parse_workflow_md(raw).unwrap();
        let eff = EffectiveLoopConfig::merge(&base, &snap.frontmatter);
        assert_eq!(eff.runner_kind, "cli");
        // Runner override uses that runner's default command instead of the
        // base runner command.
        assert_eq!(eff.runner_command, "sh");
    }

    #[test]
    fn merge_runner_kind_beats_sdk_and_turn_timeout_alias_wins() {
        let base = base_config();
        let raw = "---\nagent:\n  sdk: claude\n  kind: codex\n  turn_timeout_ms: 2500\n  max_run_timeout_ms: 9000\n---";
        let snap = parse_workflow_md(raw).unwrap();
        let eff = EffectiveLoopConfig::merge(&base, &snap.frontmatter);
        assert_eq!(eff.runner_kind, "codex");
        assert_eq!(eff.runner_command, "codex");
        assert_eq!(eff.max_run_timeout_ms, 2500);
    }

    #[test]
    fn merge_allow_stale_false() {
        let base = base_config();
        let raw = "---\npolling:\n  allow_stale: false\n---";
        let snap = parse_workflow_md(raw).unwrap();
        let eff = EffectiveLoopConfig::merge(&base, &snap.frontmatter);
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
    fn merge_allow_stale_camel_case() {
        let base = base_config();
        let raw = "---\npolling:\n  allowStale: false\n---";
        let snap = parse_workflow_md(raw).unwrap();
        let eff = EffectiveLoopConfig::merge(&base, &snap.frontmatter);
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
        let eff = EffectiveLoopConfig::merge(&base, &snap.frontmatter);
        assert_eq!(eff.needs_human, Some("needs-review".into()));
    }

    #[test]
    fn merge_needs_human_from_legacy_nested() {
        let base = base_config();
        let raw = "---\ntracker:\n  states:\n    needs_human: waiting-for-human\n---";
        let snap = parse_workflow_md(raw).unwrap();
        let eff = EffectiveLoopConfig::merge(&base, &snap.frontmatter);
        assert_eq!(eff.needs_human, Some("waiting-for-human".into()));
    }

    #[test]
    fn merge_needs_human_absent_defaults_to_needs_human() {
        let base = base_config();
        let wf = WorkflowFrontmatter::default();
        let eff = EffectiveLoopConfig::merge(&base, &wf);
        assert_eq!(eff.needs_human, Some("Needs Human".into()));
    }

    #[test]
    fn agent_yaml_sdk_field_is_used_as_runner_kind() {
        use std::net::Ipv4Addr;
        let mut base = base_config();
        base.runner = RunnerConfig {
            use_: "pi".into(),
            command: String::new(),
            model: None,
            max_run_timeout_ms: 3_600_000,
        };
        // sdk field in agent.yaml should map to runner kind via the `use_` alias
        let wf = WorkflowFrontmatter::default();
        let eff = EffectiveLoopConfig::merge(&base, &wf);
        assert_eq!(eff.runner_kind, "pi");
        assert_eq!(eff.model, None);
        let _ = Ipv4Addr::LOCALHOST; // keep import used
    }

    #[test]
    fn agent_yaml_model_falls_back_when_workflow_absent() {
        let mut base = base_config();
        base.runner.model = Some("claude-opus-4-6".into());
        let wf = WorkflowFrontmatter::default();
        let eff = EffectiveLoopConfig::merge(&base, &wf);
        assert_eq!(eff.model, Some("claude-opus-4-6".into()));
    }

    #[test]
    fn workflow_model_overrides_agent_yaml_model() {
        let mut base = base_config();
        base.runner.model = Some("base-model".into());
        let raw = "---\nagent:\n  model: override-model\n---";
        let snap = parse_workflow_md(raw).unwrap();
        let eff = EffectiveLoopConfig::merge(&base, &snap.frontmatter);
        assert_eq!(eff.model, Some("override-model".into()));
    }
}
