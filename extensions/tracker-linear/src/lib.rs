//! `LinearTracker`: polls Linear's GraphQL API scoped to a project by slugId.
//!
//! Rate-limit handling:
//! - Reads `x-ratelimit-requests-remaining` / `x-ratelimit-requests-reset` and
//!   `x-ratelimit-complexity-remaining` / `x-ratelimit-complexity-reset` headers
//!   on every response.
//! - When remaining ≤ 0 on either dimension: sleeps until that dimension's reset + 1 s.
//! - On HTTP 429: sleeps for `Retry-After` seconds (or 60 s) then retries once.
//! - Tracks the minimum remaining seen (across both dimensions) for the dashboard RATE LIMIT stat.
//!
//! Blocked-issue skipping: `poll_candidates` fetches ALL project issues,
//! builds a state-lookup map, and omits any active issue whose inverse `blocks`
//! relation contains at least one non-terminal issue.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use std::{path::Path, path::PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use cap_tracker::{Issue, Tracker, TrackerBuildConfig, TrackerFactory};
use chrono::Utc;
use host_api::{Extension, HostCommand, RegisterCtx};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tool_registry::{ToolRegistryHandle, TOOL_REGISTRY_SERVICE};

mod linear_graphql;

pub(crate) const DEFAULT_ENDPOINT: &str = "https://api.linear.app/graphql";
/// Env var holding a personal Linear API key. Sent raw, with no prefix.
pub(crate) const API_KEY_ENV: &str = "LINEAR_API_KEY";
/// Env var holding a Linear OAuth app access token (`actor=app`). Sent with a
/// `Bearer ` prefix. Takes precedence over `LINEAR_API_KEY` when both are set.
pub(crate) const OAUTH_TOKEN_ENV: &str = "LINEAR_OAUTH_TOKEN";

/// Resolve the Linear `Authorization` header value from the environment.
///
/// Linear accepts two token types via the same header:
/// - a personal API key, sent raw: `Authorization: <token>`
/// - an OAuth app access token, sent prefixed: `Authorization: Bearer <token>`
///
/// `LINEAR_OAUTH_TOKEN` takes precedence over `LINEAR_API_KEY` when both are
/// set. Returns `None` when neither is set (or both are empty).
pub(crate) fn resolve_linear_auth_header() -> Option<String> {
    if let Some(token) = std::env::var(OAUTH_TOKEN_ENV)
        .ok()
        .filter(|t| !t.is_empty())
    {
        return Some(format!("Bearer {token}"));
    }
    std::env::var(API_KEY_ENV).ok().filter(|k| !k.is_empty())
}
/// Initial sentinel: no real observation yet.
const UNSET_MIN: i64 = i64::MAX;
/// Page size for GraphQL pagination.
const PAGE_SIZE: u64 = 50;

pub struct LinearTrackerConfig {
    pub endpoint: String,
    /// The full `Authorization` header value (raw API key, or `Bearer <token>`
    /// for an OAuth app token). Built via [`resolve_linear_auth_header`].
    pub api_key: String,
    pub project_slug: String, // "" = unconstrained
    pub team: Option<String>,
    pub assignee_id: Option<String>, // already-resolved Linear user id
    pub delegate_id: Option<String>, // already-resolved Linear user id
    pub labels: Vec<String>,
    pub active_states: Vec<String>,
    pub terminal_states: Vec<String>,
    pub needs_human: Option<String>,
}

pub struct TrackerLinearExtension;

impl Extension for TrackerLinearExtension {
    fn id(&self) -> &'static str {
        "tracker-linear"
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let factory: Arc<dyn TrackerFactory> = Arc::new(LinearTrackerFactory);
            ctx.services
                .service::<dyn TrackerFactory>("linear", factory)?;
            ctx.services
                .service::<dyn HostCommand>("init-workflow", Arc::new(InitWorkflowCommand))?;
            ctx.services
                .service::<dyn HostCommand>("export", Arc::new(ExportCommand))?;

            // Register the `linear_graphql` host tool against the shared
            // registry, if one is published. The registry is owned by the
            // tool-registry-host extension and is always present in the stock
            // composition; we resolve it leniently so a stripped composition
            // without the registry still boots the tracker.
            if let Ok(registry) = ctx
                .services
                .get_named::<dyn ToolRegistryHandle>(TOOL_REGISTRY_SERVICE)
            {
                let endpoint = linear_graphql::linear_graphql_endpoint(ctx.config.get(self.id()));
                linear_graphql::register_into(registry.as_ref(), endpoint)?;
            }
            Ok(())
        })
    }
}

struct InitWorkflowCommand;

#[derive(Debug, Deserialize)]
struct InitWorkflowCommandArgs {
    dir: PathBuf,
    force: bool,
    linear_project_slug: Option<String>,
    linear_project: Option<String>,
    expose_graphql_tool: bool,
}

impl HostCommand for InitWorkflowCommand {
    fn run(&self, args: Value) -> Result<()> {
        let args: InitWorkflowCommandArgs =
            serde_json::from_value(args).context("parsing init-workflow args")?;
        init_workflow_with_options(
            &args.dir,
            args.force,
            args.linear_project_slug.as_deref(),
            args.linear_project.as_deref(),
            args.expose_graphql_tool,
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
        let result = export_linear_project_from_root(&args.dir)?;
        println!(
            "exported {} issues to {}",
            result.issue_count,
            result.dir.display()
        );
        Ok(())
    }
}

struct LinearTrackerFactory;

impl TrackerFactory for LinearTrackerFactory {
    fn build(&self, cfg: TrackerBuildConfig) -> Result<Arc<dyn Tracker>> {
        let api_key = match resolve_linear_auth_header() {
            Some(header) => header,
            None => {
                tracing::warn!(
                    "neither {OAUTH_TOKEN_ENV} nor {API_KEY_ENV} is set; Linear API requests will fail with 401"
                );
                String::new()
            }
        };
        let project_slug = cfg.project_slug.clone().unwrap_or_default();
        let team = cfg.team.clone().filter(|t| !t.is_empty());
        let assignee_raw = cfg.assignee.clone().filter(|a| !a.is_empty());
        let delegate_raw = cfg.delegate.clone().filter(|d| !d.is_empty());
        let labels = cfg.labels.clone();

        // Empty-filter guard: refuse to poll the whole workspace. User targets
        // are not yet resolved here, so configured-but-unresolved values still
        // count as constraining dimensions.
        let configured = ResolvedDims {
            project_slug: Some(project_slug.clone()).filter(|s| !s.is_empty()),
            team_key: team.clone(),
            assignee_id: assignee_raw.clone(),
            delegate_id: delegate_raw.clone(),
            labels: labels.clone(),
        };
        if configured.is_empty() {
            bail!(
                "Linear tracker has no filter configured; set at least one of tracker.project_slug, tracker.team, tracker.assignee, tracker.delegate, tracker.label"
            );
        }

        let mut tracker = LinearTracker::new(LinearTrackerConfig {
            endpoint: cfg.endpoint.unwrap_or_else(|| DEFAULT_ENDPOINT.to_string()),
            api_key,
            project_slug,
            team,
            assignee_id: None,
            delegate_id: None,
            labels,
            active_states: cfg.active_states,
            terminal_states: cfg.terminal_states,
            needs_human: cfg.needs_human,
        })?;

        let users = if assignee_raw.is_some() || delegate_raw.is_some() {
            Some(
                tracker
                    .run_async(tracker.fetch_users_async())
                    .context("fetching Linear users to resolve tracker user filters")?,
            )
        } else {
            None
        };

        // Resolve user filters to canonical user ids once, at boot (fail fast).
        if let Some(raw) = &assignee_raw {
            let id = resolve_user_id(raw, users.as_deref().unwrap_or(&[]), "assignee")
                .with_context(|| format!("resolving tracker.assignee {raw:?}"))?;
            tracker.assignee_id = Some(id);
        }
        if let Some(raw) = &delegate_raw {
            let id = resolve_user_id(raw, users.as_deref().unwrap_or(&[]), "delegate")
                .with_context(|| format!("resolving tracker.delegate {raw:?}"))?;
            tracker.delegate_id = Some(id);
        }

        Ok(Arc::new(tracker))
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct LinearProjectExport {
    pub name: Option<String>,
    pub slug: String,
    pub endpoint: String,
    pub exported_at: chrono::DateTime<chrono::Utc>,
    pub issue_count: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct LinearExport {
    pub project: LinearProjectExport,
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
}

#[derive(Debug, Clone, Deserialize)]
struct ExportTrackerConfig {
    #[serde(rename = "use")]
    use_: String,
    active_states: Vec<String>,
    terminal_states: Vec<String>,
    #[serde(default)]
    project_slug: Option<String>,
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    needs_human: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ExportWorkflowFrontmatter {
    tracker: Option<ExportWorkflowTracker>,
}

#[derive(Debug, Default, Deserialize)]
struct ExportWorkflowTracker {
    #[serde(default, alias = "kind")]
    use_: Option<String>,
    #[serde(default)]
    project_slug: Option<String>,
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    needs_human: Option<String>,
    #[serde(default)]
    active_states: Option<Vec<String>>,
    #[serde(default)]
    terminal_states: Option<Vec<String>>,
}

pub fn export_linear_project_from_root(root: &Path) -> Result<ExportResult> {
    let agent_cfg: ExportAgentConfig =
        serde_yaml::from_str(&std::fs::read_to_string(root.join("agent.yaml"))?)
            .context("parsing agent.yaml for Linear export")?;
    let workflow = load_workflow_frontmatter(&root.join("WORKFLOW.md"))?;
    let tracker = merge_export_tracker(agent_cfg.tracker.clone(), workflow.tracker);
    export_linear_project(root, &agent_cfg, &tracker)
}

fn merge_export_tracker(
    mut base: ExportTrackerConfig,
    workflow: Option<ExportWorkflowTracker>,
) -> ExportTrackerConfig {
    if let Some(workflow) = workflow {
        if let Some(use_) = workflow.use_ {
            base.use_ = use_;
        }
        if workflow.project_slug.is_some() {
            base.project_slug = workflow.project_slug;
        }
        if workflow.endpoint.is_some() {
            base.endpoint = workflow.endpoint;
        }
        if workflow.needs_human.is_some() {
            base.needs_human = workflow.needs_human;
        }
        if let Some(active_states) = workflow.active_states {
            base.active_states = active_states;
        }
        if let Some(terminal_states) = workflow.terminal_states {
            base.terminal_states = terminal_states;
        }
    }
    base
}

fn load_workflow_frontmatter(path: &Path) -> Result<ExportWorkflowFrontmatter> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let (frontmatter, _) = split_frontmatter(&raw);
    match frontmatter {
        Some(src) => serde_yaml::from_str(src).context("parsing WORKFLOW.md frontmatter"),
        None => Ok(ExportWorkflowFrontmatter::default()),
    }
}

fn split_frontmatter(src: &str) -> (Option<&str>, &str) {
    let rest = match src.strip_prefix("---\n") {
        Some(r) => r,
        None => match src.strip_prefix("---\r\n") {
            Some(r) => r,
            None => return (None, src),
        },
    };

    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" {
            let fm = &rest[..offset];
            let body = &rest[offset + line.len()..];
            return (Some(fm), body);
        }
        offset += line.len();
    }
    if rest[offset..].trim_end() == "---" {
        return (Some(&rest[..offset]), "");
    }
    (None, src)
}

fn export_linear_project(
    root: &Path,
    agent_cfg: &ExportAgentConfig,
    tracker_cfg: &ExportTrackerConfig,
) -> Result<ExportResult> {
    if tracker_cfg.use_ != "linear" {
        bail!(
            "export requires tracker.kind/use \"linear\" (got {:?})",
            tracker_cfg.use_
        );
    }
    let api_key = resolve_linear_auth_header()
        .context("LINEAR_OAUTH_TOKEN or LINEAR_API_KEY is required for Linear export")?;
    let project_slug = tracker_cfg
        .project_slug
        .clone()
        .filter(|slug| !slug.is_empty())
        .context("tracker.project_slug is required for Linear export")?;

    let tracker = LinearTracker::new(LinearTrackerConfig {
        endpoint: tracker_cfg
            .endpoint
            .clone()
            .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string()),
        api_key,
        project_slug,
        team: None,
        assignee_id: None,
        delegate_id: None,
        labels: vec![],
        active_states: tracker_cfg.active_states.clone(),
        terminal_states: tracker_cfg.terminal_states.clone(),
        needs_human: tracker_cfg.needs_human.clone(),
    })?;
    let snapshot = tracker.export_snapshot()?;
    write_snapshot(root, agent_cfg, snapshot)
}

fn write_snapshot(
    root: &Path,
    agent_cfg: &ExportAgentConfig,
    snapshot: LinearExport,
) -> Result<ExportResult> {
    let dir = root.join("data").join("export");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let project_path = dir.join("project.json");
    let issues_path = dir.join("issues.json");
    let project = serde_json::json!({
        "agent_id": agent_cfg.id,
        "agent_name": agent_cfg.name,
        "linear_project": snapshot.project,
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

pub fn init_workflow(root: &Path, force: bool) -> Result<()> {
    init_workflow_with_options(root, force, None, None, false)
}

pub fn init_workflow_with_options(
    root: &Path,
    force: bool,
    linear_project_slug: Option<&str>,
    linear_project: Option<&str>,
    expose_graphql_tool: bool,
) -> Result<()> {
    let path = root.join("WORKFLOW.md");
    if path.exists() && !force {
        bail!(
            "{} already exists; pass --force to overwrite it",
            path.display()
        );
    }
    let body =
        workflow_body_with_frontmatter(linear_project_slug, linear_project, expose_graphql_tool);
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    // `dar init-build` owns the full agent `.gitignore` (see
    // dar-cli composer::ensure_agent_gitignore). We still ensure `.env` here so
    // a standalone `init_workflow` never leaves secrets un-ignored; the helper
    // is idempotent, so init-build will not duplicate this entry.
    ensure_gitignore_entry(root, ".env")?;
    println!("wrote {}", path.display());
    Ok(())
}

pub const DEFAULT_WORKFLOW_MD_BODY: &str = r#"You are working on issue {{ issue.identifier }}: {{ issue.title }}

{{ issue.description }}

These instructions are prompt-level worker guidance. They describe what you must do; they are not daemon or orchestrator behavior.

## Required Claim Step

Before doing task work:

1. Fetch the Linear issue {{ issue.identifier }}.
2. If its current state is `Todo`, move it to `In Progress`.
3. Read all Linear issue comments and incorporate any updated requirements.
4. Add or update one concise Linear comment saying you are working on the issue.
5. Continue only after those Linear updates succeed.

Keep that same Linear comment updated with progress, validation results, blockers, and the final handoff. Do not create a noisy comment stream.

## Dependencies

Before coding, inspect the Linear issue's dependencies. Fetch each `blockedBy` issue and confirm it is in a terminal/completed state such as `Done`, `Closed`, `Cancelled`, `Canceled`, or `Duplicate`. If any blocker is incomplete, update the Linear comment with the blocker and stop without coding.

For completed blockers, read their comments for prior workspace, branch, commit, and PR notes. If a completed dependency has an available workspace or branch, base your work on it so changes stack instead of diverging.

When this issue is a sub-issue of a parent that has other sub-issues, stack on already-resolved sibling work. Use one PR per parent: if a PR already exists for the parent, push your work onto that branch and update that PR; if no PR exists yet, create one.

## Workspace

Work only inside this issue workspace. If repositories or extra checkouts are needed, clone or create them inside this workspace unless they already exist here.

For code changes, create a git worktree from the correct base branch: a completed dependency's branch/workspace when available, otherwise the repository's main branch unless the issue says otherwise.

## Review And PR Flow

When code changes are needed:

1. Make the focused change in the issue worktree.
2. Spawn a reviewer subagent and ask it to review the code changes.
3. Do not commit until the reviewer comes back clean.
4. After a clean review, commit the work in the worktree.
5. Create or update the GitHub PR using `gh`.
6. Link the PR to the Linear issue.
7. Move the issue to `In Review`.

## Git Usage

Always use non-interactive git. Never run a git (or any) command that opens an editor or waits on a TTY/stdin — it will hang the run until the stall guard parks it. Use `git commit -m "..."` or `git commit --no-edit`; for rebases, run `git rebase --continue` only after staging changes (the environment already neutralizes `GIT_EDITOR`/`GIT_SEQUENCE_EDITOR` so it will not block on a message editor). Avoid `git commit --amend` without `--no-edit`, interactive rebase (`git rebase -i`), and any command that pages output or prompts for credentials.

## Blockers

If requirements, ownership, base branch, dependency state, credentials, or validation risk are unclear, ask for human input instead of guessing. Update the Linear comment with the blocker, what you tried, and the decision needed, then move the issue to `Needs Human` and stop.

## Completion

Validate the change before handoff. When the task is complete, leave the issue out of active states: move it to `In Review` when work is done and a PR is open or updated, or to a terminal state only when the workflow explicitly calls for it."#;

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
    linear_project_slug: Option<&str>,
    linear_project: Option<&str>,
    expose_graphql_tool: bool,
) -> String {
    if linear_project_slug.is_none() && linear_project.is_none() && !expose_graphql_tool {
        return format!("{DEFAULT_WORKFLOW_MD_BODY}\n");
    }

    let mut out = String::from("---\n");
    if let Some(slug) = linear_project_slug {
        out.push_str("tracker:\n");
        out.push_str("  kind: linear\n");
        out.push_str(&format!("  project_slug: {}\n", yaml_string(slug)));
    }
    if linear_project.is_some() || expose_graphql_tool {
        out.push_str("linear:\n");
        if let Some(project) = linear_project {
            out.push_str(&format!("  project: {}\n", yaml_string(project)));
        }
        if expose_graphql_tool {
            out.push_str("  exposeGraphqlTool: true\n");
        }
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

pub struct LinearTracker {
    client: reqwest::Client,
    endpoint: String,
    /// The `Authorization` header value. Wrapped in an `RwLock` so
    /// [`LinearTracker::reload_secrets`] can swap in a rotated token at runtime
    /// without rebuilding the tracker (read on every request).
    api_key: RwLock<String>,
    project_slug: String,
    team: Option<String>,
    assignee_id: Option<String>,
    delegate_id: Option<String>,
    labels: Vec<String>,
    active: Vec<String>,
    terminal: Vec<String>,
    needs_human: Option<String>,
    /// Minimum `x-ratelimit-requests-remaining` observed across all requests.
    min_remaining: Arc<AtomicI64>,
}

impl LinearTracker {
    pub fn new(cfg: LinearTrackerConfig) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("building reqwest client for LinearTracker")?;
        Ok(Self {
            client,
            endpoint: if cfg.endpoint.is_empty() {
                DEFAULT_ENDPOINT.to_string()
            } else {
                cfg.endpoint
            },
            api_key: RwLock::new(cfg.api_key),
            project_slug: cfg.project_slug,
            team: cfg.team,
            assignee_id: cfg.assignee_id,
            delegate_id: cfg.delegate_id,
            labels: cfg.labels,
            active: cfg.active_states,
            terminal: cfg.terminal_states,
            needs_human: cfg.needs_human,
            min_remaining: Arc::new(AtomicI64::new(UNSET_MIN)),
        })
    }

    /// Current `Authorization` header value (cloned for a single request).
    fn auth_header(&self) -> String {
        self.api_key
            .read()
            .expect("LinearTracker api_key lock poisoned")
            .clone()
    }

    /// Re-resolve the Linear auth header from the environment and swap the
    /// cached token in place. Callers reload `.env` first (so the env holds the
    /// rotated token); this refreshes the already-built tracker without a
    /// rebuild. Returns `true` when the token changed.
    ///
    /// A now-missing token resolves to an empty header (same as construction),
    /// surfacing as a 401 rather than silently keeping the stale value.
    pub fn reload_secrets(&self) -> bool {
        let next = resolve_linear_auth_header().unwrap_or_default();
        let mut guard = self
            .api_key
            .write()
            .expect("LinearTracker api_key lock poisoned");
        if *guard == next {
            return false;
        }
        *guard = next;
        true
    }

    // --- async internals ---

    /// Resolved filter dimensions for this tracker (project_slug "" → None).
    fn dims(&self) -> ResolvedDims {
        ResolvedDims {
            project_slug: Some(self.project_slug.clone()).filter(|s| !s.is_empty()),
            team_key: self.team.clone(),
            assignee_id: self.assignee_id.clone(),
            delegate_id: self.delegate_id.clone(),
            labels: self.labels.clone(),
        }
    }

    /// Fetch every Linear user (all pages) to resolve configured user filters.
    async fn fetch_users_async(&self) -> Result<Vec<LinearUser>> {
        let query = r#"
query DarUsers($after: String, $first: Int!) {
  users(first: $first, after: $after) {
    pageInfo { hasNextPage endCursor }
    nodes { id name displayName email }
  }
}
"#;
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let vars = json!({ "after": cursor, "first": 250 });
            let body = json!({ "query": query, "variables": vars });
            let response = self.send_with_rate_limit_async(body).await?;
            let conn = response
                .pointer("/data/users")
                .ok_or_else(|| anyhow!("missing users in Linear response"))?;
            for node in conn["nodes"].as_array().cloned().unwrap_or_default() {
                if let Ok(u) = serde_json::from_value::<LinearUser>(node) {
                    all.push(u);
                }
            }
            if conn
                .pointer("/pageInfo/hasNextPage")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                cursor = conn
                    .pointer("/pageInfo/endCursor")
                    .and_then(Value::as_str)
                    .map(String::from);
            } else {
                break;
            }
        }
        Ok(all)
    }

    /// Fetch every issue matching `filter` (AND across dimensions), paginated.
    async fn fetch_issues_async(&self, filter: &Value) -> Result<Vec<RawIssue>> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = self.fetch_page_async(filter, cursor.as_deref()).await?;
            all.extend(page.issues);
            if page.has_next_page {
                cursor = page.end_cursor;
            } else {
                break;
            }
        }
        Ok(all)
    }

    /// All issues in the configured dimension, regardless of state.
    async fn fetch_all_in_dimension(&self) -> Result<Vec<RawIssue>> {
        self.fetch_issues_async(&build_issue_filter(&self.dims(), &[]))
            .await
    }

    async fn fetch_page_async(&self, filter: &Value, after: Option<&str>) -> Result<IssuePage> {
        let query = r#"
query DarCandidates($filter: IssueFilter, $after: String, $first: Int!) {
  issues(filter: $filter, first: $first, after: $after) {
    pageInfo { hasNextPage endCursor }
    nodes {
      id
      identifier
      title
      description
      url
      priority
      createdAt
      updatedAt
      state { name type }
      assignee { id displayName }
      delegate { id displayName name email }
      labels { nodes { name } }
      project { name slugId }
      parent { id identifier }
      inverseRelations(first: 50) {
        nodes { type issue { id identifier } }
      }
    }
  }
}
"#;
        let vars = json!({ "filter": filter, "after": after, "first": PAGE_SIZE });
        let body = json!({ "query": query, "variables": vars });
        let response = self.send_with_rate_limit_async(body).await?;
        let issues_obj = response
            .pointer("/data/issues")
            .ok_or_else(|| anyhow!("missing 'issues' node in Linear response"))?;
        let has_next_page = issues_obj
            .pointer("/pageInfo/hasNextPage")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let end_cursor = issues_obj
            .pointer("/pageInfo/endCursor")
            .and_then(Value::as_str)
            .map(String::from);
        let nodes = issues_obj["nodes"].as_array().cloned().unwrap_or_default();
        let mut issues = Vec::with_capacity(nodes.len());
        for node in nodes {
            match serde_json::from_value::<RawIssue>(node) {
                Ok(ri) => issues.push(ri),
                Err(e) => tracing::warn!("LinearTracker: skipping unparseable issue node: {e}"),
            }
        }
        Ok(IssuePage {
            issues,
            has_next_page,
            end_cursor,
        })
    }

    /// Execute one GraphQL request. Handles:
    /// - Rate-limit header tracking for both requests and complexity dimensions.
    /// - Sleep until reset + 1 s when remaining ≤ 0 on either dimension.
    /// - HTTP 429: sleep for `Retry-After` (or 60 s), retry once.
    async fn send_with_rate_limit_async(&self, body: Value) -> Result<Value> {
        let resp = self
            .client
            .post(&self.endpoint)
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("sending Linear GraphQL request")?;

        if resp.status().as_u16() == 429 {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(60);
            tracing::warn!(
                retry_after_secs = retry_after,
                "Linear rate limited (429); sleeping before retry"
            );
            tokio::time::sleep(Duration::from_secs(retry_after)).await;

            // Retry once.
            let resp2 = self
                .client
                .post(&self.endpoint)
                .header("Authorization", self.auth_header())
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await
                .context("sending Linear GraphQL request (retry after 429)")?;

            self.process_rate_limit_headers(&resp2.headers().clone())
                .await;
            let status = resp2.status();
            let text = resp2
                .text()
                .await
                .context("reading Linear response body (retry)")?;
            if !status.is_success() {
                bail!(
                    "Linear API returned HTTP {} after retry: {}",
                    status,
                    &text[..text.len().min(200)]
                );
            }
            return parse_graphql_body(&text);
        }

        // Capture headers before consuming the response body.
        let headers = resp.headers().clone();
        let status = resp.status();
        let text = resp.text().await.context("reading Linear response body")?;

        self.process_rate_limit_headers(&headers).await;

        if !status.is_success() {
            bail!(
                "Linear API returned HTTP {}: {}",
                status,
                &text[..text.len().min(200)]
            );
        }
        parse_graphql_body(&text)
    }

    /// Record rate-limit headers into `min_remaining` and sleep when a bucket is
    /// exhausted. Handles both the requests dimension
    /// (`x-ratelimit-requests-remaining` / `x-ratelimit-requests-reset`) and the
    /// complexity dimension (`x-ratelimit-complexity-remaining` /
    /// `x-ratelimit-complexity-reset`). Whichever dimension reports remaining ≤ 0
    /// first triggers the sleep; both are checked.
    async fn process_rate_limit_headers(&self, headers: &reqwest::header::HeaderMap) {
        let dims: &[(&str, &str, &str)] = &[
            (
                "requests",
                "x-ratelimit-requests-remaining",
                "x-ratelimit-requests-reset",
            ),
            (
                "complexity",
                "x-ratelimit-complexity-remaining",
                "x-ratelimit-complexity-reset",
            ),
        ];

        for (label, remaining_hdr, reset_hdr) in dims {
            let remaining = headers
                .get(*remaining_hdr)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<i64>().ok());

            let reset_ts = headers
                .get(*reset_hdr)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<i64>().ok());

            if let Some(r) = remaining {
                // Track minimum across both dimensions using fetch_min (stable since Rust 1.45).
                self.min_remaining.fetch_min(r, Ordering::SeqCst);

                if r <= 0 {
                    let wait_secs = reset_ts
                        // Linear returns epoch milliseconds; convert to seconds before diffing.
                        .map(|ts_ms| (ts_ms / 1000 - Utc::now().timestamp() + 1).max(0) as u64)
                        .unwrap_or(60);
                    if wait_secs > 0 {
                        tracing::warn!(
                            wait_secs,
                            dimension = *label,
                            "Linear rate limit exhausted; sleeping until bucket resets"
                        );
                        tokio::time::sleep(Duration::from_secs(wait_secs)).await;
                    }
                }
            }
        }
    }

    // --- sync wrappers (bridge to the sync Tracker trait) ---

    fn run_async<F, T>(&self, fut: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
    }

    fn poll_candidates_inner(&self) -> Result<Vec<Issue>> {
        self.run_async(self.do_poll_candidates())
    }

    async fn do_poll_candidates(&self) -> Result<Vec<Issue>> {
        // Fetch the full in-dimension set (all states) so blocked-by can see
        // dependencies in any non-terminal state, not just active ones. Active
        // filtering is applied client-side below.
        let raw = self.fetch_all_in_dimension().await?;

        // identifier -> state name, over the in-dimension set. A blocker absent
        // from this map is outside the configured dimension and is ignored
        // (treated as not blocking), consistent with the prior cross-project rule.
        let state_map: std::collections::HashMap<&str, &str> = raw
            .iter()
            .map(|r| (r.identifier.as_str(), r.state.name.as_str()))
            .collect();

        let mut out = Vec::new();
        for r in &raw {
            if !self.active.contains(&r.state.name) {
                continue;
            }
            let blocker_ids: Vec<&str> = r
                .blocked_by()
                .iter()
                .map(|b| b.identifier.as_str())
                .collect();
            if !is_blocked(&blocker_ids, &state_map, &self.terminal) {
                out.push(raw_to_issue(r));
            }
        }
        Ok(out)
    }

    fn fetch_all_inner(&self) -> Result<Vec<Issue>> {
        self.run_async(async {
            let raw = self.fetch_all_in_dimension().await?;
            Ok(raw.iter().map(raw_to_issue).collect())
        })
    }

    pub fn export_snapshot(&self) -> Result<LinearExport> {
        self.run_async(async {
            let raw = self.fetch_all_in_dimension().await?;
            let issues: Vec<Issue> = raw.iter().map(raw_to_issue).collect();
            let (pname, pslug) = raw
                .first()
                .and_then(|r| r.project.as_ref())
                .map(|p| (p.name.clone(), p.slug_id.clone()))
                .unwrap_or((None, None));
            Ok(LinearExport {
                project: LinearProjectExport {
                    name: pname,
                    slug: pslug.unwrap_or_else(|| self.project_slug.clone()),
                    endpoint: self.endpoint.clone(),
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
        self.run_async(self.do_park_issue_needs_human(issue, needs_human, comment))
    }

    async fn do_park_issue_needs_human(
        &self,
        issue: &Issue,
        needs_human: &str,
        comment: &str,
    ) -> Result<()> {
        let state_id = self
            .fetch_issue_team_state_id(&issue.id, needs_human)
            .await
            .with_context(|| {
                format!(
                    "resolving Linear state {needs_human:?} for issue {}",
                    issue.identifier
                )
            })?;

        let mutation = r#"
mutation DarParkIssue($issueId: String!, $stateId: String!, $body: String!) {
  issueUpdate(id: $issueId, input: { stateId: $stateId }) {
    success
  }
  commentCreate(input: { issueId: $issueId, body: $body }) {
    success
  }
}
"#;
        let vars = json!({
            "issueId": issue.id,
            "stateId": state_id,
            "body": comment,
        });
        let body = json!({ "query": mutation, "variables": vars });
        let response = self.send_with_rate_limit_async(body).await?;
        ensure_success(&response, "/data/issueUpdate/success", "issueUpdate")?;
        ensure_success(&response, "/data/commentCreate/success", "commentCreate")?;
        Ok(())
    }

    /// Fetch a single issue by UUID or identifier (e.g. "ALG-123") using the
    /// targeted `issue(id:)` query instead of a full paginated project scan.
    async fn fetch_one_issue_async(&self, id: &str) -> Result<Option<Issue>> {
        let query = r#"
query DarFetchOne($id: String!) {
  issue(id: $id) {
    id
    identifier
    title
    description
    url
    priority
    createdAt
    updatedAt
    state { name type }
    assignee { id displayName }
    delegate { id displayName name email }
    labels { nodes { name } }
    parent { id identifier }
    inverseRelations(first: 50) {
      nodes {
        type
        issue { id identifier }
      }
    }
    project { name slugId }
  }
}
"#;
        let vars = json!({ "id": id });
        let body = json!({ "query": query, "variables": vars });
        let response = self.send_with_rate_limit_async(body).await?;

        // Linear returns `"issue": null` when not found — treat as Ok(None).
        let node = match response.pointer("/data/issue") {
            None | Some(Value::Null) => return Ok(None),
            Some(n) => n.clone(),
        };

        let raw: RawIssue =
            serde_json::from_value(node).context("parsing Linear issue node in fetch_one")?;
        Ok(Some(raw_to_issue(&raw)))
    }

    async fn fetch_issue_team_state_id(&self, issue_id: &str, state_name: &str) -> Result<String> {
        let query = r#"
query DarNeedsHumanState($issueId: String!, $stateName: String!) {
  issue(id: $issueId) {
    team {
      states(filter: { name: { eq: $stateName } }, first: 1) {
        nodes { id name }
      }
    }
  }
}
"#;
        let vars = json!({
            "issueId": issue_id,
            "stateName": state_name,
        });
        let body = json!({ "query": query, "variables": vars });
        let response = self.send_with_rate_limit_async(body).await?;
        response
            .pointer("/data/issue/team/states/nodes/0/id")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| anyhow!("Linear state {state_name:?} not found on issue team"))
    }
}

impl Tracker for LinearTracker {
    fn poll_candidates(&self) -> Result<Vec<Issue>> {
        self.poll_candidates_inner()
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
        self.run_async(self.fetch_one_issue_async(id))
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

    fn reload_secrets(&self) -> bool {
        LinearTracker::reload_secrets(self)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_graphql_body(text: &str) -> Result<Value> {
    let v: Value = serde_json::from_str(text).context("parsing Linear GraphQL JSON response")?;
    if let Some(errors) = v.get("errors") {
        if !errors.as_array().map(|a| a.is_empty()).unwrap_or(true) {
            bail!("Linear GraphQL errors: {}", errors);
        }
    }
    Ok(v)
}

fn ensure_success(response: &Value, pointer: &str, label: &str) -> Result<()> {
    if response.pointer(pointer).and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        bail!("Linear {label} mutation did not return success=true")
    }
}

fn raw_to_issue(r: &RawIssue) -> Issue {
    let mut builder = Issue::builder(
        r.id.clone(),
        r.identifier.clone(),
        r.title.clone(),
        r.state.name.clone(),
    )
    .description(r.description.clone())
    .url(r.url.clone())
    .priority(r.priority)
    .assignees(
        r.assignee
            .as_ref()
            .map(|a| vec![a.display_name.clone().unwrap_or_else(|| a.id.clone())])
            .unwrap_or_default(),
    )
    .labels(r.labels.nodes.iter().map(|l| l.name.clone()).collect())
    .created_at(r.created_at)
    .updated_at(r.updated_at)
    .parent_id(r.parent.as_ref().map(|p| p.id.clone()))
    .blocked_by(
        r.blocked_by()
            .iter()
            .map(|b| b.identifier.clone())
            .collect(),
    )
    .project_name(r.project.as_ref().and_then(|p| p.name.clone()))
    .project_slug(r.project.as_ref().and_then(|p| p.slug_id.clone()));

    if let Some(delegate) = &r.delegate {
        builder = builder.metadata_entry(
            "linear_delegate",
            json!({
                "id": delegate.id,
                "displayName": delegate.display_name,
                "name": delegate.name,
                "email": delegate.email,
            }),
        );
    }

    builder.build()
}

// ---------------------------------------------------------------------------
// Filter dimensions
// ---------------------------------------------------------------------------

/// Resolved filter dimensions for one tracker. Empty fields = unconstrained.
#[derive(Debug, Clone, Default)]
struct ResolvedDims {
    project_slug: Option<String>,
    team_key: Option<String>,
    assignee_id: Option<String>,
    delegate_id: Option<String>,
    labels: Vec<String>,
}

impl ResolvedDims {
    /// True when no dimension constrains the poll (whole-workspace risk).
    fn is_empty(&self) -> bool {
        self.project_slug.is_none()
            && self.team_key.is_none()
            && self.assignee_id.is_none()
            && self.delegate_id.is_none()
            && self.labels.is_empty()
    }
}

/// Build a Linear `IssueFilter` (AND across dimensions). `active_states`, when
/// non-empty, adds a `state.name.in` clause. Pure: no I/O.
fn build_issue_filter(dims: &ResolvedDims, active_states: &[String]) -> Value {
    let mut and: Vec<Value> = Vec::new();
    if let Some(slug) = dims.project_slug.as_deref().filter(|s| !s.is_empty()) {
        and.push(json!({ "project": { "slugId": { "eq": slug } } }));
    }
    if let Some(key) = dims.team_key.as_deref().filter(|s| !s.is_empty()) {
        and.push(json!({ "team": { "key": { "eq": key } } }));
    }
    if let Some(id) = dims.assignee_id.as_deref().filter(|s| !s.is_empty()) {
        and.push(json!({ "assignee": { "id": { "eq": id } } }));
    }
    if let Some(id) = dims.delegate_id.as_deref().filter(|s| !s.is_empty()) {
        and.push(json!({ "delegate": { "id": { "eq": id } } }));
    }
    if !dims.labels.is_empty() {
        and.push(json!({ "labels": { "some": { "name": { "in": dims.labels } } } }));
    }
    if !active_states.is_empty() {
        and.push(json!({ "state": { "name": { "in": active_states } } }));
    }
    json!({ "and": and })
}

/// A candidate is blocked iff one of its blockers is an in-dimension issue
/// (present in `state_map`) whose state is not terminal. Blockers outside the
/// configured dimension are absent from the map and treated as not blocking.
fn is_blocked(
    blocker_ids: &[&str],
    state_map: &std::collections::HashMap<&str, &str>,
    terminal: &[String],
) -> bool {
    blocker_ids.iter().any(|b| {
        state_map
            .get(b)
            .map(|s| !terminal.iter().any(|t| t == s))
            .unwrap_or(false)
    })
}

// ---------------------------------------------------------------------------
// User resolution
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct LinearUser {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "displayName")]
    display_name: Option<String>,
    #[serde(default)]
    email: Option<String>,
}

fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, &c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Resolve a raw user config value to a canonical Linear user id.
/// UUID passes through. Otherwise matches displayName / name / email
/// case-insensitively (a leading `@` is stripped). Zero or multiple matches
/// are errors (fail fast at boot). Pure: no I/O.
fn resolve_user_id(raw: &str, users: &[LinearUser], field: &str) -> Result<String> {
    let needle = raw.trim().strip_prefix('@').unwrap_or_else(|| raw.trim());
    if is_uuid(needle) {
        return Ok(needle.to_string());
    }
    let matches: std::collections::BTreeSet<String> = users
        .iter()
        .filter(|u| {
            let eq =
                |f: &Option<String>| f.as_deref().is_some_and(|v| v.eq_ignore_ascii_case(needle));
            eq(&u.display_name) || eq(&u.name) || eq(&u.email)
        })
        .map(|u| u.id.clone())
        .collect();
    match matches.len() {
        0 => bail!("{field} {needle:?} matched no Linear user (by displayName, name, or email)"),
        1 => Ok(matches.into_iter().next().unwrap()),
        n => bail!("{field} {needle:?} is ambiguous: matched {n} Linear users"),
    }
}

// ---------------------------------------------------------------------------
// Serde structs for Linear GraphQL response
// ---------------------------------------------------------------------------

struct IssuePage {
    issues: Vec<RawIssue>,
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawIssue {
    id: String,
    identifier: String,
    title: String,
    description: Option<String>,
    url: Option<String>,
    priority: Option<i32>,
    #[serde(rename = "createdAt")]
    created_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(rename = "updatedAt")]
    updated_at: Option<chrono::DateTime<chrono::Utc>>,
    state: RawState,
    #[serde(default)]
    assignee: Option<RawAssignee>,
    #[serde(default)]
    delegate: Option<RawDelegate>,
    labels: RawLabelConnection,
    #[serde(default)]
    project: Option<RawProjectRef>,
    parent: Option<RawRef>,
    #[serde(rename = "inverseRelations", default)]
    inverse_relations: RawRelationConnection,
}

#[derive(Debug, Deserialize)]
struct RawAssignee {
    id: String,
    #[serde(rename = "displayName")]
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawDelegate {
    id: String,
    #[serde(default, rename = "displayName")]
    display_name: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    email: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawProjectRef {
    name: Option<String>,
    #[serde(rename = "slugId")]
    slug_id: Option<String>,
}

impl RawIssue {
    fn blocked_by(&self) -> Vec<&RawRef> {
        self.inverse_relations
            .nodes
            .iter()
            .filter(|r| r.kind == "blocks")
            .map(|r| &r.issue)
            .collect()
    }
}

#[derive(Debug, Deserialize)]
struct RawState {
    name: String,
    #[allow(dead_code)]
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Debug, Default, Deserialize)]
struct RawLabelConnection {
    nodes: Vec<RawLabel>,
}

#[derive(Debug, Deserialize)]
struct RawLabel {
    name: String,
}

/// Wrapper for GraphQL relation connections that return `{ nodes: [...] }`.
#[derive(Debug, Default, Deserialize)]
struct RawRelationConnection {
    nodes: Vec<RawRelation>,
}

#[derive(Debug, Deserialize)]
struct RawRelation {
    #[serde(rename = "type")]
    kind: String,
    issue: RawRef,
}

#[derive(Debug, Deserialize)]
struct RawRef {
    #[allow(dead_code)]
    id: String,
    identifier: String,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

    use super::{
        resolve_linear_auth_header, LinearTracker, LinearTrackerConfig, RawIssue, API_KEY_ENV,
        OAUTH_TOKEN_ENV, UNSET_MIN,
    };

    /// Serializes the env-var mutation tests below: they share process env.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn auth_header_prefers_oauth_token_with_bearer_prefix() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var(OAUTH_TOKEN_ENV, "oauth-abc");
        std::env::set_var(API_KEY_ENV, "lin_api_xyz");
        assert_eq!(
            resolve_linear_auth_header().as_deref(),
            Some("Bearer oauth-abc")
        );
        std::env::remove_var(OAUTH_TOKEN_ENV);
        std::env::remove_var(API_KEY_ENV);
    }

    #[test]
    fn auth_header_uses_raw_api_key_when_no_oauth_token() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(OAUTH_TOKEN_ENV);
        std::env::set_var(API_KEY_ENV, "lin_api_xyz");
        assert_eq!(resolve_linear_auth_header().as_deref(), Some("lin_api_xyz"));
        std::env::remove_var(API_KEY_ENV);
    }

    #[test]
    fn auth_header_none_when_unset_or_empty() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(OAUTH_TOKEN_ENV);
        std::env::remove_var(API_KEY_ENV);
        assert_eq!(resolve_linear_auth_header(), None);
        std::env::set_var(OAUTH_TOKEN_ENV, "");
        std::env::set_var(API_KEY_ENV, "");
        assert_eq!(resolve_linear_auth_header(), None);
        std::env::remove_var(OAUTH_TOKEN_ENV);
        std::env::remove_var(API_KEY_ENV);
    }

    #[test]
    fn reload_secrets_swaps_cached_token_from_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(OAUTH_TOKEN_ENV);
        std::env::set_var(API_KEY_ENV, "lin_old");

        let tracker = LinearTracker::new(LinearTrackerConfig {
            endpoint: "http://localhost".to_string(),
            api_key: resolve_linear_auth_header().unwrap_or_default(),
            project_slug: "test".to_string(),
            team: None,
            assignee_id: None,
            delegate_id: None,
            labels: vec![],
            active_states: vec![],
            terminal_states: vec![],
            needs_human: None,
        })
        .unwrap();
        assert_eq!(tracker.auth_header(), "lin_old");

        // No change yet → reload is a no-op.
        assert!(!tracker.reload_secrets());
        assert_eq!(tracker.auth_header(), "lin_old");

        // Rotate the env token; reload swaps it in without a rebuild.
        std::env::set_var(API_KEY_ENV, "lin_new");
        assert!(tracker.reload_secrets());
        assert_eq!(tracker.auth_header(), "lin_new");

        // An OAuth token takes precedence and is sent with the Bearer prefix.
        std::env::set_var(OAUTH_TOKEN_ENV, "oauth_tok");
        assert!(tracker.reload_secrets());
        assert_eq!(tracker.auth_header(), "Bearer oauth_tok");

        std::env::remove_var(OAUTH_TOKEN_ENV);
        std::env::remove_var(API_KEY_ENV);
    }

    fn make_tracker() -> LinearTracker {
        LinearTracker::new(LinearTrackerConfig {
            endpoint: "http://localhost".to_string(),
            api_key: "test".to_string(),
            project_slug: "test".to_string(),
            team: None,
            assignee_id: None,
            delegate_id: None,
            labels: vec![],
            active_states: vec![],
            terminal_states: vec![],
            needs_human: None,
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

    /// Helper that runs the async header-processing on a fresh tracker and returns
    /// the resulting min_remaining value.
    fn process_headers_sync(headers: &HeaderMap) -> i64 {
        let tracker = make_tracker();
        // Use a single-threaded tokio runtime so we don't need a full multi-thread
        // runtime just for header parsing (no actual sleep occurs when remaining > 0).
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(tracker.process_rate_limit_headers(headers));
        tracker.min_remaining.load(Ordering::SeqCst)
    }

    #[test]
    fn requests_dimension_is_tracked() {
        let headers = header_map(&[("x-ratelimit-requests-remaining", "42")]);
        assert_eq!(process_headers_sync(&headers), 42);
    }

    #[test]
    fn complexity_dimension_is_tracked() {
        let headers = header_map(&[("x-ratelimit-complexity-remaining", "17")]);
        assert_eq!(process_headers_sync(&headers), 17);
    }

    #[test]
    fn min_of_both_dimensions_is_recorded() {
        // complexity (10) is more constrained than requests (100) — min should be 10.
        let headers = header_map(&[
            ("x-ratelimit-requests-remaining", "100"),
            ("x-ratelimit-complexity-remaining", "10"),
        ]);
        assert_eq!(process_headers_sync(&headers), 10);
    }

    #[test]
    fn no_rate_limit_headers_leaves_unset() {
        let headers = header_map(&[]);
        assert_eq!(process_headers_sync(&headers), UNSET_MIN);
    }

    #[test]
    fn rate_limit_remaining_returns_none_before_any_request() {
        use super::Tracker;
        let tracker = make_tracker();
        assert_eq!(tracker.rate_limit_remaining(), None);
    }

    #[test]
    fn inverse_blocks_relations_become_blockers() {
        let issue: RawIssue = serde_json::from_str(
            r#"{
              "id": "issue-b",
              "identifier": "ALG-2",
              "title": "Blocked issue",
              "description": null,
              "url": null,
              "priority": null,
              "createdAt": null,
              "updatedAt": null,
              "state": { "name": "Todo", "type": "unstarted" },
              "labels": { "nodes": [] },
              "parent": null,
              "inverseRelations": {
                "nodes": [
                  {
                    "type": "blocks",
                    "issue": {
                      "id": "issue-a",
                      "identifier": "ALG-1",
                      "state": { "name": "In Progress", "type": "started" }
                    }
                  },
                  {
                    "type": "related",
                    "issue": {
                      "id": "issue-c",
                      "identifier": "ALG-3",
                      "state": { "name": "Todo", "type": "unstarted" }
                    }
                  }
                ]
              }
            }"#,
        )
        .unwrap();

        let blockers = issue.blocked_by();
        assert_eq!(blockers.len(), 1);
        assert_eq!(blockers[0].identifier, "ALG-1");
        assert_eq!(super::raw_to_issue(&issue).blocked_by, vec!["ALG-1"]);
    }

    #[test]
    fn raw_graphql_issue_maps_to_portable_issue() {
        let issue: RawIssue = serde_json::from_str(
            r#"{
              "id": "issue-1",
              "identifier": "ALG-1",
              "title": "Move tracker",
              "description": "Details",
              "url": "https://linear.app/algodyn/issue/ALG-1",
              "priority": 2,
              "createdAt": "2026-06-11T10:00:00Z",
              "updatedAt": "2026-06-11T11:00:00Z",
              "state": { "name": "Todo", "type": "unstarted" },
              "assignee": { "id": "u1", "displayName": "thinh" },
              "delegate": { "id": "u2", "displayName": "workeragent", "name": "Worker Agent", "email": "workeragent@linear.app" },
              "labels": { "nodes": [{ "name": "backend" }] },
              "project": { "name": "Dar", "slugId": "dar" },
              "parent": { "id": "parent-1", "identifier": "ALG-0" },
              "inverseRelations": { "nodes": [] }
            }"#,
        )
        .unwrap();

        let mapped = super::raw_to_issue(&issue);

        assert_eq!(mapped.id, "issue-1");
        assert_eq!(mapped.identifier, "ALG-1");
        assert_eq!(mapped.title, "Move tracker");
        assert_eq!(mapped.description.as_deref(), Some("Details"));
        assert_eq!(
            mapped.url.as_deref(),
            Some("https://linear.app/algodyn/issue/ALG-1")
        );
        assert_eq!(mapped.state, "Todo");
        assert_eq!(mapped.priority, Some(2));
        assert_eq!(mapped.assignees, vec!["thinh"]);
        assert_eq!(
            mapped.metadata["linear_delegate"],
            json!({
                "id": "u2",
                "displayName": "workeragent",
                "name": "Worker Agent",
                "email": "workeragent@linear.app"
            })
        );
        assert_eq!(mapped.labels, vec!["backend"]);
        assert_eq!(mapped.parent_id.as_deref(), Some("parent-1"));
        assert_eq!(mapped.project_name.as_deref(), Some("Dar"));
        assert_eq!(mapped.project_slug.as_deref(), Some("dar"));
    }

    #[test]
    fn linear_preserves_native_candidate_order() {
        use super::Tracker;
        let tracker = make_tracker();
        assert!(!tracker.sort_candidates_locally());
    }

    #[test]
    fn default_workflow_body_contains_standard_worker_procedure() {
        let body = super::DEFAULT_WORKFLOW_MD_BODY;
        assert!(body.contains("## Required Claim Step"));
        assert!(body.contains("Fetch the Linear issue {{ issue.identifier }}"));
        assert!(body.contains("If its current state is `Todo`, move it to `In Progress`"));
        assert!(body.contains("Read all Linear issue comments"));
        assert!(body.contains("Add or update one concise Linear comment"));
        assert!(body.contains("Continue only after those Linear updates succeed"));
        assert!(body.contains("## Dependencies"));
        assert!(body.contains("Fetch each `blockedBy` issue"));
        assert!(body.contains("base your work on it so changes stack instead of diverging"));
        assert!(body.contains("one PR per parent"));
        assert!(body.contains("## Workspace"));
        assert!(body.contains("Work only inside this issue workspace"));
        assert!(body.contains("## Review And PR Flow"));
        assert!(body.contains("Spawn a reviewer subagent"));
        assert!(body.contains("Do not commit until the reviewer comes back clean"));
        assert!(body.contains("Link the PR to the Linear issue"));
        assert!(body.contains("## Git Usage"));
        assert!(body.contains("Always use non-interactive git"));
        assert!(body.contains("## Blockers"));
        assert!(body.contains("move the issue to `Needs Human` and stop"));
        assert!(body.contains("## Completion"));
        assert!(body.contains("move it to `In Review` when work is done"));
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
    fn init_workflow_can_seed_linear_frontmatter_without_agent_yaml() {
        let dir = tempfile::tempdir().unwrap();
        super::init_workflow_with_options(dir.path(), false, Some("dar"), Some("Dar"), true)
            .unwrap();

        let body = std::fs::read_to_string(dir.path().join("WORKFLOW.md")).unwrap();
        assert!(body.starts_with("---\n"));
        assert!(body.contains("  kind: linear\n"));
        assert!(body.contains("  project_slug: dar\n"));
        assert!(body.contains("  project: Dar\n"));
        assert!(body.contains("  exposeGraphqlTool: true\n"));
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

    #[test]
    fn write_snapshot_writes_project_and_issues_under_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let agent_cfg = super::ExportAgentConfig {
            id: "agent-1".to_string(),
            name: "Agent One".to_string(),
            tracker: super::ExportTrackerConfig {
                use_: "linear".to_string(),
                active_states: vec!["Todo".to_string()],
                terminal_states: vec!["Done".to_string()],
                project_slug: Some("proj".to_string()),
                endpoint: None,
                needs_human: None,
            },
        };
        let snapshot = super::LinearExport {
            project: super::LinearProjectExport {
                name: Some("Project".to_string()),
                slug: "proj".to_string(),
                endpoint: "https://api.linear.app/graphql".to_string(),
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

    // --- Filter builder ---

    use super::{
        build_issue_filter, is_blocked, is_uuid, resolve_user_id, LinearUser, ResolvedDims,
    };
    use serde_json::json;

    fn dims_project() -> ResolvedDims {
        ResolvedDims {
            project_slug: Some("dar".into()),
            ..Default::default()
        }
    }

    #[test]
    fn filter_project_only_with_active_states() {
        let active = vec!["Todo".to_string()];
        let f = build_issue_filter(&dims_project(), &active);
        assert_eq!(
            f,
            json!({ "and": [
                { "project": { "slugId": { "eq": "dar" } } },
                { "state": { "name": { "in": ["Todo"] } } }
            ]})
        );
    }

    #[test]
    fn filter_team_only() {
        let dims = ResolvedDims {
            team_key: Some("ALG".into()),
            ..Default::default()
        };
        let f = build_issue_filter(&dims, &[]);
        assert_eq!(
            f,
            json!({ "and": [{ "team": { "key": { "eq": "ALG" } } }] })
        );
    }

    #[test]
    fn filter_assignee_only() {
        let dims = ResolvedDims {
            assignee_id: Some("u1".into()),
            ..Default::default()
        };
        let f = build_issue_filter(&dims, &[]);
        assert_eq!(
            f,
            json!({ "and": [{ "assignee": { "id": { "eq": "u1" } } }] })
        );
    }

    #[test]
    fn filter_delegate_only() {
        let dims = ResolvedDims {
            delegate_id: Some("u2".into()),
            ..Default::default()
        };
        let f = build_issue_filter(&dims, &[]);
        assert_eq!(
            f,
            json!({ "and": [{ "delegate": { "id": { "eq": "u2" } } }] })
        );
    }

    #[test]
    fn filter_delegate_with_active_states() {
        let dims = ResolvedDims {
            delegate_id: Some("u2".into()),
            ..Default::default()
        };
        let f = build_issue_filter(&dims, &["Todo".to_string(), "In Progress".to_string()]);
        assert_eq!(
            f,
            json!({ "and": [
                { "delegate": { "id": { "eq": "u2" } } },
                { "state": { "name": { "in": ["Todo", "In Progress"] } } }
            ]})
        );
    }

    #[test]
    fn filter_assignee_and_delegate_combined() {
        let dims = ResolvedDims {
            assignee_id: Some("human".into()),
            delegate_id: Some("agent".into()),
            ..Default::default()
        };
        let f = build_issue_filter(&dims, &[]);
        assert_eq!(
            f,
            json!({ "and": [
                { "assignee": { "id": { "eq": "human" } } },
                { "delegate": { "id": { "eq": "agent" } } }
            ]})
        );
    }

    #[test]
    fn filter_label_single() {
        let dims = ResolvedDims {
            labels: vec!["bug".into()],
            ..Default::default()
        };
        let f = build_issue_filter(&dims, &[]);
        assert_eq!(
            f,
            json!({ "and": [{ "labels": { "some": { "name": { "in": ["bug"] } } } }] })
        );
    }

    #[test]
    fn filter_label_list() {
        let dims = ResolvedDims {
            labels: vec!["bug".into(), "urgent".into()],
            ..Default::default()
        };
        let f = build_issue_filter(&dims, &[]);
        assert_eq!(
            f,
            json!({ "and": [
                { "labels": { "some": { "name": { "in": ["bug", "urgent"] } } } }
            ]})
        );
    }

    #[test]
    fn filter_all_dimensions_combined() {
        let dims = ResolvedDims {
            project_slug: Some("dar".into()),
            team_key: Some("ALG".into()),
            assignee_id: Some("u1".into()),
            delegate_id: Some("u2".into()),
            labels: vec!["bug".into()],
        };
        let f = build_issue_filter(&dims, &["Todo".to_string()]);
        assert_eq!(
            f,
            json!({ "and": [
                { "project": { "slugId": { "eq": "dar" } } },
                { "team": { "key": { "eq": "ALG" } } },
                { "assignee": { "id": { "eq": "u1" } } },
                { "delegate": { "id": { "eq": "u2" } } },
                { "labels": { "some": { "name": { "in": ["bug"] } } } },
                { "state": { "name": { "in": ["Todo"] } } }
            ]})
        );
    }

    #[test]
    fn filter_empty_dims_only_state_clause() {
        let f = build_issue_filter(&ResolvedDims::default(), &["Todo".to_string()]);
        assert_eq!(
            f,
            json!({ "and": [{ "state": { "name": { "in": ["Todo"] } } }] })
        );
    }

    #[test]
    fn filter_empty_active_states_has_no_state_clause() {
        let f = build_issue_filter(&dims_project(), &[]);
        assert_eq!(
            f,
            json!({ "and": [{ "project": { "slugId": { "eq": "dar" } } }] })
        );
    }

    // --- Empty-filter guard ---

    #[test]
    fn resolved_dims_default_is_empty() {
        assert!(ResolvedDims::default().is_empty());
    }

    #[test]
    fn resolved_dims_any_single_field_is_non_empty() {
        assert!(!ResolvedDims {
            project_slug: Some("p".into()),
            ..Default::default()
        }
        .is_empty());
        assert!(!ResolvedDims {
            team_key: Some("ALG".into()),
            ..Default::default()
        }
        .is_empty());
        assert!(!ResolvedDims {
            assignee_id: Some("u1".into()),
            ..Default::default()
        }
        .is_empty());
        assert!(!ResolvedDims {
            delegate_id: Some("u2".into()),
            ..Default::default()
        }
        .is_empty());
        assert!(!ResolvedDims {
            labels: vec!["bug".into()],
            ..Default::default()
        }
        .is_empty());
    }

    // --- Blocked-by scope ---

    #[test]
    fn is_blocked_when_in_dimension_blocker_non_terminal() {
        let mut map = std::collections::HashMap::new();
        map.insert("ALG-1", "Backlog");
        let terminal = vec!["Done".to_string(), "Cancelled".to_string()];
        assert!(is_blocked(&["ALG-1"], &map, &terminal));
    }

    #[test]
    fn not_blocked_when_in_dimension_blocker_terminal() {
        let mut map = std::collections::HashMap::new();
        map.insert("ALG-1", "Done");
        let terminal = vec!["Done".to_string()];
        assert!(!is_blocked(&["ALG-1"], &map, &terminal));
    }

    #[test]
    fn not_blocked_when_blocker_outside_dimension() {
        let map = std::collections::HashMap::<&str, &str>::new();
        let terminal = vec!["Done".to_string()];
        assert!(!is_blocked(&["ALG-9"], &map, &terminal));
    }

    // --- Assignee resolver ---

    fn users() -> Vec<LinearUser> {
        serde_json::from_value(json!([
            { "id": "u1", "name": "Thinh Dinh", "displayName": "thinh", "email": "thinh@x.io" },
            { "id": "u2", "name": "Alex", "displayName": "alex", "email": "alex@x.io" },
            { "id": "u3", "name": "Thinh Dinh", "displayName": "thinh2", "email": "thinh@dup.io" }
        ]))
        .unwrap()
    }

    #[test]
    fn is_uuid_recognizes_canonical_form() {
        assert!(is_uuid("3b9d8f2e-1c4a-4d6b-8e2f-7a1b2c3d4e5f"));
        assert!(!is_uuid("not-a-uuid"));
        assert!(!is_uuid("thinh"));
    }

    #[test]
    fn resolve_assignee_uuid_passthrough() {
        let id = "3b9d8f2e-1c4a-4d6b-8e2f-7a1b2c3d4e5f";
        assert_eq!(resolve_user_id(id, &users(), "assignee").unwrap(), id);
    }

    #[test]
    fn resolve_assignee_display_name_with_and_without_at() {
        assert_eq!(
            resolve_user_id("@thinh", &users(), "assignee").unwrap(),
            "u1"
        );
        assert_eq!(
            resolve_user_id("thinh", &users(), "assignee").unwrap(),
            "u1"
        );
    }

    #[test]
    fn resolve_assignee_by_name_is_ambiguous() {
        // "Thinh Dinh" matches two users by name.
        let err = resolve_user_id("Thinh Dinh", &users(), "assignee").unwrap_err();
        assert!(err.to_string().contains("ambiguous"));
    }

    #[test]
    fn resolve_assignee_by_email() {
        assert_eq!(
            resolve_user_id("alex@x.io", &users(), "assignee").unwrap(),
            "u2"
        );
    }

    #[test]
    fn resolve_assignee_case_insensitive() {
        assert_eq!(resolve_user_id("ALEX", &users(), "assignee").unwrap(), "u2");
    }

    #[test]
    fn resolve_assignee_no_match_errors() {
        let err = resolve_user_id("nobody", &users(), "assignee").unwrap_err();
        assert!(err.to_string().contains("matched no Linear user"));
    }

    #[test]
    fn resolve_delegate_uuid_passthrough() {
        let id = "3b9d8f2e-1c4a-4d6b-8e2f-7a1b2c3d4e5f";
        assert_eq!(resolve_user_id(id, &users(), "delegate").unwrap(), id);
    }

    #[test]
    fn resolve_delegate_display_name_with_and_without_at() {
        assert_eq!(
            resolve_user_id("@alex", &users(), "delegate").unwrap(),
            "u2"
        );
        assert_eq!(resolve_user_id("alex", &users(), "delegate").unwrap(), "u2");
    }

    #[test]
    fn resolve_delegate_by_name_is_ambiguous() {
        let err = resolve_user_id("Thinh Dinh", &users(), "delegate").unwrap_err();
        assert!(err.to_string().contains("delegate"));
        assert!(err.to_string().contains("ambiguous"));
    }

    #[test]
    fn resolve_delegate_by_email() {
        assert_eq!(
            resolve_user_id("alex@x.io", &users(), "delegate").unwrap(),
            "u2"
        );
    }

    #[test]
    fn resolve_delegate_no_match_errors() {
        let err = resolve_user_id("nobody", &users(), "delegate").unwrap_err();
        assert!(err.to_string().contains("delegate"));
        assert!(err.to_string().contains("matched no Linear user"));
    }

    #[test]
    fn min_remaining_updates_across_calls() {
        // Simulate two successive calls: first sees 50 requests, then 30 complexity.
        let tracker = make_tracker();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let h1 = header_map(&[("x-ratelimit-requests-remaining", "50")]);
        rt.block_on(tracker.process_rate_limit_headers(&h1));
        assert_eq!(tracker.min_remaining.load(Ordering::SeqCst), 50);

        let h2 = header_map(&[("x-ratelimit-complexity-remaining", "30")]);
        rt.block_on(tracker.process_rate_limit_headers(&h2));
        // 30 < 50 → min should now be 30
        assert_eq!(tracker.min_remaining.load(Ordering::SeqCst), 30);

        let h3 = header_map(&[("x-ratelimit-requests-remaining", "80")]);
        rt.block_on(tracker.process_rate_limit_headers(&h3));
        // 80 > 30 → min stays 30
        assert_eq!(tracker.min_remaining.load(Ordering::SeqCst), 30);
    }
}
