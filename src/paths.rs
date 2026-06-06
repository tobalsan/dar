//! Centralizes all path math relative to the agent root so every module agrees.
//!
//! Holds the canonical root and derives `issues/`, `workspaces/`,
//! `logs/agent.log`, `agent.yaml`, and `WORKFLOW.md`. Also owns workspace
//! identifier sanitization and the containment invariant (a child cwd MUST live
//! inside `workspace.root`).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::config::{TrackerConfig, WorkspaceConfig};

/// All paths derive from a single canonical, absolute agent root.
#[derive(Debug, Clone)]
pub struct AgentPaths {
    /// Canonical absolute path to the agent folder.
    pub root: PathBuf,
}

impl AgentPaths {
    /// Wrap an already-resolved (canonical, absolute) root. Resolution is the
    /// CLI's job; this type just does path arithmetic on top of it.
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// `<root>/agent.yaml`.
    pub fn agent_yaml(&self) -> PathBuf {
        self.root.join("agent.yaml")
    }

    /// `<root>/WORKFLOW.md`.
    pub fn workflow_md(&self) -> PathBuf {
        self.root.join("WORKFLOW.md")
    }

    /// Issues directory, resolved against the tracker's configured `config.path`
    /// (relative to the agent root).
    pub fn issues_dir(&self, cfg: &TrackerConfig) -> PathBuf {
        self.root.join(&cfg.config.path)
    }

    /// `<root>/logs`.
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// `<root>/logs/agent.log`.
    pub fn log_file(&self) -> PathBuf {
        self.logs_dir().join("agent.log")
    }

    /// `<root>/data` — persistent process data dir (SQLite store, etc.).
    pub fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }

    /// `<root>/data/store.db` — SQLite persistence store.
    pub fn store_db(&self) -> PathBuf {
        self.data_dir().join("store.db")
    }

    /// Workspace root, resolved against the configured `workspace.root`
    /// (relative to the agent root).
    pub fn workspace_root(&self, cfg: &WorkspaceConfig) -> PathBuf {
        self.root.join(&cfg.root)
    }
}

/// Replace any char outside `[A-Za-z0-9._-]` with `_` so an issue identifier is
/// safe to use as a single path component.
pub fn sanitize_identifier(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Build (and ensure existence of) the per-issue workspace dir
/// `<workspace_root>/<sanitized(identifier)>/`, then assert it is contained
/// within `workspace_root`.
pub fn issue_workspace(workspace_root: &Path, identifier: &str) -> Result<PathBuf> {
    let safe = sanitize_identifier(identifier);
    if safe.is_empty() {
        bail!("issue identifier {:?} sanitizes to empty path component", identifier);
    }
    let ws = workspace_root.join(&safe);

    // Ensure the workspace root and the per-issue dir exist before the
    // containment check so canonicalization can resolve real paths.
    std::fs::create_dir_all(&ws)
        .with_context(|| format!("creating workspace dir {}", ws.display()))?;

    assert_contained(workspace_root, &ws)?;
    Ok(ws)
}

/// Reject any `child` path that escapes `root`. Both are canonicalized so that
/// `..`, symlinks, and `.` cannot be used to break out of the workspace root.
pub fn assert_contained(root: &Path, child: &Path) -> Result<()> {
    let root_c = root
        .canonicalize()
        .with_context(|| format!("canonicalizing workspace root {}", root.display()))?;
    let child_c = child
        .canonicalize()
        .with_context(|| format!("canonicalizing workspace path {}", child.display()))?;

    if !child_c.starts_with(&root_c) {
        bail!(
            "workspace path {} escapes workspace root {}",
            child_c.display(),
            root_c.display()
        );
    }
    Ok(())
}
