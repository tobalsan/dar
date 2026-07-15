//! Centralizes all path math relative to the agent root so every module agrees.
//!
//! Holds the canonical root and derives `issues/`, `workspaces/`,
//! `logs/agent.log`, `agent.yaml`, and `WORKFLOW.md`. Also owns workspace
//! identifier sanitization and the containment invariant (a child cwd MUST live
//! inside `workspace.root`).
//!
//! ## Two roots
//!
//! `root` is the agent's identity home: `agent.yaml`, `.env`, system files,
//! and extension data dirs always live there, no matter which workflow is
//! running. `workflow_root` is the resolved `WORKFLOW.md`'s directory —
//! `workspace.root` and `workflow_md()` resolve against it. For the default
//! workflow (`<root>/WORKFLOW.md`) the two are identical. `state_dir` is
//! where this workflow's run-history db + logs live: `root` for the default
//! workflow (byte-identical to the legacy layout), or
//! `<root>/workflows/<workflow_key>` for an external `--workflow <path>` so
//! several workflows can run concurrently against one agent identity without
//! clobbering each other's state.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

/// All paths derive from a canonical, absolute agent root, plus an optional
/// separate workflow root for `--workflow <path>` runs.
#[derive(Debug, Clone)]
pub struct AgentPaths {
    /// Canonical absolute path to the agent folder. Identity paths —
    /// `agent.yaml`, `.env`, system files, extension data dirs — always live
    /// here.
    pub root: PathBuf,
    /// Canonical absolute path to the resolved WORKFLOW.md's directory.
    /// Equals `root` for the default workflow. `workflow_md()` and
    /// `workspace_root()` resolve against this.
    pub workflow_root: PathBuf,
    /// Where this workflow's run-history db + logs live. Equals `root` for
    /// the default workflow; `<root>/workflows/<workflow_key>` otherwise.
    pub state_dir: PathBuf,
}

impl AgentPaths {
    /// Wrap an already-resolved (canonical, absolute) root, defaulting
    /// `workflow_root` and `state_dir` to it — today's default-workflow
    /// layout. Resolution is the CLI's job; this type just does path
    /// arithmetic on top of it.
    pub fn new(root: PathBuf) -> Self {
        Self {
            workflow_root: root.clone(),
            state_dir: root.clone(),
            root,
        }
    }

    /// Construct with an explicit workflow root and state dir, for a
    /// `--workflow <path>` run whose WORKFLOW.md lives outside the agent
    /// root.
    pub fn with_workflow(root: PathBuf, workflow_root: PathBuf, state_dir: PathBuf) -> Self {
        Self {
            root,
            workflow_root,
            state_dir,
        }
    }

    /// `<root>/agent.yaml`.
    #[allow(dead_code)]
    pub fn agent_yaml(&self) -> PathBuf {
        self.root.join("agent.yaml")
    }

    /// `<workflow_root>/WORKFLOW.md`.
    pub fn workflow_md(&self) -> PathBuf {
        self.workflow_root.join("WORKFLOW.md")
    }

    /// `<state_dir>/logs`.
    pub fn logs_dir(&self) -> PathBuf {
        self.state_dir.join("logs")
    }

    /// `<state_dir>/logs/agent.log`.
    pub fn log_file(&self) -> PathBuf {
        self.logs_dir().join("agent.log")
    }

    /// `<state_dir>/data` — persistent process data dir (SQLite store, etc.).
    pub fn data_dir(&self) -> PathBuf {
        self.state_dir.join("data")
    }

    /// `<state_dir>/data/store.db` — SQLite persistence store.
    pub fn store_db(&self) -> PathBuf {
        self.data_dir().join("store.db")
    }

    /// Workspace root, resolved against the configured `workspace.root` raw
    /// value. Relative paths use the WORKFLOW.md dir; `$AGENT_HOME` uses the
    /// agent identity root.
    pub fn workspace_root(&self, raw: &Path) -> PathBuf {
        resolve_workspace_root(&self.root, &self.workflow_root, raw)
    }

    /// Display label for a non-default workflow (the workflow dir's
    /// basename), or `None` for the default workflow (`workflow_root ==
    /// root`) so dashboard/TUI surfaces omit it entirely.
    pub fn workflow_label(&self) -> Option<String> {
        if self.workflow_root == self.root {
            return None;
        }
        self.workflow_root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
    }
}

/// Stable per-workflow key used to partition state under
/// `<agent root>/workflows/<key>/` for non-default workflows:
/// `<wfdir-basename>-<shorthash>`, where `shorthash` is the first 6 hex
/// characters of sha256(canonical WORKFLOW.md path) — e.g. `triage-3f9c2a`.
/// Hashing the full path (not just the dir name) means two differently
/// located workflow dirs that happen to share a basename never collide.
pub fn workflow_key(canonical_wf_path: &Path) -> String {
    let basename = canonical_wf_path
        .parent()
        .and_then(|dir| dir.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workflow".to_string());
    let mut hasher = Sha256::new();
    hasher.update(canonical_wf_path.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let shorthash = hex::encode(&digest[..3]);
    format!("{basename}-{shorthash}")
}

/// Resolve `workspace.root`: relative values are relative to the WORKFLOW.md
/// dir, `~` expands to HOME, and `$AGENT_HOME` expands to the agent identity
/// root regardless of where WORKFLOW.md lives.
pub fn resolve_workspace_root(agent_root: &Path, workflow_root: &Path, raw: &Path) -> PathBuf {
    let raw = raw.to_string_lossy();
    let expanded = expand_workspace_root_vars(&raw, agent_root);
    let path = PathBuf::from(expanded);
    if path.is_absolute() {
        path
    } else {
        workflow_root.join(path)
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
    fn resolves_relative_workspace_root_against_workflow_root() {
        assert_eq!(
            resolve_workspace_root(
                Path::new("/tmp/agent"),
                Path::new("/tmp/workflow"),
                Path::new("workspaces"),
            ),
            PathBuf::from("/tmp/workflow/workspaces")
        );
    }

    #[test]
    fn resolves_agent_home_workspace_root_against_agent_root() {
        let agent_root = Path::new("/tmp/agent");
        let workflow_root = Path::new("/tmp/workflow");
        assert_eq!(
            resolve_workspace_root(agent_root, workflow_root, Path::new("$AGENT_HOME/ws"),),
            PathBuf::from("/tmp/agent/ws")
        );
        assert_eq!(
            resolve_workspace_root(agent_root, workflow_root, Path::new("${AGENT_HOME}/ws"),),
            PathBuf::from("/tmp/agent/ws")
        );
    }

    #[test]
    fn resolves_tilde_workspace_root_against_home() {
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        assert_eq!(
            resolve_workspace_root(
                Path::new("/tmp/agent"),
                Path::new("/tmp/workflow"),
                Path::new("~/ws"),
            ),
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

    fn default_workspace_root() -> PathBuf {
        PathBuf::from("workspaces")
    }

    #[test]
    fn default_workflow_paths_match_legacy_layout() {
        let root = PathBuf::from("/tmp/agent");
        let paths = AgentPaths::new(root.clone());

        assert_eq!(paths.workflow_root, root);
        assert_eq!(paths.state_dir, root);
        assert_eq!(paths.workflow_md(), root.join("WORKFLOW.md"));
        assert_eq!(paths.store_db(), root.join("data/store.db"));
        assert_eq!(paths.log_file(), root.join("logs/agent.log"));
        assert_eq!(
            paths.workspace_root(&default_workspace_root()),
            root.join("workspaces")
        );
    }

    #[test]
    fn external_workflow_partitions_state_under_workflows_key() {
        let root = PathBuf::from("/tmp/agent");
        let workflow_root = PathBuf::from("/tmp/wf-a");
        let key = workflow_key(&workflow_root.join("WORKFLOW.md"));
        let paths = AgentPaths::with_workflow(
            root.clone(),
            workflow_root.clone(),
            root.join("workflows").join(&key),
        );

        assert_eq!(paths.workflow_md(), workflow_root.join("WORKFLOW.md"));
        assert_eq!(
            paths.store_db(),
            root.join("workflows").join(&key).join("data/store.db")
        );
        assert_eq!(
            paths.log_file(),
            root.join("workflows").join(&key).join("logs/agent.log")
        );
        // Identity paths stay on the agent root regardless of workflow.
        assert_eq!(paths.agent_yaml(), root.join("agent.yaml"));
        // Relative workspace roots land beside the external WORKFLOW.md.
        assert_eq!(
            paths.workspace_root(&default_workspace_root()),
            workflow_root.join("workspaces")
        );
        // `$AGENT_HOME` remains the agent identity root even for an external
        // workflow.
        assert_eq!(
            paths.workspace_root(Path::new("$AGENT_HOME/workspaces")),
            root.join("workspaces")
        );
    }

    #[test]
    fn workflow_label_is_none_for_default_workflow() {
        let root = PathBuf::from("/tmp/agent");
        let paths = AgentPaths::new(root);
        assert_eq!(paths.workflow_label(), None);
    }

    #[test]
    fn workflow_label_is_workflow_dir_basename_for_external_workflow() {
        let root = PathBuf::from("/tmp/agent");
        let workflow_root = PathBuf::from("/tmp/wf-a");
        let key = workflow_key(&workflow_root.join("WORKFLOW.md"));
        let paths = AgentPaths::with_workflow(
            root.clone(),
            workflow_root,
            root.join("workflows").join(&key),
        );
        assert_eq!(paths.workflow_label(), Some("wf-a".to_string()));
    }

    #[test]
    fn workflow_key_is_stable_and_shaped_basename_dash_shorthash() {
        let path = Path::new("/tmp/wf-a/WORKFLOW.md");
        let key = workflow_key(path);

        assert_eq!(key, workflow_key(path), "must be deterministic");
        let (basename, shorthash) = key.rsplit_once('-').expect("basename-shorthash shape");
        assert_eq!(basename, "wf-a");
        assert_eq!(shorthash.len(), 6);
        assert!(shorthash.chars().all(|c| c.is_ascii_hexdigit()));

        // A different workflow path must not collide.
        let other = workflow_key(Path::new("/tmp/wf-b/WORKFLOW.md"));
        assert_ne!(key, other);
    }
}
