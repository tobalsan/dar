//! clap CLI definition.

use std::path::PathBuf;

use anyhow::{Context, Result};
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
    pub fn resolve_root(&self) -> Result<PathBuf> {
        resolve_root(self.dir.as_deref())
    }
}

impl DoctorArgs {
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
