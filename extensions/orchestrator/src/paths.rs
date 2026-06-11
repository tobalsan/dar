//! Centralizes all path math relative to the agent root so every module agrees.
//!
//! Holds the canonical root and derives `issues/`, `workspaces/`,
//! `logs/agent.log`, `agent.yaml`, and `WORKFLOW.md`. Also owns workspace
//! identifier sanitization and the containment invariant (a child cwd MUST live
//! inside `workspace.root`).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::config::WorkspaceConfig;

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
    #[allow(dead_code)]
    pub fn agent_yaml(&self) -> PathBuf {
        self.root.join("agent.yaml")
    }

    /// `<root>/WORKFLOW.md`.
    pub fn workflow_md(&self) -> PathBuf {
        self.root.join("WORKFLOW.md")
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
    #[allow(dead_code)]
    pub fn workspace_root(&self, cfg: &WorkspaceConfig) -> PathBuf {
        resolve_workspace_root(&self.root, &cfg.root)
    }
}

/// Resolve `workspace.root`: relative values are relative to the agent/project
/// folder, `~` expands to HOME, and `$AGENT_HOME` expands to the agent root.
pub fn resolve_workspace_root(agent_root: &Path, raw: &Path) -> PathBuf {
    let raw = raw.to_string_lossy();
    let expanded = expand_workspace_root_vars(&raw, agent_root);
    let path = PathBuf::from(expanded);
    if path.is_absolute() {
        path
    } else {
        agent_root.join(path)
    }
}

fn expand_workspace_root_vars(raw: &str, agent_root: &Path) -> String {
    let mut out = raw.to_string();
    let agent_root_s = agent_root.to_string_lossy();
    out = out.replace("${AGENT_HOME}", &agent_root_s);
    out = out.replace("$AGENT_HOME", &agent_root_s);
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy();
        if out == "~" {
            out = home.to_string();
        } else if let Some(rest) = out.strip_prefix("~/") {
            out = format!("{home}/{rest}");
        }
    }
    out
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
    let safe = safe_issue_component(identifier)?;
    let ws = workspace_root.join(&safe);

    // Ensure the workspace root and the per-issue dir exist before the
    // containment check so canonicalization can resolve real paths.
    std::fs::create_dir_all(&ws)
        .with_context(|| format!("creating workspace dir {}", ws.display()))?;

    assert_contained(workspace_root, &ws)?;
    Ok(ws)
}

/// Return the per-issue workspace path without creating it.
pub fn issue_workspace_path(workspace_root: &Path, identifier: &str) -> Result<PathBuf> {
    let safe = safe_issue_component(identifier)?;
    Ok(workspace_root.join(safe))
}

fn safe_issue_component(identifier: &str) -> Result<String> {
    let safe = sanitize_identifier(identifier);
    if safe.is_empty() || safe == "." || safe == ".." {
        bail!(
            "issue identifier {:?} sanitizes to empty path component",
            identifier
        );
    }
    Ok(safe)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_workspace_root_against_agent_root() {
        let root = Path::new("/tmp/agent");
        assert_eq!(
            resolve_workspace_root(root, Path::new("workspaces")),
            PathBuf::from("/tmp/agent/workspaces")
        );
    }

    #[test]
    fn resolves_agent_home_workspace_root_against_agent_root() {
        let root = Path::new("/tmp/agent");
        assert_eq!(
            resolve_workspace_root(root, Path::new("$AGENT_HOME/ws")),
            PathBuf::from("/tmp/agent/ws")
        );
        assert_eq!(
            resolve_workspace_root(root, Path::new("${AGENT_HOME}/ws")),
            PathBuf::from("/tmp/agent/ws")
        );
    }

    #[test]
    fn resolves_tilde_workspace_root_against_home() {
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        assert_eq!(
            resolve_workspace_root(Path::new("/tmp/agent"), Path::new("~/ws")),
            PathBuf::from(home).join("ws")
        );
    }

    #[test]
    fn issue_workspace_path_uses_sanitized_single_component() {
        let path = issue_workspace_path(Path::new("/tmp/ws"), "ALG/179..x").unwrap();
        assert_eq!(path, PathBuf::from("/tmp/ws/ALG_179..x"));
    }

    #[test]
    fn issue_workspace_path_rejects_dot_components() {
        assert!(issue_workspace_path(Path::new("/tmp/ws"), ".").is_err());
        assert!(issue_workspace_path(Path::new("/tmp/ws"), "..").is_err());
    }
}
