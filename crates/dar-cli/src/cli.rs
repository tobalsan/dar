//! clap CLI definition.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

/// Folder-scoped agent runtime.
#[derive(Debug, Parser)]
#[command(name = "dar", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the orchestrator loop and dashboard (long-running).
    Run(RunArgs),
    /// Serve a unified dashboard over every live agent on this host (long-running).
    Dash(DashArgs),
    /// Validate agent.yaml, WORKFLOW.md, and the tracker; exit code only.
    Doctor(DoctorArgs),
    /// Initialize a new agent workspace (writes agent.yaml + .gitignore).
    Create(CreateArgs),
    /// Bootstrap the per-agent composition crate.
    InitBuild(InitBuildArgs),
    /// Regenerate and build the per-agent binary.
    Build(BuildArgs),
    /// Refresh the per-agent Cargo.lock.
    LockRefresh(DirArgs),
    /// Manage this agent binary.
    #[command(name = "self")]
    Self_(SelfArgs),
    /// Scaffold the default WORKFLOW.md prompt in the agent folder.
    InitWorkflow(InitWorkflowArgs),
    /// Export the configured tracker project and issues under the data dir.
    Export(ExportArgs),
    /// Host-owned MCP bridge over stdio (spawned by runners; not for direct use).
    #[command(name = "__mcp-bridge", hide = true)]
    McpBridge(McpBridgeArgs),
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Agent folder to run in (defaults to the current directory).
    #[arg(long)]
    pub dir: Option<PathBuf>,
    /// Workflow to run: a directory containing WORKFLOW.md, or an explicit
    /// `.../WORKFLOW.md` path. Defaults to `<dir>/WORKFLOW.md`.
    #[arg(long)]
    pub workflow: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct DashArgs {
    /// Address to bind the aggregator on (default 0.0.0.0).
    #[arg(long)]
    pub bind: Option<std::net::IpAddr>,
    /// Port to bind the aggregator on (default 7878).
    #[arg(long)]
    pub port: Option<u16>,
    /// Registry directory to read agent presence from (default ~/.dar/dashboards).
    #[arg(long)]
    pub registry_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Agent folder to validate (defaults to the current directory).
    #[arg(long)]
    pub dir: Option<PathBuf>,
    /// Workflow to validate: a directory containing WORKFLOW.md, or an
    /// explicit `.../WORKFLOW.md` path. Defaults to `<dir>/WORKFLOW.md`.
    #[arg(long)]
    pub workflow: Option<PathBuf>,
    /// Also preflight static Linux build prerequisites.
    #[arg(long = "static")]
    pub static_: bool,
    /// Static build target triple to preflight.
    #[arg(long)]
    pub target: Option<String>,
}

#[derive(Debug, Args)]
pub struct BuildArgs {
    /// Agent folder to build (defaults to the current directory).
    #[arg(long)]
    pub dir: Option<PathBuf>,
    /// Vendor dependencies into .dar/vendor for offline builds.
    #[arg(long)]
    pub vendor: bool,
    /// Run cargo without network access.
    #[arg(long)]
    pub offline: bool,
    /// Build for an explicit Rust target triple.
    #[arg(long)]
    pub target: Option<String>,
    /// Build a static Linux musl binary for this host architecture.
    #[arg(long = "static")]
    pub static_: bool,
    /// On macOS, build arm64 + x86_64 and join them with lipo.
    #[arg(long)]
    pub universal: bool,
}

#[derive(Debug, Args)]
pub struct CreateArgs {
    /// Config folder to initialize (default: current directory). Absolute if it
    /// starts with '/', else resolved relative to the current directory.
    pub path: Option<PathBuf>,
    /// Runner to use (default: pi).
    #[arg(long)]
    pub runner: Option<String>,
    /// Provider forwarded to the runner (ignored when runner is codex).
    #[arg(long)]
    pub provider: Option<String>,
    /// Model forwarded to the runner (default: the runner's own default).
    #[arg(long)]
    pub model: Option<String>,
    /// Enable the orchestrator loop (writes the tracker/orchestrator/workspace
    /// trio + WORKFLOW.md).
    #[arg(long)]
    pub orchestrator: bool,
}

#[derive(Debug, Args)]
pub struct InitBuildArgs {
    /// Agent folder to build (defaults to the current directory).
    #[arg(long)]
    pub dir: Option<PathBuf>,
    /// Vendor dependencies into .dar/vendor for offline builds.
    #[arg(long)]
    pub vendor: bool,
    /// Run cargo without network access.
    #[arg(long)]
    pub offline: bool,
}

#[derive(Debug, Args)]
pub struct DirArgs {
    /// Agent folder (defaults to the current directory).
    #[arg(long)]
    pub dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct McpBridgeArgs {
    /// Agent folder (defaults to the current directory).
    #[arg(long)]
    pub dir: Option<PathBuf>,
    /// Workflow identity of the live host this bridge serves.
    #[arg(long)]
    pub workflow: Option<PathBuf>,
}

impl McpBridgeArgs {
    pub fn resolve_root(&self) -> Result<PathBuf> {
        resolve_root(self.dir.as_deref())
    }
}

#[derive(Debug, Args)]
pub struct SelfArgs {
    #[command(subcommand)]
    pub command: SelfCommand,
}

#[derive(Debug, Args)]
pub struct SelfRebuildArgs {
    /// Live agent id to rebuild. Requires dashboard presence.
    pub agent: Option<String>,
    /// Agent folder for an offline one-pass rebuild.
    #[arg(long)]
    pub dir: Option<PathBuf>,
    /// Select a workflow when more than one live process has this agent id.
    #[arg(long)]
    pub workflow: Option<PathBuf>,
    /// Registry directory to read live dashboard presence from.
    #[arg(long)]
    pub registry_dir: Option<PathBuf>,
    #[arg(long)]
    pub vendor: bool,
    #[arg(long)]
    pub offline: bool,
    #[arg(long)]
    pub target: Option<String>,
    #[arg(long = "static")]
    pub static_: bool,
    #[arg(long)]
    pub universal: bool,
}

#[derive(Debug, Subcommand)]
pub enum SelfCommand {
    /// Rebuild a live agent, or rebuild --dir once without restarting.
    Rebuild(SelfRebuildArgs),
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
    /// Seed Plane workspace slug in WORKFLOW.md frontmatter.
    #[arg(long = "plane-workspace")]
    pub plane_workspace: Option<String>,
    /// Seed Plane project UUID in WORKFLOW.md frontmatter.
    #[arg(long = "plane-project")]
    pub plane_project: Option<String>,
    /// Expose the optional plane_api worker tool.
    #[arg(long)]
    pub expose_api_tool: bool,
}

#[derive(Debug, Args)]
pub struct ExportArgs {
    /// Agent folder to export from (defaults to the current directory).
    #[arg(long)]
    pub dir: Option<PathBuf>,
    /// Workflow to export from: a directory containing WORKFLOW.md, or an
    /// explicit `.../WORKFLOW.md` path. Defaults to `<dir>/WORKFLOW.md`.
    #[arg(long)]
    pub workflow: Option<PathBuf>,
}

impl RunArgs {
    pub fn resolve_root(&self) -> Result<PathBuf> {
        resolve_root(self.dir.as_deref())
    }
}

impl DoctorArgs {
    pub fn resolve_root(&self) -> Result<PathBuf> {
        resolve_root(self.dir.as_deref())
    }
}

impl BuildArgs {
    pub fn resolve_root(&self) -> Result<PathBuf> {
        resolve_root(self.dir.as_deref())
    }
}

impl SelfRebuildArgs {
    pub fn resolve_root(&self) -> Result<PathBuf> {
        resolve_root(self.dir.as_deref())
    }
}

impl InitBuildArgs {
    pub fn resolve_root(&self) -> Result<PathBuf> {
        resolve_root(self.dir.as_deref())
    }
}

impl CreateArgs {
    /// Resolve the config folder without requiring it to exist yet: absolute
    /// paths are used as-is, relative paths join the current directory. The
    /// folder is created later by `create::run` (`resolve_root`'s `canonicalize`
    /// would fail here because the folder may not exist).
    pub fn resolve_root(&self) -> Result<PathBuf> {
        match &self.path {
            Some(path) if path.is_absolute() => Ok(path.clone()),
            Some(path) => Ok(std::env::current_dir()
                .context("resolving current directory")?
                .join(path)),
            None => std::env::current_dir().context("resolving current directory"),
        }
    }
}

impl DirArgs {
    pub fn resolve_root(&self) -> Result<PathBuf> {
        resolve_root(self.dir.as_deref())
    }
}

impl InitWorkflowArgs {
    pub fn resolve_root(&self) -> Result<PathBuf> {
        resolve_root(self.dir.as_deref())
    }
}

impl ExportArgs {
    pub fn resolve_root(&self) -> Result<PathBuf> {
        resolve_root(self.dir.as_deref())
    }
}

fn resolve_root(dir: Option<&std::path::Path>) -> Result<PathBuf> {
    let raw = match dir {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir().context("resolving current directory")?,
    };
    raw.canonicalize()
        .with_context(|| format!("resolving agent folder {}", raw.display()))
}

/// Resolve `--workflow` against a canonical agent `dir`.
///
/// - No flag: the default workflow, `<dir>/WORKFLOW.md` (need not exist yet —
///   passive agents have none). Returns `is_default = true`.
/// - Flag pointing at a directory: joins `WORKFLOW.md` onto it.
/// - Flag pointing at a file: the file name MUST be exactly `WORKFLOW.md`
///   (the one-file-per-workflow convention); anything else is a clear error.
///
/// A relative flag value resolves against the current directory (matching
/// `--dir`'s own resolution), then is canonicalized so workflow identity is
/// stable across re-runs (`Design decisions: Frontmatter schema` /
/// `Path split`). Returns `(workflow_file, workflow_dir, is_default)`, where
/// `is_default` reflects whether the *resolved* workflow file equals
/// `<dir>/WORKFLOW.md` — true even if the flag was passed but happens to
/// point at that same file.
pub fn resolve_workflow(
    dir: &std::path::Path,
    flag: Option<&std::path::Path>,
) -> Result<(PathBuf, PathBuf, bool)> {
    let (workflow_file, workflow_dir) = match flag {
        None => (dir.join("WORKFLOW.md"), dir.to_path_buf()),
        Some(raw) => {
            let raw = if raw.is_absolute() {
                raw.to_path_buf()
            } else {
                std::env::current_dir()
                    .context("resolving current directory")?
                    .join(raw)
            };
            let canonical = raw
                .canonicalize()
                .with_context(|| format!("resolving --workflow {}", raw.display()))?;
            if canonical.is_dir() {
                (canonical.join("WORKFLOW.md"), canonical)
            } else {
                let name = canonical.file_name().and_then(|n| n.to_str());
                if name != Some("WORKFLOW.md") {
                    anyhow::bail!(
                        "--workflow file must be named WORKFLOW.md, got `{}`",
                        canonical.display()
                    );
                }
                let workflow_dir = canonical
                    .parent()
                    .map(std::path::Path::to_path_buf)
                    .unwrap_or_else(|| canonical.clone());
                (canonical, workflow_dir)
            }
        }
    };
    let is_default = workflow_file == dir.join("WORKFLOW.md");
    Ok((workflow_file, workflow_dir, is_default))
}

#[cfg(test)]
mod resolve_workflow_tests {
    use super::*;

    #[test]
    fn default_when_no_flag() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().canonicalize().unwrap();

        let (file, wf_dir, is_default) = resolve_workflow(&dir, None).unwrap();

        assert_eq!(file, dir.join("WORKFLOW.md"));
        assert_eq!(wf_dir, dir);
        assert!(is_default);
    }

    #[test]
    fn flag_pointing_at_directory_joins_workflow_md() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().canonicalize().unwrap();
        let wf_root = dir.join("workflows/triage");
        std::fs::create_dir_all(&wf_root).unwrap();
        std::fs::write(wf_root.join("WORKFLOW.md"), "hi").unwrap();

        let (file, wf_dir, is_default) = resolve_workflow(&dir, Some(&wf_root)).unwrap();

        assert_eq!(file, wf_root.canonicalize().unwrap().join("WORKFLOW.md"));
        assert_eq!(wf_dir, wf_root.canonicalize().unwrap());
        assert!(!is_default);
    }

    #[test]
    fn flag_pointing_at_explicit_workflow_md_file() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().canonicalize().unwrap();
        let wf_root = dir.join("workflows/triage");
        std::fs::create_dir_all(&wf_root).unwrap();
        let wf_file = wf_root.join("WORKFLOW.md");
        std::fs::write(&wf_file, "hi").unwrap();

        let (file, wf_dir, is_default) = resolve_workflow(&dir, Some(&wf_file)).unwrap();

        assert_eq!(file, wf_file.canonicalize().unwrap());
        assert_eq!(wf_dir, wf_root.canonicalize().unwrap());
        assert!(!is_default);
    }

    #[test]
    fn flag_pointing_at_default_workflow_file_is_still_default() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().canonicalize().unwrap();
        let wf_file = dir.join("WORKFLOW.md");
        std::fs::write(&wf_file, "hi").unwrap();

        let (file, wf_dir, is_default) = resolve_workflow(&dir, Some(&wf_file)).unwrap();

        assert_eq!(file, wf_file);
        assert_eq!(wf_dir, dir);
        assert!(is_default);
    }

    #[test]
    fn flag_pointing_at_wrongly_named_file_errors() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().canonicalize().unwrap();
        let wf_root = dir.join("workflows/triage");
        std::fs::create_dir_all(&wf_root).unwrap();
        let bad_file = wf_root.join("workflow.md");
        std::fs::write(&bad_file, "hi").unwrap();

        let err = resolve_workflow(&dir, Some(&bad_file)).unwrap_err();

        assert!(
            err.to_string().contains("must be named WORKFLOW.md"),
            "{err:#}"
        );
    }
}
