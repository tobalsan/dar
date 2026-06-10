//! clap CLI definition.
//!
//! Three subcommands per the PRD:
//!   agentropy run           [--dir PATH]   # default cwd; long-running
//!   agentropy doctor        [--dir PATH]   # preflight; exit code only
//!   agentropy init-workflow [--dir PATH]   # scaffold WORKFLOW.md
//!   agentropy export        [--dir PATH]   # dump Linear project/issues
//!
//! Root resolution (canonical, absolute) is the CLI's job; downstream code in
//! `AgentPaths` only does path arithmetic on top of the resolved root.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};

/// Folder-scoped agent runtime.
#[derive(Debug, Parser)]
#[command(name = "agentropy", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the orchestrator loop and dashboard (long-running).
    Run(RunArgs),
    /// Validate agent.yaml, WORKFLOW.md, and the tracker; exit code only.
    Doctor(DoctorArgs),
    /// Scaffold the default WORKFLOW.md prompt in the agent folder.
    InitWorkflow(InitWorkflowArgs),
    /// Export the configured Linear project and issues under the data dir.
    Export(ExportArgs),
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Agent folder to run in (defaults to the current directory).
    #[arg(long)]
    pub dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Agent folder to validate (defaults to the current directory).
    #[arg(long)]
    pub dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct InitWorkflowArgs {
    /// Agent folder where WORKFLOW.md should be created (defaults to the current directory).
    #[arg(long)]
    pub dir: Option<PathBuf>,
    /// Overwrite an existing WORKFLOW.md.
    #[arg(long)]
    pub force: bool,
    /// Seed WORKFLOW.md frontmatter for a Linear project slug.
    #[arg(long = "linear-project-slug")]
    pub linear_project_slug: Option<String>,
    /// Optional display name for the Linear project in WORKFLOW.md frontmatter.
    #[arg(long = "linear-project")]
    pub linear_project: Option<String>,
    /// Expose the optional linear_graphql worker tool.
    #[arg(long)]
    pub expose_graphql_tool: bool,
}

#[derive(Debug, Args)]
pub struct ExportArgs {
    /// Agent folder to export from (defaults to the current directory).
    #[arg(long)]
    pub dir: Option<PathBuf>,
}

impl RunArgs {
    /// Resolve the agent root to a canonical, absolute path.
    pub fn resolve_root(&self) -> Result<PathBuf> {
        resolve_root(self.dir.as_deref())
    }
}

impl DoctorArgs {
    /// Resolve the agent root to a canonical, absolute path.
    pub fn resolve_root(&self) -> Result<PathBuf> {
        resolve_root(self.dir.as_deref())
    }
}

impl InitWorkflowArgs {
    /// Resolve the agent root to a canonical, absolute path.
    pub fn resolve_root(&self) -> Result<PathBuf> {
        resolve_root(self.dir.as_deref())
    }
}

impl ExportArgs {
    /// Resolve the agent root to a canonical, absolute path.
    pub fn resolve_root(&self) -> Result<PathBuf> {
        resolve_root(self.dir.as_deref())
    }
}

/// Canonical default WORKFLOW.md prompt body emitted by `init-workflow`.
///
/// This is intentionally prompt content, not daemon logic: the worker receives
/// these instructions and performs tracker, review, git, and PR actions itself.
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

## Blockers

If requirements, ownership, base branch, dependency state, credentials, or validation risk are unclear, ask for human input instead of guessing. Update the Linear comment with the blocker, what you tried, and the decision needed, then move the issue to `Needs Human` and stop.

## Completion

Validate the change before handoff. When the task is complete, leave the issue out of active states: move it to `In Review` when work is done and a PR is open or updated, or to a terminal state only when the workflow explicitly calls for it."#;

/// Scaffold WORKFLOW.md with the canonical default prompt body.
///
/// Errors if the file already exists and `force` is false.
pub(crate) fn init_workflow(root: &Path, force: bool) -> Result<()> {
    init_workflow_with_options(root, force, None, None, false)
}

pub(crate) fn init_workflow_with_options(
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
    println!("wrote {}", path.display());
    Ok(())
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

/// Resolve `--dir` (or cwd when absent) into a canonical, absolute path.
fn resolve_root(dir: Option<&std::path::Path>) -> Result<PathBuf> {
    let raw = match dir {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir().context("resolving current directory")?,
    };
    raw.canonicalize()
        .with_context(|| format!("resolving agent folder {}", raw.display()))
}

#[cfg(test)]
mod tests {
    use super::{init_workflow, init_workflow_with_options, DEFAULT_WORKFLOW_MD_BODY};

    #[test]
    fn default_workflow_body_contains_standard_worker_procedure() {
        let body = DEFAULT_WORKFLOW_MD_BODY;

        for required in [
            "Fetch the Linear issue",
            "move it to `In Progress`",
            "Keep that same Linear comment updated",
            "Work only inside this issue workspace",
            "create a git worktree from the correct base branch",
            "Spawn a reviewer subagent",
            "Do not commit until the reviewer comes back clean",
            "Create or update the GitHub PR using `gh`",
            "Link the PR to the Linear issue",
            "Move the issue to `In Review`",
            "move the issue to `Needs Human`",
            "Fetch each `blockedBy` issue",
            "Use one PR per parent",
        ] {
            assert!(body.contains(required), "missing required text: {required}");
        }
    }

    #[test]
    fn default_workflow_body_is_prompt_only_guidance() {
        let body = DEFAULT_WORKFLOW_MD_BODY;

        assert!(body.contains("prompt-level worker guidance"));
        assert!(!body.contains("daemon must"));
        assert!(!body.contains("orchestrator must"));
    }

    #[test]
    fn init_workflow_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        init_workflow(dir.path(), false).unwrap();
        let written = std::fs::read_to_string(dir.path().join("WORKFLOW.md")).unwrap();
        assert!(written.contains("prompt-level worker guidance"));
        assert!(written.ends_with('\n'));
    }

    #[test]
    fn init_workflow_refuses_overwrite_without_force() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("WORKFLOW.md"), "existing content").unwrap();
        let err = init_workflow(dir.path(), false).unwrap_err();
        assert!(err.to_string().contains("already exists"), "got: {err}");
        // Original file must be untouched.
        let contents = std::fs::read_to_string(dir.path().join("WORKFLOW.md")).unwrap();
        assert_eq!(contents, "existing content");
    }

    #[test]
    fn init_workflow_overwrites_with_force() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("WORKFLOW.md"), "old content").unwrap();
        init_workflow(dir.path(), true).unwrap();
        let written = std::fs::read_to_string(dir.path().join("WORKFLOW.md")).unwrap();
        assert!(written.contains("prompt-level worker guidance"));
    }

    #[test]
    fn init_workflow_can_seed_linear_frontmatter_without_agent_yaml() {
        let dir = tempfile::tempdir().unwrap();
        init_workflow_with_options(
            dir.path(),
            false,
            Some("abc123"),
            Some("Agentropy Test"),
            true,
        )
        .unwrap();

        let written = std::fs::read_to_string(dir.path().join("WORKFLOW.md")).unwrap();
        assert!(written.contains("tracker:\n  kind: linear\n  project_slug: abc123"));
        assert!(written.contains("linear:\n  project: Agentropy Test\n  exposeGraphqlTool: true"));
        assert!(!dir.path().join("agent.yaml").exists());
    }
}
