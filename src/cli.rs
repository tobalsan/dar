//! clap CLI definition.
//!
//! Two subcommands per the PRD:
//!   agentropy run    [--dir PATH]   # default cwd; long-running
//!   agentropy doctor [--dir PATH]   # preflight; exit code only
//!
//! Root resolution (canonical, absolute) is the CLI's job; downstream code in
//! `AgentPaths` only does path arithmetic on top of the resolved root.

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

/// Resolve `--dir` (or cwd when absent) into a canonical, absolute path.
fn resolve_root(dir: Option<&std::path::Path>) -> Result<PathBuf> {
    let raw = match dir {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir().context("resolving current directory")?,
    };
    raw.canonicalize()
        .with_context(|| format!("resolving agent folder {}", raw.display()))
}
