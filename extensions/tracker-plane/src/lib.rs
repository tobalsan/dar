//! `PlaneTracker`: polls Plane's REST API scoped to one workspace + project.
//!
//! Plane models a work item's state as a UUID and exposes human-readable state
//! *names* only via the project's state list, so the tracker resolves the state
//! table once at boot and maps `state_id` → name for the portable [`Issue`].
//!
//! Auth (section 6): `PLANE_BOT_TOKEN` / `PLANE_OAUTH_TOKEN` (sent as
//! `Authorization: Bearer <token>`) take precedence over `PLANE_API_KEY` (sent
//! as the `X-API-Key` header).
//!
//! Rate-limit handling (section 5): reads `X-RateLimit-Remaining` /
//! `X-RateLimit-Reset` on every response. Unlike Linear, Plane's reset header is
//! a **duration in seconds** (seconds until the bucket refills), not an epoch, so
//! it is used directly as the sleep length. On HTTP 429 the `Retry-After` header
//! (or 60 s) is honoured with a single retry. The minimum remaining seen feeds
//! the dashboard RATE LIMIT stat.
//!
//! Blocked-issue skipping (section 4 + AMENDMENT): candidacy is derived from the
//! work item's **relations** (`GET .../work-items/{id}/relations/`), which return
//! a JSON object grouped by relation type. The `blocked_by` group lists related
//! work item UUIDs; a candidate is skipped when any blocker is a non-terminal
//! work item in the same project.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use cap_tracker::{Issue, Tracker, TrackerBuildConfig, TrackerFactory};
use chrono::Utc;
use host_api::{Extension, HostCommand, RegisterCtx};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tool_registry::{ToolRegistryHandle, TOOL_REGISTRY_SERVICE};

mod plane_api;

/// Default Plane cloud REST API base. Self-hosted deployments override via
/// `extensions.tracker-plane.api_url`.
pub(crate) const DEFAULT_API_URL: &str = "https://api.plane.so";
/// Default Plane cloud web app base, used to build shareable issue URLs.
pub(crate) const DEFAULT_APP_URL: &str = "https://app.plane.so";
/// Env var holding a Plane bot/OAuth access token. Sent as `Authorization:
/// Bearer <token>`. Takes precedence over `PLANE_API_KEY` when set.
pub(crate) const BOT_TOKEN_ENV: &str = "PLANE_BOT_TOKEN";
/// Legacy/alternate env var for a Plane OAuth access token. Sent as
/// `Authorization: Bearer <token>`.
pub(crate) const OAUTH_TOKEN_ENV: &str = "PLANE_OAUTH_TOKEN";
/// Env var holding a Plane personal API key. Sent as the `X-API-Key` header.
pub(crate) const API_KEY_ENV: &str = "PLANE_API_KEY";

/// Initial sentinel: no real rate-limit observation yet.
const UNSET_MIN: i64 = i64::MAX;
/// Page size for cursor pagination.
const PAGE_SIZE: &str = "100";

fn default_api_url() -> String {
    DEFAULT_API_URL.to_string()
}

fn default_app_url() -> String {
    DEFAULT_APP_URL.to_string()
}

// ---------------------------------------------------------------------------
// Extension config (`extensions.tracker-plane`)
// ---------------------------------------------------------------------------

/// Per-extension config for `extensions.tracker-plane`. Holds the Plane scoping
/// (`workspace` slug + `project` UUID) and the API/app base URLs. Captured by
/// [`PlaneTrackerFactory`] at register time and shared with the (part 2)
/// `plane_api` host tool via the accessors below.
#[derive(Debug, Clone, Deserialize)]
pub struct PlaneExtConfig {
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub project: String,
    #[serde(default = "default_api_url")]
    pub api_url: String,
    #[serde(default = "default_app_url")]
    pub app_url: String,
}

impl Default for PlaneExtConfig {
    fn default() -> Self {
        Self {
            workspace: String::new(),
            project: String::new(),
            api_url: default_api_url(),
            app_url: default_app_url(),
        }
    }
}

impl PlaneExtConfig {
    /// Workspace slug (e.g. `"acme"`).
    pub fn workspace(&self) -> &str {
        &self.workspace
    }
    /// Project UUID.
    pub fn project(&self) -> &str {
        &self.project
    }
    /// REST API base URL (default [`DEFAULT_API_URL`]).
    pub fn api_url(&self) -> &str {
        &self.api_url
    }
    /// Web app base URL, used to build shareable issue URLs (default
    /// [`DEFAULT_APP_URL`]).
    pub fn app_url(&self) -> &str {
        &self.app_url
    }
}

/// Parse the `extensions.tracker-plane` block. Missing/invalid config yields the
/// default (empty workspace/project, default URLs); required-field validation is
/// deferred to [`PlaneTrackerFactory::build`] so linking the extension without
/// configuring it never fails boot.
pub(crate) fn parse_ext_config(config: Option<&Value>) -> PlaneExtConfig {
    config
        .and_then(|v| serde_json::from_value::<PlaneExtConfig>(v.clone()).ok())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Auth (section 6)
// ---------------------------------------------------------------------------

/// A resolved Plane credential. Plane accepts either an OAuth access token
/// (`Authorization: Bearer <token>`) or a personal API key (`X-API-Key`).
#[derive(Debug, Clone)]
pub(crate) enum PlaneAuth {
    Bearer(String),
    ApiKey(String),
}

impl PlaneAuth {
    /// The `(header_name, header_value)` pair to send for this credential.
    fn header(&self) -> (&'static str, String) {
        match self {
            PlaneAuth::Bearer(t) => ("Authorization", format!("Bearer {t}")),
            PlaneAuth::ApiKey(k) => ("X-API-Key", k.clone()),
        }
    }
}

/// Resolve the Plane credential from the environment. Bearer bot/OAuth tokens
/// take precedence over `PLANE_API_KEY` (`X-API-Key`). Returns `None` when no
/// supported env var is set (or all are empty).
pub(crate) fn resolve_plane_auth() -> Option<PlaneAuth> {
    for env in [BOT_TOKEN_ENV, OAUTH_TOKEN_ENV] {
        if let Some(token) = std::env::var(env).ok().filter(|t| !t.is_empty()) {
            return Some(PlaneAuth::Bearer(token));
        }
    }
    std::env::var(API_KEY_ENV)
        .ok()
        .filter(|k| !k.is_empty())
        .map(PlaneAuth::ApiKey)
}

// ---------------------------------------------------------------------------
// Extension
// ---------------------------------------------------------------------------

pub struct TrackerPlaneExtension;

impl Extension for TrackerPlaneExtension {
    fn id(&self) -> &'static str {
        "tracker-plane"
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let config = parse_ext_config(ctx.config.get(self.id()));
            let factory: Arc<dyn TrackerFactory> = Arc::new(PlaneTrackerFactory::new(config));
            ctx.services
                .service::<dyn TrackerFactory>("plane", factory)?;

            // Section 8: the init-workflow / export CLI commands, namespaced by
            // tracker so they coexist with tracker-linear's `init-workflow` /
            // `export` in a composition that links both extensions.
            ctx.services
                .service::<dyn HostCommand>("init-workflow.plane", Arc::new(InitWorkflowCommand))?;
            ctx.services
                .service::<dyn HostCommand>("export.plane", Arc::new(ExportCommand))?;

            // Section 7: register the `plane_api` host tool against the shared
            // registry, if one is published. The registry is owned by the
            // tool-registry-host extension and is always present in the stock
            // composition; resolve it leniently so a stripped composition
            // without the registry still boots the tracker.
            if let Ok(registry) = ctx
                .services
                .get_named::<dyn ToolRegistryHandle>(TOOL_REGISTRY_SERVICE)
            {
                let base = plane_api::plane_api_base(ctx.config.get(self.id()));
                plane_api::register_into(registry.as_ref(), base)?;
            }
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// CLI commands (section 8): init-workflow.plane / export.plane
// ---------------------------------------------------------------------------

struct InitWorkflowCommand;

#[derive(Debug, Deserialize)]
struct InitWorkflowCommandArgs {
    dir: PathBuf,
    force: bool,
    #[serde(default)]
    plane_workspace: Option<String>,
    #[serde(default)]
    plane_project: Option<String>,
    #[serde(default)]
    expose_api_tool: bool,
}

impl HostCommand for InitWorkflowCommand {
    fn run(&self, args: Value) -> Result<()> {
        let args: InitWorkflowCommandArgs =
            serde_json::from_value(args).context("parsing init-workflow args")?;
        init_workflow_with_options(
            &args.dir,
            args.force,
            args.plane_workspace.as_deref(),
            args.plane_project.as_deref(),
            args.expose_api_tool,
        )
    }
}

struct ExportCommand;

#[derive(Debug, Deserialize)]
struct ExportCommandArgs {
    dir: PathBuf,
}

impl HostCommand for ExportCommand {
    fn run(&self, args: Value) -> Result<()> {
        let args: ExportCommandArgs =
            serde_json::from_value(args).context("parsing export args")?;
        let result = export_plane_project_from_root(&args.dir)?;
        println!(
            "exported {} issues to {}",
            result.issue_count,
            result.dir.display()
        );
        Ok(())
    }
}

/// The default `WORKFLOW.md` body (section 8.3). Prompt-level worker guidance in
/// Plane terms. The Dependencies section reflects the AMENDMENT: the daemon
/// already auto-skips work items whose `blocked_by` relations include an
/// incomplete blocker, and the worker double-checks relations mid-run via the
/// `plane_api` tool.
pub const DEFAULT_WORKFLOW_MD_BODY: &str = r#"You are working on issue {{ issue.identifier }}: {{ issue.title }}

{{ issue.description }}

These instructions are prompt-level worker guidance. They describe what you must do; they are not daemon or orchestrator behavior.

## Required Claim Step

Before doing task work:

1. Fetch the Plane work item {{ issue.identifier }} with the `plane_api` tool.
2. If its current state is `Todo`, move it to `In Progress`.
3. Read all Plane work item comments and incorporate any updated requirements.
4. Add or update one concise Plane comment saying you are working on the work item.
5. Continue only after those Plane updates succeed.

Keep that same Plane comment updated with progress, validation results, blockers, and the final handoff. Do not create a noisy comment stream.

## Dependencies

The daemon already skips any work item whose `blocked_by` relations include a non-terminal blocker, so a dispatched work item's blockers were clear at dispatch time. Dependency state can still change while you work, so double-check the relations mid-run: use the `plane_api` tool to `GET workspaces/{workspace}/projects/{project}/work-items/{id}/relations/` and confirm each `blocked_by` work item is in a terminal/completed state such as `Done`, `Closed`, or `Cancelled`. If a blocker has regressed to an incomplete state, update the Plane comment with the blocker and stop without coding.

For completed blockers, read their comments for prior workspace, branch, commit, and PR notes. If a completed dependency has an available workspace or branch, base your work on it so changes stack instead of diverging.

When this work item is a sub-item of a parent that has other sub-items, stack on already-resolved sibling work. Use one PR per parent: if a PR already exists for the parent, push your work onto that branch and update that PR; if no PR exists yet, create one.

## Workspace

Work only inside this issue workspace. If repositories or extra checkouts are needed, clone or create them inside this workspace unless they already exist here.

For code changes, create a git worktree from the correct base branch: a completed dependency's branch/workspace when available, otherwise the repository's main branch unless the work item says otherwise.

## Review And PR Flow

When code changes are needed:

1. Make the focused change in the issue worktree.
2. Spawn a reviewer subagent and ask it to review the code changes.
3. Do not commit until the reviewer comes back clean.
4. After a clean review, commit the work in the worktree.
5. Create or update the GitHub PR using `gh`.
6. Link the PR to the Plane work item.
7. Move the work item to `In Review`.

## Git Usage

Always use non-interactive git. Never run a git (or any) command that opens an editor or waits on a TTY/stdin — it will hang the run until the stall guard parks it. Use `git commit -m "..."` or `git commit --no-edit`; for rebases, run `git rebase --continue` only after staging changes (the environment already neutralizes `GIT_EDITOR`/`GIT_SEQUENCE_EDITOR` so it will not block on a message editor). Avoid `git commit --amend` without `--no-edit`, interactive rebase (`git rebase -i`), and any command that pages output or prompts for credentials.

## Blockers

If requirements, ownership, base branch, dependency state, credentials, or validation risk are unclear, ask for human input instead of guessing. Update the Plane comment with the blocker, what you tried, and the decision needed, then move the work item to `Needs Human` and stop.

## Completion

Validate the change before handoff. When the task is complete, leave the work item out of active states: move it to `In Review` when work is done and a PR is open or updated, or to a terminal state only when the workflow explicitly calls for it."#;

pub fn init_workflow(root: &Path, force: bool) -> Result<()> {
    init_workflow_with_options(root, force, None, None, false)
}

pub fn init_workflow_with_options(
    root: &Path,
    force: bool,
    plane_workspace: Option<&str>,
    plane_project: Option<&str>,
    expose_api_tool: bool,
) -> Result<()> {
    let path = root.join("WORKFLOW.md");
    if path.exists() && !force {
        bail!(
            "{} already exists; pass --force to overwrite it",
            path.display()
        );
    }
    let body = workflow_body_with_frontmatter(plane_workspace, plane_project, expose_api_tool);
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    // `dar init-build` owns the full agent `.gitignore`. We still ensure `.env`
    // here so a standalone `init_workflow` never leaves secrets un-ignored; the
    // helper is idempotent, so init-build will not duplicate this entry.
    ensure_gitignore_entry(root, ".env")?;
    println!("wrote {}", path.display());
    Ok(())
}

fn ensure_gitignore_entry(root: &Path, entry: &str) -> Result<()> {
    let path = root.join(".gitignore");
    let existing = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if existing.lines().any(|line| line.trim() == entry) {
        return Ok(());
    }

    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(entry);
    next.push('\n');
    std::fs::write(&path, next).with_context(|| format!("writing {}", path.display()))
}

fn workflow_body_with_frontmatter(
    plane_workspace: Option<&str>,
    plane_project: Option<&str>,
    expose_api_tool: bool,
) -> String {
    if plane_workspace.is_none() && plane_project.is_none() && !expose_api_tool {
        return format!("{DEFAULT_WORKFLOW_MD_BODY}\n");
    }

    let mut out = String::from("---\n");
    if plane_workspace.is_some() || plane_project.is_some() {
        out.push_str("tracker:\n");
        out.push_str("  kind: plane\n");
        if let Some(workspace) = plane_workspace {
            out.push_str(&format!("  workspace: {}\n", yaml_string(workspace)));
        }
        if let Some(project) = plane_project {
            out.push_str(&format!("  project: {}\n", yaml_string(project)));
        }
    }
    if expose_api_tool {
        out.push_str("plane:\n");
        out.push_str("  exposeApiTool: true\n");
    }
    out.push_str("---\n\n");
    out.push_str(DEFAULT_WORKFLOW_MD_BODY);
    out.push('\n');
    out
}

fn yaml_string(value: &str) -> String {
    serde_yaml::to_string(value)
        .unwrap_or_else(|_| format!("{value:?}"))
        .trim()
        .to_string()
}

// ---------------------------------------------------------------------------
// Export (section 8.2)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct PlaneProjectExport {
    pub name: Option<String>,
    pub identifier: String,
    pub workspace: String,
    pub api_url: String,
    pub exported_at: chrono::DateTime<chrono::Utc>,
    pub issue_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlaneExport {
    pub project: PlaneProjectExport,
    pub issues: Vec<Issue>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExportResult {
    pub dir: PathBuf,
    pub project_path: PathBuf,
    pub issues_path: PathBuf,
    pub issue_count: usize,
}

#[derive(Debug, Deserialize)]
struct ExportAgentConfig {
    id: String,
    name: String,
    tracker: ExportTrackerConfig,
    #[serde(default)]
    extensions: ExportExtensions,
}

#[derive(Debug, Default, Deserialize)]
struct ExportExtensions {
    #[serde(rename = "tracker-plane", default)]
    tracker_plane: Option<PlaneExtConfig>,
}

#[derive(Debug, Default, Deserialize)]
struct ExportWorkflowFrontmatter {
    tracker: Option<ExportWorkflowTracker>,
    #[serde(default)]
    extensions: ExportExtensions,
}

#[derive(Debug, Default, Deserialize)]
struct ExportWorkflowTracker {
    #[serde(default, alias = "kind")]
    use_: Option<String>,
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    needs_human: Option<String>,
    #[serde(default)]
    active_states: Option<Vec<String>>,
    #[serde(default)]
    terminal_states: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
struct ExportTrackerConfig {
    #[serde(rename = "use")]
    use_: String,
    active_states: Vec<String>,
    terminal_states: Vec<String>,
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    needs_human: Option<String>,
}

pub fn export_plane_project_from_root(root: &Path) -> Result<ExportResult> {
    let mut agent_cfg: ExportAgentConfig =
        serde_yaml::from_str(&std::fs::read_to_string(root.join("agent.yaml"))?)
            .context("parsing agent.yaml for Plane export")?;
    let workflow = load_workflow_frontmatter(&root.join("WORKFLOW.md"))?;
    merge_export_workflow(&mut agent_cfg, workflow);
    export_plane_project(root, &agent_cfg)
}

fn merge_export_workflow(agent_cfg: &mut ExportAgentConfig, workflow: ExportWorkflowFrontmatter) {
    if let Some(tracker) = workflow.tracker {
        if let Some(use_) = tracker.use_ {
            agent_cfg.tracker.use_ = use_;
        }
        if tracker.endpoint.is_some() {
            agent_cfg.tracker.endpoint = tracker.endpoint;
        }
        if tracker.workspace.is_some() {
            agent_cfg.tracker.workspace = tracker.workspace;
        }
        if tracker.project.is_some() {
            agent_cfg.tracker.project = tracker.project;
        }
        if tracker.needs_human.is_some() {
            agent_cfg.tracker.needs_human = tracker.needs_human;
        }
        if let Some(active_states) = tracker.active_states {
            agent_cfg.tracker.active_states = active_states;
        }
        if let Some(terminal_states) = tracker.terminal_states {
            agent_cfg.tracker.terminal_states = terminal_states;
        }
    }
    if workflow.extensions.tracker_plane.is_some() {
        agent_cfg.extensions.tracker_plane = workflow.extensions.tracker_plane;
    }
}

fn load_workflow_frontmatter(path: &Path) -> Result<ExportWorkflowFrontmatter> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ExportWorkflowFrontmatter::default());
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let (frontmatter, _) = split_frontmatter(&raw);
    match frontmatter {
        Some(src) => serde_yaml::from_str(src).context("parsing WORKFLOW.md frontmatter"),
        None => Ok(ExportWorkflowFrontmatter::default()),
    }
}

fn split_frontmatter(src: &str) -> (Option<&str>, &str) {
    let rest = match src.strip_prefix("---\n") {
        Some(rest) => rest,
        None => match src.strip_prefix("---\r\n") {
            Some(rest) => rest,
            None => return (None, src),
        },
    };

    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" {
            let body_start = offset + line.len();
            return (Some(&rest[..offset]), &rest[body_start..]);
        }
        offset += line.len();
    }
    (None, src)
}

fn export_plane_project(root: &Path, agent_cfg: &ExportAgentConfig) -> Result<ExportResult> {
    if agent_cfg.tracker.use_ != "plane" {
        bail!(
            "export requires tracker.use \"plane\" (got {:?})",
            agent_cfg.tracker.use_
        );
    }
    let ext = agent_cfg
        .extensions
        .tracker_plane
        .clone()
        .unwrap_or_default();
    let workspace = agent_cfg
        .tracker
        .workspace
        .as_deref()
        .unwrap_or(&ext.workspace)
        .trim()
        .to_string();
    if workspace.is_empty() {
        bail!("tracker.workspace is required for Plane export");
    }
    let project = agent_cfg
        .tracker
        .project
        .as_deref()
        .unwrap_or(&ext.project)
        .trim()
        .to_string();
    if project.is_empty() {
        bail!("tracker.project is required for Plane export");
    }
    let auth = resolve_plane_auth();
    if auth.is_none() {
        bail!("{BOT_TOKEN_ENV}, {OAUTH_TOKEN_ENV}, or {API_KEY_ENV} is required for Plane export");
    }

    let api_url = agent_cfg
        .tracker
        .endpoint
        .clone()
        .filter(|e| !e.is_empty())
        .filter(|e| e != "https://api.linear.app/graphql")
        .unwrap_or_else(|| ext.api_url.clone());

    let mut tracker = PlaneTracker::new(PlaneTrackerConfig {
        api_url,
        app_url: ext.app_url.clone(),
        workspace,
        project,
        auth,
        active_states: agent_cfg.tracker.active_states.clone(),
        terminal_states: agent_cfg.tracker.terminal_states.clone(),
        needs_human: agent_cfg.tracker.needs_human.clone(),
        mention: None,
    })?;
    tracker.resolve_boot()?;
    let snapshot = tracker.export_snapshot()?;
    write_snapshot(root, agent_cfg, snapshot)
}

fn write_snapshot(
    root: &Path,
    agent_cfg: &ExportAgentConfig,
    snapshot: PlaneExport,
) -> Result<ExportResult> {
    let dir = root.join("data").join("export");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let project_path = dir.join("project.json");
    let issues_path = dir.join("issues.json");
    let project = serde_json::json!({
        "agent_id": agent_cfg.id,
        "agent_name": agent_cfg.name,
        "plane_project": snapshot.project,
    });
    write_json(&project_path, &project)?;
    write_json(&issues_path, &snapshot.issues)?;

    Ok(ExportResult {
        dir,
        project_path,
        issues_path,
        issue_count: snapshot.issues.len(),
    })
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value).context("serializing export JSON")?;
    std::fs::write(path, [bytes, b"\n".to_vec()].concat())
        .with_context(|| format!("writing {}", path.display()))
}

// ---------------------------------------------------------------------------
// Factory (section 2.2 / 2.3 / 2.4)
// ---------------------------------------------------------------------------

/// Builds a [`PlaneTracker`] from the captured [`PlaneExtConfig`] merged with the
/// per-agent [`TrackerBuildConfig`] (states + needs-human). Unlike the Linear
/// factory this is not a unit struct: it carries the extension config resolved at
/// register time.
pub struct PlaneTrackerFactory {
    config: PlaneExtConfig,
}

impl PlaneTrackerFactory {
    pub fn new(config: PlaneExtConfig) -> Self {
        Self { config }
    }
}

impl TrackerFactory for PlaneTrackerFactory {
    fn build(&self, cfg: TrackerBuildConfig) -> Result<Arc<dyn Tracker>> {
        // --- config rejections (section 2.3) ---
        let workspace = cfg
            .workspace
            .as_deref()
            .unwrap_or(&self.config.workspace)
            .trim()
            .to_string();
        if workspace.is_empty() {
            bail!("tracker.workspace is required for Plane (workspace slug)");
        }
        let project = cfg
            .project
            .as_deref()
            .unwrap_or(&self.config.project)
            .trim()
            .to_string();
        validate_states(
            &cfg.active_states,
            &cfg.terminal_states,
            cfg.needs_human.as_deref(),
        )?;

        // --- auth (section 6) ---
        let auth = resolve_plane_auth();
        if auth.is_none() {
            tracing::warn!(
                "none of {BOT_TOKEN_ENV}, {OAUTH_TOKEN_ENV}, or {API_KEY_ENV} is set; Plane API requests will fail with 401"
            );
        }

        // `endpoint` from the tracker block overrides the extension api_url when
        // present (self-hosted convenience); otherwise use the extension config.
        let api_url = cfg
            .endpoint
            .clone()
            .filter(|e| !e.is_empty())
            .filter(|e| e != "https://api.linear.app/graphql")
            .unwrap_or_else(|| self.config.api_url.clone());

        let mut tracker = PlaneTracker::new(PlaneTrackerConfig {
            api_url,
            app_url: self.config.app_url.clone(),
            workspace,
            project,
            auth,
            active_states: cfg.active_states,
            terminal_states: cfg.terminal_states,
            needs_human: cfg.needs_human,
            mention: cfg.mention,
        })?;

        // --- boot resolution (section 2.4): fetch project identifier + state
        // and label tables, then fail fast if a configured state name is unknown.
        tracker.resolve_boot()?;

        Ok(Arc::new(tracker))
    }
}

/// Pure state validation (section 2.3). Enforces non-empty active/terminal sets,
/// that they are disjoint, and that a configured `needs_human` parking state is
/// not itself an active (candidate) state.
fn validate_states(
    active: &[String],
    terminal: &[String],
    needs_human: Option<&str>,
) -> Result<()> {
    if active.is_empty() {
        bail!("tracker.active_states must not be empty for the Plane tracker");
    }
    if terminal.is_empty() {
        bail!("tracker.terminal_states must not be empty for the Plane tracker");
    }
    if let Some(s) = active.iter().find(|a| terminal.iter().any(|t| t == *a)) {
        bail!("state {s:?} appears in both tracker.active_states and tracker.terminal_states");
    }
    if let Some(nh) = needs_human.filter(|s| !s.is_empty()) {
        if active.iter().any(|a| a == nh) {
            bail!("tracker.needs_human {nh:?} must not also be a tracker.active_states value");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tracker
// ---------------------------------------------------------------------------

pub struct PlaneTrackerConfig {
    pub api_url: String,
    pub app_url: String,
    pub workspace: String,
    pub project: String,
    pub(crate) auth: Option<PlaneAuth>,
    pub active_states: Vec<String>,
    pub terminal_states: Vec<String>,
    pub needs_human: Option<String>,
    pub mention: Option<String>,
}

pub struct PlaneTracker {
    client: reqwest::Client,
    api_url: String,
    app_url: String,
    workspace: String,
    project: String,
    auth: Option<PlaneAuth>,
    active: Vec<String>,
    terminal: Vec<String>,
    needs_human: Option<String>,
    mention: Option<String>,
    mention_user_id: Option<String>,
    // --- boot-resolved (section 2.4); empty until `resolve_boot` runs ---
    project_identifier: String,
    project_name: Option<String>,
    project_meta_by_id: HashMap<String, ProjectMeta>,
    state_name_by_id: HashMap<String, String>,
    state_id_by_name: HashMap<String, String>,
    label_name_by_id: HashMap<String, String>,
    /// Minimum `X-RateLimit-Remaining` observed across all requests.
    min_remaining: Arc<AtomicI64>,
}

impl PlaneTracker {
    pub fn new(cfg: PlaneTrackerConfig) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("building reqwest client for PlaneTracker")?;
        Ok(Self {
            client,
            api_url: if cfg.api_url.is_empty() {
                DEFAULT_API_URL.to_string()
            } else {
                cfg.api_url
            },
            app_url: if cfg.app_url.is_empty() {
                DEFAULT_APP_URL.to_string()
            } else {
                cfg.app_url
            },
            workspace: cfg.workspace,
            project: cfg.project,
            auth: cfg.auth,
            active: cfg.active_states,
            terminal: cfg.terminal_states,
            needs_human: cfg.needs_human,
            mention: cfg.mention,
            mention_user_id: None,
            project_identifier: String::new(),
            project_name: None,
            project_meta_by_id: HashMap::new(),
            state_name_by_id: HashMap::new(),
            state_id_by_name: HashMap::new(),
            label_name_by_id: HashMap::new(),
            min_remaining: Arc::new(AtomicI64::new(UNSET_MIN)),
        })
    }

    fn workspace_base(&self) -> String {
        format!(
            "{}/api/v1/workspaces/{}",
            self.api_url.trim_end_matches('/'),
            self.workspace
        )
    }

    fn projects_base(&self) -> String {
        self.project_base(&self.project)
    }

    fn project_base(&self, project: &str) -> String {
        format!(
            "{}/api/v1/workspaces/{}/projects/{}",
            self.api_url.trim_end_matches('/'),
            self.workspace,
            project
        )
    }

    fn map_ctx(&self, identifier_by_uuid: &HashMap<String, String>) -> MapCtx<'_> {
        MapCtx {
            state_name_by_id: &self.state_name_by_id,
            label_name_by_id: &self.label_name_by_id,
            identifier_by_uuid: identifier_by_uuid.clone(),
            project_identifier: &self.project_identifier,
            project_name: self.project_name.as_deref(),
            project_meta_by_id: &self.project_meta_by_id,
            workspace: &self.workspace,
            app_url: &self.app_url,
        }
    }

    // --- HTTP core ---

    fn authed(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let rb = rb
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::CONTENT_TYPE, "application/json");
        match &self.auth {
            Some(auth) => {
                let (name, value) = auth.header();
                rb.header(name, value)
            }
            None => rb,
        }
    }

    /// Execute one request (built fresh by `build` so a 429 retry re-sends it).
    /// Tracks rate-limit headers and honours a single `Retry-After` retry.
    async fn send_with_rate_limit_async<F>(&self, build: F) -> Result<Value>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let resp = build().send().await.context("sending Plane API request")?;

        if resp.status().as_u16() == 429 {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(60);
            tracing::warn!(
                retry_after_secs = retry_after,
                "Plane rate limited (429); sleeping before retry"
            );
            tokio::time::sleep(Duration::from_secs(retry_after)).await;

            let resp2 = build()
                .send()
                .await
                .context("sending Plane API request (retry after 429)")?;
            self.process_rate_limit_headers(&resp2.headers().clone())
                .await;
            let status = resp2.status();
            let text = resp2
                .text()
                .await
                .context("reading Plane response body (retry)")?;
            if !status.is_success() {
                bail!(
                    "Plane API returned HTTP {} after retry: {}",
                    status,
                    &text[..text.len().min(200)]
                );
            }
            return parse_json_body(&text);
        }

        let headers = resp.headers().clone();
        let status = resp.status();
        let text = resp.text().await.context("reading Plane response body")?;
        self.process_rate_limit_headers(&headers).await;
        if !status.is_success() {
            bail!(
                "Plane API returned HTTP {}: {}",
                status,
                &text[..text.len().min(200)]
            );
        }
        parse_json_body(&text)
    }

    /// Record `X-RateLimit-Remaining` into `min_remaining` and, when exhausted,
    /// sleep for the reset window. Plane's `X-RateLimit-Reset` is a **duration in
    /// seconds** (unlike Linear's epoch millis), so it is used directly.
    async fn process_rate_limit_headers(&self, headers: &reqwest::header::HeaderMap) {
        let remaining = headers
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<i64>().ok());
        let reset = headers
            .get("x-ratelimit-reset")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<i64>().ok());

        if let Some(r) = remaining {
            self.min_remaining.fetch_min(r, Ordering::SeqCst);
            if r <= 0 {
                let wait_secs = reset_wait_secs(reset);
                if wait_secs > 0 {
                    tracing::warn!(
                        wait_secs,
                        "Plane rate limit exhausted; sleeping until bucket resets"
                    );
                    tokio::time::sleep(Duration::from_secs(wait_secs)).await;
                }
            }
        }
    }

    async fn get_json(&self, url: &str, query: &[(&str, String)]) -> Result<Value> {
        self.send_with_rate_limit_async(|| self.authed(self.client.get(url).query(query)))
            .await
    }

    /// Fetch every page of a cursor-paginated Plane list endpoint, returning the
    /// flattened `results`. Non-paginated array responses are returned as-is.
    async fn fetch_all_paginated(&self, url: &str) -> Result<Vec<Value>> {
        self.fetch_all_paginated_query(url, Vec::new()).await
    }

    async fn fetch_all_paginated_query(
        &self,
        url: &str,
        extra_query: Vec<(&str, String)>,
    ) -> Result<Vec<Value>> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut query: Vec<(&str, String)> = vec![("per_page", PAGE_SIZE.to_string())];
            query.extend(extra_query.iter().cloned());
            if let Some(c) = &cursor {
                query.push(("cursor", c.clone()));
            }
            let resp = self.get_json(url, &query).await?;
            let (results, next_cursor, has_next) = extract_page(&resp);
            all.extend(results);
            if has_next {
                cursor = next_cursor;
            } else {
                break;
            }
        }
        Ok(all)
    }

    // --- boot resolution (section 2.4) ---

    fn resolve_boot(&mut self) -> Result<()> {
        self.run_async(async {
            let project_ids = if self.project.trim().is_empty() {
                self.fetch_all_paginated(&format!("{}/projects/", self.workspace_base()))
                    .await
                    .context("fetching Plane workspace projects")?
                    .into_iter()
                    .filter_map(|p| p.get("id").and_then(Value::as_str).map(str::to_string))
                    .collect::<Vec<_>>()
            } else {
                vec![self.project.clone()]
            };
            if project_ids.is_empty() {
                bail!(
                    "Plane workspace {} has no projects visible to this token",
                    self.workspace
                );
            }

            let mut project_meta_by_id = HashMap::new();
            let mut by_id = HashMap::new();
            let mut by_name = HashMap::new();
            let mut labels = HashMap::new();
            for project_id in &project_ids {
                let base = self.project_base(project_id);
                let meta = self
                    .get_json(&format!("{base}/"), &[])
                    .await
                    .with_context(|| format!("fetching Plane project metadata for {project_id}"))?;
                let identifier = meta
                    .get("identifier")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow!("Plane project {project_id} has no identifier"))?
                    .to_string();
                let name = meta.get("name").and_then(Value::as_str).map(String::from);
                project_meta_by_id.insert(
                    project_id.clone(),
                    ProjectMeta {
                        identifier: identifier.clone(),
                        name: name.clone(),
                    },
                );

                for s in self
                    .fetch_all_paginated(&format!("{base}/states/"))
                    .await
                    .with_context(|| format!("fetching Plane project states for {identifier}"))?
                {
                    if let (Some(id), Some(sname)) = (
                        s.get("id").and_then(Value::as_str),
                        s.get("name").and_then(Value::as_str),
                    ) {
                        by_id.insert(id.to_string(), sname.to_string());
                        by_name.insert(sname.to_string(), id.to_string());
                    }
                }

                for l in self
                    .fetch_all_paginated(&format!("{base}/labels/"))
                    .await
                    .with_context(|| format!("fetching Plane project labels for {identifier}"))?
                {
                    if let (Some(id), Some(lname)) = (
                        l.get("id").and_then(Value::as_str),
                        l.get("name").and_then(Value::as_str),
                    ) {
                        labels.insert(id.to_string(), lname.to_string());
                    }
                }
            }
            let first_meta = project_ids
                .first()
                .and_then(|id| project_meta_by_id.get(id))
                .cloned()
                .expect("project list checked non-empty");

            let mention_user_id = match self
                .mention
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                Some(name) => Some(self.resolve_bot_mention_user(name).await?),
                None => None,
            };

            Ok((
                first_meta.identifier,
                first_meta.name,
                project_meta_by_id,
                by_id,
                by_name,
                labels,
                mention_user_id,
            ))
        })
        .map(
            |(identifier, name, project_meta_by_id, by_id, by_name, labels, mention_user_id)| {
                self.project_identifier = identifier;
                self.project_name = name;
                self.project_meta_by_id = project_meta_by_id;
                self.state_name_by_id = by_id;
                self.state_id_by_name = by_name;
                self.label_name_by_id = labels;
                self.mention_user_id = mention_user_id;
            },
        )?;

        // Fail fast if a configured state name is not a real project state.
        let known: std::collections::BTreeSet<&str> =
            self.state_name_by_id.values().map(String::as_str).collect();
        for s in self.active.iter().chain(self.terminal.iter()) {
            if !known.contains(s.as_str()) {
                bail!(
                    "state {s:?} is not a state of Plane workspace/project {} (known: {:?})",
                    self.workspace,
                    known
                );
            }
        }
        if let Some(nh) = self.needs_human.as_deref().filter(|s| !s.is_empty()) {
            if !known.contains(nh) {
                bail!(
                    "tracker.needs_human {nh:?} is not a state of Plane workspace/project {}",
                    self.workspace
                );
            }
        }
        Ok(())
    }

    // --- async internals ---

    async fn resolve_bot_mention_user(&self, display_name: &str) -> Result<String> {
        let url = format!("{}/members/", self.workspace_base());
        let members = self
            .fetch_all_paginated_query(
                &url,
                vec![
                    ("display_name", display_name.to_string()),
                    ("is_bot", "true".to_string()),
                ],
            )
            .await
            .with_context(|| format!("resolving Plane bot mention {display_name:?}"))?;
        let matches: Vec<String> = members
            .into_iter()
            .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        match matches.as_slice() {
            [id] => Ok(id.clone()),
            [] => bail!("tracker.mention {display_name:?} did not match a Plane bot user"),
            _ => bail!("tracker.mention {display_name:?} matched multiple Plane bot users"),
        }
    }

    async fn fetch_all_work_items(&self) -> Result<Vec<RawWorkItem>> {
        self.fetch_all_work_items_with_mention_filter(true).await
    }

    async fn fetch_all_work_items_unfiltered(&self) -> Result<Vec<RawWorkItem>> {
        self.fetch_all_work_items_with_mention_filter(false).await
    }

    async fn fetch_all_work_items_with_mention_filter(
        &self,
        apply_mention_filter: bool,
    ) -> Result<Vec<RawWorkItem>> {
        let url = if self.project.trim().is_empty() {
            format!("{}/work-items/", self.workspace_base())
        } else {
            format!("{}/work-items/", self.projects_base())
        };
        let mut out = Vec::new();
        let query = if apply_mention_filter {
            self.mention_user_id
                .as_ref()
                .map(|id| vec![("pql", format!("mention = {id:?}"))])
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        for node in self.fetch_all_paginated_query(&url, query).await? {
            match serde_json::from_value::<RawWorkItem>(node) {
                Ok(item) => out.push(item),
                Err(e) => tracing::warn!("PlaneTracker: skipping unparseable work item: {e}"),
            }
        }
        Ok(out)
    }

    /// `blocked_by` related-issue UUIDs for one work item (AMENDMENT).
    async fn fetch_blocked_by_ids(&self, item: &RawWorkItem) -> Result<Vec<String>> {
        let url = format!(
            "{}/work-items/{}/relations/",
            self.project_base(&item.project_id),
            item.id
        );
        let resp = self.get_json(&url, &[]).await?;
        let relations: RawRelations =
            serde_json::from_value(resp).context("parsing Plane work-item relations response")?;
        Ok(relations.blocked_by_ids())
    }

    fn state_name(&self, state_id: &str) -> String {
        self.state_name_by_id
            .get(state_id)
            .cloned()
            .unwrap_or_else(|| state_id.to_string())
    }

    async fn do_poll_candidates(&self) -> Result<Vec<Issue>> {
        let raw = self.fetch_all_work_items().await?;

        // UUID → state name and UUID → identifier over the whole project so a
        // blocker in any state can be resolved. Blockers absent from the map are
        // outside this project and treated as not blocking.
        let state_by_uuid: HashMap<String, String> = raw
            .iter()
            .map(|r| (r.id.clone(), self.state_name(&r.state_id)))
            .collect();
        let identifier_by_uuid: HashMap<String, String> = raw
            .iter()
            .map(|r| {
                (
                    r.id.clone(),
                    build_identifier(&self.project_identifier, r.sequence_id),
                )
            })
            .collect();
        let ctx = self.map_ctx(&identifier_by_uuid);

        let mut out = Vec::new();
        for r in &raw {
            let state = self.state_name(&r.state_id);
            if !self.active.contains(&state) {
                continue;
            }
            let blocked_by_ids = self.fetch_blocked_by_ids(r).await?;
            if is_blocked(&blocked_by_ids, &state_by_uuid, &self.terminal) {
                continue;
            }
            out.push(raw_to_issue(r, &ctx, &blocked_by_ids));
        }
        Ok(out)
    }

    fn fetch_all_inner(&self) -> Result<Vec<Issue>> {
        self.fetch_all_inner_with_mention_filter(true)
    }

    fn fetch_all_inner_unfiltered(&self) -> Result<Vec<Issue>> {
        self.fetch_all_inner_with_mention_filter(false)
    }

    fn fetch_all_inner_with_mention_filter(
        &self,
        apply_mention_filter: bool,
    ) -> Result<Vec<Issue>> {
        self.run_async(async {
            let raw = if apply_mention_filter {
                self.fetch_all_work_items().await?
            } else {
                self.fetch_all_work_items_unfiltered().await?
            };
            let identifier_by_uuid: HashMap<String, String> = raw
                .iter()
                .map(|r| {
                    (
                        r.id.clone(),
                        build_identifier(&self.project_identifier, r.sequence_id),
                    )
                })
                .collect();
            let ctx = self.map_ctx(&identifier_by_uuid);
            Ok(raw.iter().map(|r| raw_to_issue(r, &ctx, &[])).collect())
        })
    }

    /// Snapshot every work item in the project as portable [`Issue`]s plus the
    /// project meta, for `export.plane` (section 8.2).
    pub fn export_snapshot(&self) -> Result<PlaneExport> {
        self.run_async(async {
            let raw = self.fetch_all_work_items().await?;
            let identifier_by_uuid: HashMap<String, String> = raw
                .iter()
                .map(|r| {
                    (
                        r.id.clone(),
                        build_identifier(&self.project_identifier, r.sequence_id),
                    )
                })
                .collect();
            let ctx = self.map_ctx(&identifier_by_uuid);
            let issues: Vec<Issue> = raw.iter().map(|r| raw_to_issue(r, &ctx, &[])).collect();
            Ok(PlaneExport {
                project: PlaneProjectExport {
                    name: self.project_name.clone(),
                    identifier: self.project_identifier.clone(),
                    workspace: self.workspace.clone(),
                    api_url: self.api_url.clone(),
                    exported_at: Utc::now(),
                    issue_count: issues.len(),
                },
                issues,
            })
        })
    }

    fn park_issue_needs_human_inner(&self, issue: &Issue, comment: &str) -> Result<()> {
        let needs_human = self
            .needs_human
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("tracker.needs_human is not configured"))?;
        let state_id = self
            .state_id_by_name
            .get(needs_human)
            .cloned()
            .ok_or_else(|| anyhow!("Plane state {needs_human:?} not resolved at boot"))?;
        let project_id = issue
            .metadata
            .get("plane_project_id")
            .and_then(Value::as_str)
            .unwrap_or(&self.project);
        self.run_async(self.do_park_issue_needs_human(&issue.id, &state_id, project_id, comment))
    }

    /// Section 3: PATCH the work item to the needs-human state, then POST a
    /// comment explaining the park.
    async fn do_park_issue_needs_human(
        &self,
        issue_id: &str,
        state_id: &str,
        project_id: &str,
        comment: &str,
    ) -> Result<()> {
        let base = self.project_base(project_id);

        let patch_url = format!("{base}/work-items/{issue_id}/");
        let patch_body = serde_json::json!({ "state": state_id });
        self.send_with_rate_limit_async(|| {
            self.authed(self.client.patch(&patch_url)).json(&patch_body)
        })
        .await
        .with_context(|| format!("moving Plane work item {issue_id} to needs-human state"))?;

        let comment_url = format!("{base}/work-items/{issue_id}/comments/");
        let comment_body = serde_json::json!({ "comment_html": html_paragraph(comment) });
        self.send_with_rate_limit_async(|| {
            self.authed(self.client.post(&comment_url))
                .json(&comment_body)
        })
        .await
        .with_context(|| format!("commenting on Plane work item {issue_id}"))?;
        Ok(())
    }

    // --- sync bridge (block_in_place, mirrors tracker-linear) ---

    fn run_async<F, T>(&self, fut: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
    }
}

impl Tracker for PlaneTracker {
    fn poll_candidates(&self) -> Result<Vec<Issue>> {
        self.run_async(self.do_poll_candidates())
    }

    fn fetch_states(&self, ids: &[String]) -> Result<Vec<Issue>> {
        let all = self.fetch_all_inner()?;
        Ok(all
            .into_iter()
            .filter(|i| ids.contains(&i.id) || ids.contains(&i.identifier))
            .collect())
    }

    fn fetch_terminal(&self) -> Result<Vec<Issue>> {
        let all = self.fetch_all_inner()?;
        Ok(all
            .into_iter()
            .filter(|i| self.terminal.contains(&i.state))
            .collect())
    }

    fn fetch_one(&self, id: &str) -> Result<Option<Issue>> {
        let all = self.fetch_all_inner_unfiltered()?;
        Ok(all.into_iter().find(|i| i.id == id || i.identifier == id))
    }

    fn park_issue_needs_human(&self, issue: &Issue, comment: &str) -> Result<()> {
        self.park_issue_needs_human_inner(issue, comment)
    }

    fn rate_limit_remaining(&self) -> Option<i64> {
        let v = self.min_remaining.load(Ordering::SeqCst);
        if v == UNSET_MIN {
            None
        } else {
            Some(v)
        }
    }

    /// Plane's cursor pagination is not priority-ordered, so the orchestrator
    /// should apply its local v0 candidate sort.
    fn sort_candidates_locally(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

fn parse_json_body(text: &str) -> Result<Value> {
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(text).context("parsing Plane JSON response")
}

/// Extract `(results, next_cursor, has_next)` from one paginated page. A bare
/// JSON array (non-paginated endpoint) is treated as a single terminal page.
fn extract_page(resp: &Value) -> (Vec<Value>, Option<String>, bool) {
    if let Some(arr) = resp.as_array() {
        return (arr.clone(), None, false);
    }
    let results = resp
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let next_cursor = resp
        .get("next_cursor")
        .and_then(Value::as_str)
        .map(String::from);
    let has_next = resp
        .get("next_page_results")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    (results, next_cursor, has_next)
}

/// Sleep length (seconds) for an exhausted bucket. Plane's `X-RateLimit-Reset`
/// is a duration in seconds (seconds until refill); add 1 s of slack. Negatives
/// clamp to 0 (so the result is at least 1). Absent → 60 s.
fn reset_wait_secs(reset: Option<i64>) -> u64 {
    match reset {
        Some(secs) => secs.max(0) as u64 + 1,
        None => 60,
    }
}

/// Map Plane's priority string onto Linear's integer scale (0 = none, 1 = urgent
/// … 4 = low) so the orchestrator's shared priority sort behaves consistently.
fn map_priority(priority: Option<&str>) -> Option<i32> {
    match priority {
        Some("urgent") => Some(1),
        Some("high") => Some(2),
        Some("medium") => Some(3),
        Some("low") => Some(4),
        Some("none") => Some(0),
        _ => None,
    }
}

/// Build the human identifier `{PROJECT}-{sequence_id}` (e.g. `"PROJ-12"`).
fn build_identifier(project_identifier: &str, sequence_id: i64) -> String {
    format!("{project_identifier}-{sequence_id}")
}

/// Build the shareable Plane web URL for an issue identifier.
fn build_issue_url(app_url: &str, workspace: &str, identifier: &str) -> String {
    format!(
        "{}/{}/browse/{}/",
        app_url.trim_end_matches('/'),
        workspace,
        identifier
    )
}

/// Wrap plain comment text as a minimal HTML paragraph for Plane's
/// `comment_html` field, escaping the HTML metacharacters.
fn html_paragraph(text: &str) -> String {
    let escaped = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!("<p>{escaped}</p>")
}

/// A candidate is blocked iff one of its `blocked_by` related UUIDs is an
/// in-project work item (present in `state_by_uuid`) whose state is not terminal.
fn is_blocked(
    blocker_ids: &[String],
    state_by_uuid: &HashMap<String, String>,
    terminal: &[String],
) -> bool {
    blocker_ids.iter().any(|b| {
        state_by_uuid
            .get(b)
            .map(|s| !terminal.iter().any(|t| t == s))
            .unwrap_or(false)
    })
}

/// Constant context for mapping a [`RawWorkItem`] to a portable [`Issue`].
#[derive(Debug, Clone)]
struct ProjectMeta {
    identifier: String,
    name: Option<String>,
}

struct MapCtx<'a> {
    state_name_by_id: &'a HashMap<String, String>,
    label_name_by_id: &'a HashMap<String, String>,
    identifier_by_uuid: HashMap<String, String>,
    project_identifier: &'a str,
    project_name: Option<&'a str>,
    project_meta_by_id: &'a HashMap<String, ProjectMeta>,
    workspace: &'a str,
    app_url: &'a str,
}

fn raw_to_issue(r: &RawWorkItem, ctx: &MapCtx, blocked_by_ids: &[String]) -> Issue {
    let project_meta = ctx.project_meta_by_id.get(&r.project_id);
    let project_identifier = project_meta
        .map(|m| m.identifier.as_str())
        .unwrap_or(ctx.project_identifier);
    let project_name = project_meta
        .and_then(|m| m.name.as_deref())
        .or(ctx.project_name);
    let identifier = build_identifier(project_identifier, r.sequence_id);
    let state = ctx
        .state_name_by_id
        .get(&r.state_id)
        .cloned()
        .unwrap_or_else(|| r.state_id.clone());
    let url = build_issue_url(ctx.app_url, ctx.workspace, &identifier);
    let labels = r
        .label_ids
        .iter()
        .map(|id| {
            ctx.label_name_by_id
                .get(id)
                .cloned()
                .unwrap_or_else(|| id.clone())
        })
        .collect();
    let blocked_by = blocked_by_ids
        .iter()
        .map(|id| {
            ctx.identifier_by_uuid
                .get(id)
                .cloned()
                .unwrap_or_else(|| id.clone())
        })
        .collect();

    Issue::builder(r.id.clone(), identifier, r.name.clone(), state)
        .description(r.description_html.clone())
        .url(Some(url))
        .priority(map_priority(r.priority.as_deref()))
        .assignees(r.assignee_ids.clone())
        .labels(labels)
        .created_at(r.created_at)
        .updated_at(r.updated_at)
        .parent_id(r.parent_id.clone())
        .blocked_by(blocked_by)
        .project_name(project_name.map(String::from))
        .project_slug(Some(project_identifier.to_string()))
        .metadata_entry("plane_project_id", Value::String(r.project_id.clone()))
        .build()
}

// ---------------------------------------------------------------------------
// Serde structs for the Plane REST responses
// ---------------------------------------------------------------------------

/// A Plane work item. Accepts both the modern `_id`-suffixed field names and the
/// older bare names via serde aliases so it survives across API revisions.
#[derive(Debug, Deserialize)]
struct RawWorkItem {
    id: String,
    name: String,
    #[serde(default)]
    description_html: Option<String>,
    #[serde(default)]
    priority: Option<String>,
    #[serde(alias = "state")]
    state_id: String,
    sequence_id: i64,
    #[serde(default, alias = "labels")]
    label_ids: Vec<String>,
    #[serde(default, alias = "assignees")]
    assignee_ids: Vec<String>,
    #[serde(default, alias = "parent")]
    parent_id: Option<String>,
    #[serde(default, alias = "project")]
    project_id: String,
    #[serde(default)]
    created_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// The grouped-by-relation-type response of `.../work-items/{id}/relations/`.
/// Only the `blocked_by` group participates in candidacy (AMENDMENT); the other
/// groups are ignored.
#[derive(Debug, Default, Deserialize)]
struct RawRelations {
    #[serde(default)]
    blocked_by: Vec<RawRelatedRef>,
}

impl RawRelations {
    fn blocked_by_ids(&self) -> Vec<String> {
        self.blocked_by.iter().map(|r| r.issue_id.clone()).collect()
    }
}

#[derive(Debug, Deserialize)]
struct RawRelatedRef {
    issue_id: String,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::Ordering;

    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    use serde_json::json;

    use super::*;

    /// Serializes the env-var mutation tests: they share process env.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    // --- auth (section 6) ---

    #[test]
    fn auth_prefers_bot_token_as_bearer() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var(BOT_TOKEN_ENV, "bot-abc");
        std::env::set_var(OAUTH_TOKEN_ENV, "oauth-abc");
        std::env::set_var(API_KEY_ENV, "plane_key_xyz");
        let (name, value) = resolve_plane_auth().unwrap().header();
        assert_eq!(name, "Authorization");
        assert_eq!(value, "Bearer bot-abc");
        std::env::remove_var(BOT_TOKEN_ENV);
        std::env::remove_var(OAUTH_TOKEN_ENV);
        std::env::remove_var(API_KEY_ENV);
    }

    #[test]
    fn auth_falls_back_to_oauth_token_as_bearer() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var(BOT_TOKEN_ENV);
        std::env::set_var(OAUTH_TOKEN_ENV, "oauth-abc");
        std::env::set_var(API_KEY_ENV, "plane_key_xyz");
        let (name, value) = resolve_plane_auth().unwrap().header();
        assert_eq!(name, "Authorization");
        assert_eq!(value, "Bearer oauth-abc");
        std::env::remove_var(OAUTH_TOKEN_ENV);
        std::env::remove_var(API_KEY_ENV);
    }

    #[test]
    fn auth_falls_back_to_api_key_header() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var(BOT_TOKEN_ENV);
        std::env::remove_var(OAUTH_TOKEN_ENV);
        std::env::set_var(API_KEY_ENV, "plane_key_xyz");
        let (name, value) = resolve_plane_auth().unwrap().header();
        assert_eq!(name, "X-API-Key");
        assert_eq!(value, "plane_key_xyz");
        std::env::remove_var(API_KEY_ENV);
    }

    #[test]
    fn auth_none_when_unset_or_empty() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var(BOT_TOKEN_ENV);
        std::env::remove_var(OAUTH_TOKEN_ENV);
        std::env::remove_var(API_KEY_ENV);
        assert!(resolve_plane_auth().is_none());
        std::env::set_var(BOT_TOKEN_ENV, "");
        std::env::set_var(OAUTH_TOKEN_ENV, "");
        std::env::set_var(API_KEY_ENV, "");
        assert!(resolve_plane_auth().is_none());
        std::env::remove_var(BOT_TOKEN_ENV);
        std::env::remove_var(OAUTH_TOKEN_ENV);
        std::env::remove_var(API_KEY_ENV);
    }

    // --- config parsing ---

    #[test]
    fn parse_ext_config_reads_all_fields() {
        let cfg = parse_ext_config(Some(&json!({
            "workspace": "acme",
            "project": "proj-uuid",
            "api_url": "https://plane.internal",
            "app_url": "https://app.internal"
        })));
        assert_eq!(cfg.workspace(), "acme");
        assert_eq!(cfg.project(), "proj-uuid");
        assert_eq!(cfg.api_url(), "https://plane.internal");
        assert_eq!(cfg.app_url(), "https://app.internal");
    }

    #[test]
    fn parse_ext_config_defaults_urls() {
        let cfg = parse_ext_config(Some(&json!({ "workspace": "acme", "project": "p" })));
        assert_eq!(cfg.api_url(), DEFAULT_API_URL);
        assert_eq!(cfg.app_url(), DEFAULT_APP_URL);
    }

    #[test]
    fn parse_ext_config_none_is_default() {
        let cfg = parse_ext_config(None);
        assert_eq!(cfg.workspace(), "");
        assert_eq!(cfg.project(), "");
        assert_eq!(cfg.api_url(), DEFAULT_API_URL);
        assert_eq!(cfg.app_url(), DEFAULT_APP_URL);
    }

    // --- rate limit (section 5) ---

    fn make_tracker() -> PlaneTracker {
        PlaneTracker::new(PlaneTrackerConfig {
            api_url: "http://localhost".to_string(),
            app_url: "https://app.plane.so".to_string(),
            workspace: "ws".to_string(),
            project: "proj".to_string(),
            auth: None,
            active_states: vec![],
            terminal_states: vec![],
            needs_human: None,
            mention: None,
        })
        .unwrap()
    }

    fn header_map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        m
    }

    fn process_headers_sync(headers: &HeaderMap) -> i64 {
        let tracker = make_tracker();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(tracker.process_rate_limit_headers(headers));
        tracker.min_remaining.load(Ordering::SeqCst)
    }

    #[test]
    fn remaining_is_tracked() {
        let headers = header_map(&[("x-ratelimit-remaining", "42")]);
        assert_eq!(process_headers_sync(&headers), 42);
    }

    #[test]
    fn no_rate_limit_headers_leaves_unset() {
        assert_eq!(process_headers_sync(&header_map(&[])), UNSET_MIN);
    }

    #[test]
    fn rate_limit_remaining_returns_none_before_any_request() {
        assert_eq!(make_tracker().rate_limit_remaining(), None);
    }

    #[test]
    fn min_remaining_updates_across_calls() {
        let tracker = make_tracker();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(
            tracker.process_rate_limit_headers(&header_map(&[("x-ratelimit-remaining", "50")])),
        );
        assert_eq!(tracker.min_remaining.load(Ordering::SeqCst), 50);
        rt.block_on(
            tracker.process_rate_limit_headers(&header_map(&[("x-ratelimit-remaining", "30")])),
        );
        assert_eq!(tracker.min_remaining.load(Ordering::SeqCst), 30);
        rt.block_on(
            tracker.process_rate_limit_headers(&header_map(&[("x-ratelimit-remaining", "80")])),
        );
        assert_eq!(tracker.min_remaining.load(Ordering::SeqCst), 30);
    }

    #[test]
    fn reset_wait_treats_header_as_seconds_duration() {
        // Plane sends seconds-until-reset, not an epoch: 30 → 31 (with 1s slack),
        // NOT (30/1000 - now). This is the key divergence from Linear.
        assert_eq!(reset_wait_secs(Some(30)), 31);
    }

    #[test]
    fn reset_wait_defaults_when_absent() {
        assert_eq!(reset_wait_secs(None), 60);
    }

    #[test]
    fn reset_wait_clamps_negative_to_one() {
        assert_eq!(reset_wait_secs(Some(-5)), 1);
    }

    // --- priority mapping ---

    #[test]
    fn map_priority_covers_all_levels() {
        assert_eq!(map_priority(Some("urgent")), Some(1));
        assert_eq!(map_priority(Some("high")), Some(2));
        assert_eq!(map_priority(Some("medium")), Some(3));
        assert_eq!(map_priority(Some("low")), Some(4));
    }

    #[test]
    fn map_priority_none_is_zero() {
        assert_eq!(map_priority(Some("none")), Some(0));
    }

    #[test]
    fn map_priority_unknown_or_missing_is_none() {
        assert_eq!(map_priority(Some("weird")), None);
        assert_eq!(map_priority(None), None);
    }

    // --- identifier / url building ---

    #[test]
    fn build_identifier_joins_prefix_and_sequence() {
        assert_eq!(build_identifier("PROJ", 12), "PROJ-12");
    }

    #[test]
    fn build_issue_url_trims_trailing_slash() {
        assert_eq!(
            build_issue_url("https://app.plane.so/", "acme", "PROJ-12"),
            "https://app.plane.so/acme/browse/PROJ-12/"
        );
        assert_eq!(
            build_issue_url("https://app.plane.so", "acme", "PROJ-12"),
            "https://app.plane.so/acme/browse/PROJ-12/"
        );
    }

    #[test]
    fn html_paragraph_escapes_metacharacters() {
        assert_eq!(
            html_paragraph("a < b & c > d"),
            "<p>a &lt; b &amp; c &gt; d</p>"
        );
    }

    // --- raw_to_issue ---

    fn map_ctx_for<'a>(
        states: &'a HashMap<String, String>,
        labels: &'a HashMap<String, String>,
        identifiers: HashMap<String, String>,
    ) -> MapCtx<'a> {
        let project_meta_by_id = Box::leak(Box::new(HashMap::new()));
        MapCtx {
            state_name_by_id: states,
            label_name_by_id: labels,
            identifier_by_uuid: identifiers,
            project_identifier: "PROJ",
            project_name: Some("Dar"),
            project_meta_by_id,
            workspace: "acme",
            app_url: "https://app.plane.so",
        }
    }

    fn sample_work_item() -> RawWorkItem {
        serde_json::from_str(
            r#"{
              "id": "wi-1",
              "name": "Move tracker",
              "description_html": "<p>Details</p>",
              "priority": "high",
              "state_id": "state-todo",
              "sequence_id": 7,
              "label_ids": ["lbl-1"],
              "assignee_ids": ["user-1"],
              "parent_id": "wi-0",
              "project_id": "proj-1",
              "created_at": "2026-06-11T10:00:00Z",
              "updated_at": "2026-06-11T11:00:00Z"
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn raw_to_issue_maps_core_fields() {
        let mut states = HashMap::new();
        states.insert("state-todo".to_string(), "Todo".to_string());
        let mut labels = HashMap::new();
        labels.insert("lbl-1".to_string(), "backend".to_string());
        let ctx = map_ctx_for(&states, &labels, HashMap::new());

        let issue = raw_to_issue(&sample_work_item(), &ctx, &[]);
        assert_eq!(issue.id, "wi-1");
        assert_eq!(issue.identifier, "PROJ-7");
        assert_eq!(issue.title, "Move tracker");
        assert_eq!(issue.state, "Todo");
        assert_eq!(issue.description.as_deref(), Some("<p>Details</p>"));
        assert_eq!(issue.priority, Some(2));
        assert_eq!(issue.assignees, vec!["user-1"]);
        assert_eq!(issue.labels, vec!["backend"]);
        assert_eq!(issue.parent_id.as_deref(), Some("wi-0"));
        assert_eq!(
            issue.url.as_deref(),
            Some("https://app.plane.so/acme/browse/PROJ-7/")
        );
        assert_eq!(issue.project_name.as_deref(), Some("Dar"));
        assert_eq!(issue.project_slug.as_deref(), Some("PROJ"));
    }

    #[test]
    fn raw_to_issue_falls_back_to_uuid_when_state_unmapped() {
        let states = HashMap::new();
        let labels = HashMap::new();
        let ctx = map_ctx_for(&states, &labels, HashMap::new());
        let issue = raw_to_issue(&sample_work_item(), &ctx, &[]);
        // Unknown state id passes through; unknown label id passes through.
        assert_eq!(issue.state, "state-todo");
        assert_eq!(issue.labels, vec!["lbl-1"]);
    }

    #[test]
    fn raw_to_issue_resolves_blocked_by_to_identifiers() {
        let states = HashMap::new();
        let labels = HashMap::new();
        let mut identifiers = HashMap::new();
        identifiers.insert("wi-a".to_string(), "PROJ-1".to_string());
        let ctx = map_ctx_for(&states, &labels, identifiers);
        let issue = raw_to_issue(
            &sample_work_item(),
            &ctx,
            &["wi-a".to_string(), "wi-x".to_string()],
        );
        // Known blocker → identifier; unknown blocker → raw uuid.
        assert_eq!(issue.blocked_by, vec!["PROJ-1", "wi-x"]);
    }

    // --- relations / blocked (AMENDMENT) ---

    #[test]
    fn relations_extract_only_blocked_by_group() {
        let relations: RawRelations = serde_json::from_value(json!({
            "blocking": [{ "project_id": "p", "issue_id": "wi-9" }],
            "blocked_by": [
                { "project_id": "p", "issue_id": "wi-a" },
                { "project_id": "p", "issue_id": "wi-b" }
            ],
            "duplicate": [],
            "relates_to": [{ "project_id": "p", "issue_id": "wi-c" }]
        }))
        .unwrap();
        assert_eq!(relations.blocked_by_ids(), vec!["wi-a", "wi-b"]);
    }

    #[test]
    fn relations_empty_when_no_blocked_by() {
        let relations: RawRelations = serde_json::from_value(json!({
            "blocking": [{ "project_id": "p", "issue_id": "wi-9" }]
        }))
        .unwrap();
        assert!(relations.blocked_by_ids().is_empty());
    }

    fn state_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn is_blocked_when_in_project_blocker_non_terminal() {
        let map = state_map(&[("wi-a", "In Progress")]);
        let terminal = vec!["Done".to_string(), "Cancelled".to_string()];
        assert!(is_blocked(&["wi-a".to_string()], &map, &terminal));
    }

    #[test]
    fn not_blocked_when_blocker_terminal() {
        let map = state_map(&[("wi-a", "Done")]);
        let terminal = vec!["Done".to_string()];
        assert!(!is_blocked(&["wi-a".to_string()], &map, &terminal));
    }

    #[test]
    fn not_blocked_when_blocker_outside_project() {
        let map = state_map(&[]);
        let terminal = vec!["Done".to_string()];
        assert!(!is_blocked(&["wi-z".to_string()], &map, &terminal));
    }

    // --- state validation (section 2.3) ---

    #[test]
    fn validate_states_rejects_empty_active() {
        let err = validate_states(&[], &["Done".into()], None).unwrap_err();
        assert!(err.to_string().contains("active_states"));
    }

    #[test]
    fn validate_states_rejects_empty_terminal() {
        let err = validate_states(&["Todo".into()], &[], None).unwrap_err();
        assert!(err.to_string().contains("terminal_states"));
    }

    #[test]
    fn validate_states_rejects_overlap() {
        let err = validate_states(&["Todo".into()], &["Todo".into()], None).unwrap_err();
        assert!(err.to_string().contains("both"));
    }

    #[test]
    fn validate_states_rejects_needs_human_in_active() {
        let err = validate_states(&["Todo".into()], &["Done".into()], Some("Todo")).unwrap_err();
        assert!(err.to_string().contains("needs_human"));
    }

    #[test]
    fn validate_states_accepts_valid_config() {
        assert!(validate_states(
            &["Todo".into(), "In Progress".into()],
            &["Done".into()],
            Some("Needs Human")
        )
        .is_ok());
    }

    // --- factory config rejections (section 2.3) ---

    /// `Arc<dyn Tracker>` is not `Debug`, so `unwrap_err` is unavailable; extract
    /// the error explicitly.
    fn build_err(factory: &PlaneTrackerFactory, cfg: TrackerBuildConfig) -> String {
        match factory.build(cfg) {
            Ok(_) => panic!("expected PlaneTrackerFactory::build to reject the config"),
            Err(e) => e.to_string(),
        }
    }

    fn build_cfg(active: Vec<String>, terminal: Vec<String>) -> TrackerBuildConfig {
        TrackerBuildConfig {
            root: std::path::PathBuf::from("."),
            config_path: None,
            active_states: active,
            terminal_states: terminal,
            project_slug: None,
            project: None,
            workspace: None,
            endpoint: None,
            needs_human: None,
            team: None,
            assignee: None,
            delegate: None,
            mention: None,
            labels: vec![],
        }
    }

    #[test]
    fn factory_rejects_empty_workspace() {
        let factory = PlaneTrackerFactory::new(PlaneExtConfig {
            workspace: String::new(),
            project: "p".into(),
            ..Default::default()
        });
        let err = build_err(
            &factory,
            build_cfg(vec!["Todo".into()], vec!["Done".into()]),
        );
        assert!(err.contains("workspace"));
    }

    #[test]
    fn factory_rejects_invalid_states_before_network() {
        let factory = PlaneTrackerFactory::new(PlaneExtConfig {
            workspace: "acme".into(),
            project: "p".into(),
            ..Default::default()
        });
        // Empty active_states fails validation before any Plane request.
        let err = build_err(&factory, build_cfg(vec![], vec!["Done".into()]));
        assert!(err.contains("active_states"));
    }

    // --- misc ---

    #[test]
    fn sort_candidates_locally_is_true() {
        assert!(make_tracker().sort_candidates_locally());
    }

    // --- workflow body / init-workflow (section 8.3) ---

    #[test]
    fn default_workflow_body_contains_standard_worker_procedure() {
        let body = super::DEFAULT_WORKFLOW_MD_BODY;
        assert!(body.contains("## Required Claim Step"));
        assert!(body.contains("Fetch the Plane work item {{ issue.identifier }}"));
        assert!(body.contains("If its current state is `Todo`, move it to `In Progress`"));
        assert!(body.contains("Read all Plane work item comments"));
        assert!(body.contains("## Dependencies"));
        assert!(body.contains("## Workspace"));
        assert!(body.contains("## Review And PR Flow"));
        assert!(body.contains("Spawn a reviewer subagent"));
        assert!(body.contains("Link the PR to the Plane work item"));
        assert!(body.contains("## Git Usage"));
        assert!(body.contains("## Blockers"));
        assert!(body.contains("move the work item to `Needs Human` and stop"));
        assert!(body.contains("## Completion"));
    }

    #[test]
    fn default_workflow_body_describes_daemon_auto_skip_and_plane_api_double_check() {
        let body = super::DEFAULT_WORKFLOW_MD_BODY;
        // AMENDMENT: blocked work is auto-skipped by the daemon; the worker
        // double-checks relations mid-run via the plane_api tool.
        assert!(
            body.contains("The daemon already skips any work item whose `blocked_by` relations")
        );
        assert!(body.contains("`plane_api`"));
        assert!(body.contains("work-items/{id}/relations/"));
    }

    #[test]
    fn default_workflow_body_is_prompt_only_guidance() {
        let body = super::DEFAULT_WORKFLOW_MD_BODY;
        assert!(!body.contains("daemon will"));
        assert!(!body.contains("orchestrator will"));
        assert!(body.contains("prompt-level worker guidance"));
    }

    #[test]
    fn init_workflow_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        super::init_workflow(dir.path(), false).unwrap();

        let body = std::fs::read_to_string(dir.path().join("WORKFLOW.md")).unwrap();
        assert_eq!(body, format!("{}\n", super::DEFAULT_WORKFLOW_MD_BODY));
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".gitignore")).unwrap(),
            ".env\n"
        );
    }

    #[test]
    fn init_workflow_refuses_overwrite_without_force() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("WORKFLOW.md"), "existing").unwrap();
        let err = super::init_workflow(dir.path(), false).unwrap_err();
        assert!(err.to_string().contains("already exists"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("WORKFLOW.md")).unwrap(),
            "existing"
        );
    }

    #[test]
    fn init_workflow_overwrites_with_force() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("WORKFLOW.md"), "existing").unwrap();
        super::init_workflow(dir.path(), true).unwrap();
        let body = std::fs::read_to_string(dir.path().join("WORKFLOW.md")).unwrap();
        assert_eq!(body, format!("{}\n", super::DEFAULT_WORKFLOW_MD_BODY));
    }

    #[test]
    fn init_workflow_can_seed_plane_frontmatter_without_agent_yaml() {
        let dir = tempfile::tempdir().unwrap();
        super::init_workflow_with_options(dir.path(), false, Some("acme"), Some("proj-uuid"), true)
            .unwrap();

        let body = std::fs::read_to_string(dir.path().join("WORKFLOW.md")).unwrap();
        assert!(body.starts_with("---\n"));
        assert!(body.contains("  kind: plane\n"));
        assert!(body.contains("  workspace: acme\n"));
        assert!(body.contains("  project: proj-uuid\n"));
        assert!(body.contains("  exposeApiTool: true\n"));
    }

    #[test]
    fn init_workflow_preserves_existing_gitignore_and_adds_env_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "target\n").unwrap();

        super::init_workflow(dir.path(), false).unwrap();
        super::init_workflow(dir.path(), true).unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join(".gitignore")).unwrap(),
            "target\n.env\n"
        );
    }

    // --- export (section 8.2) ---

    #[test]
    fn write_snapshot_writes_project_and_issues_under_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let agent_cfg = super::ExportAgentConfig {
            id: "agent-1".to_string(),
            name: "Agent One".to_string(),
            tracker: super::ExportTrackerConfig {
                use_: "plane".to_string(),
                active_states: vec!["Todo".to_string()],
                terminal_states: vec!["Done".to_string()],
                endpoint: None,
                workspace: None,
                project: None,
                needs_human: None,
            },
            extensions: super::ExportExtensions::default(),
        };
        let snapshot = super::PlaneExport {
            project: super::PlaneProjectExport {
                name: Some("Project".to_string()),
                identifier: "PROJ".to_string(),
                workspace: "acme".to_string(),
                api_url: DEFAULT_API_URL.to_string(),
                exported_at: chrono::Utc::now(),
                issue_count: 0,
            },
            issues: Vec::new(),
        };

        let result = super::write_snapshot(dir.path(), &agent_cfg, snapshot).unwrap();

        assert_eq!(result.issue_count, 0);
        assert!(result.project_path.starts_with(dir.path().join("data")));
        assert!(result.issues_path.starts_with(dir.path().join("data")));
        assert!(result.project_path.exists());
        assert!(result.issues_path.exists());
    }
}
