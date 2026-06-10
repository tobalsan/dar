//! Local export helpers for Linear-backed agent folders.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::config::{self, AgentConfig};
use crate::paths::AgentPaths;
use crate::prompt;
use crate::tracker::linear::{LinearExport, LinearTracker, LinearTrackerConfig};
use crate::workflow_config::EffectiveLoopConfig;

#[derive(Debug, Clone, Serialize)]
pub struct ExportResult {
    pub dir: PathBuf,
    pub project_path: PathBuf,
    pub issues_path: PathBuf,
    pub issue_count: usize,
}

pub fn export_linear_project_from_paths(paths: &AgentPaths) -> Result<ExportResult> {
    let agent_cfg = config::load(&paths.root)?;
    agent_cfg.validate().context("invalid agent.yaml")?;
    let prompt = prompt::PromptRenderer::load(&paths.workflow_md())?;
    let effective_cfg = EffectiveLoopConfig::merge(&agent_cfg, &prompt.snapshot().frontmatter);
    export_linear_project(paths, &agent_cfg, &effective_cfg)
}

pub fn export_linear_project(
    paths: &AgentPaths,
    agent_cfg: &AgentConfig,
    effective_cfg: &EffectiveLoopConfig,
) -> Result<ExportResult> {
    if effective_cfg.tracker_kind != "linear" {
        bail!(
            "export requires tracker.kind/use \"linear\" (got {:?})",
            effective_cfg.tracker_kind
        );
    }
    let api_key = std::env::var("LINEAR_API_KEY")
        .ok()
        .filter(|key| !key.is_empty())
        .context("LINEAR_API_KEY is required for Linear export")?;
    let project_slug = effective_cfg
        .tracker_project_slug
        .clone()
        .filter(|slug| !slug.is_empty())
        .context("tracker.project_slug is required for Linear export")?;

    let tracker = LinearTracker::new(LinearTrackerConfig {
        endpoint: effective_cfg.tracker_endpoint.clone(),
        api_key,
        project_slug,
        active_states: effective_cfg.active_states.clone(),
        terminal_states: effective_cfg.terminal_states.clone(),
        needs_human: effective_cfg.needs_human.clone(),
    })?;
    let snapshot = tracker.export_snapshot()?;
    write_snapshot(paths, agent_cfg, snapshot)
}

fn write_snapshot(
    paths: &AgentPaths,
    agent_cfg: &AgentConfig,
    snapshot: LinearExport,
) -> Result<ExportResult> {
    let dir = paths.data_dir().join("export");
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

fn write_json<T: Serialize>(path: &std::path::Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value).context("serializing export JSON")?;
    std::fs::write(path, [bytes, b"\n".to_vec()].concat())
        .with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracker::linear::LinearProjectExport;

    #[test]
    fn write_snapshot_writes_project_and_issues_under_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AgentPaths::new(dir.path().canonicalize().unwrap());
        let agent_cfg = AgentConfig {
            id: "agent-1".to_string(),
            name: "Agent One".to_string(),
            tracker: crate::config::TrackerConfig {
                use_: "linear".to_string(),
                config: None,
                active_states: vec!["Todo".to_string()],
                terminal_states: vec!["Done".to_string()],
                project_slug: Some("proj".to_string()),
                endpoint: None,
                needs_human: None,
            },
            runner: crate::config::RunnerConfig {
                use_: "fake".to_string(),
                command: "sh".to_string(),
                model: None,
                max_run_timeout_ms: 1000,
            },
            orchestrator: crate::config::OrchestratorConfig {
                poll_interval_ms: 1000,
                max_concurrent: 1,
                max_active_runs: 1,
                max_retries: 1,
                retry_backoff_ms: 1000,
            },
            workspace: crate::config::WorkspaceConfig {
                root: "workspaces".into(),
            },
            dashboard: crate::config::DashboardConfig {
                bind: "127.0.0.1".parse().unwrap(),
                port: 7878,
            },
        };
        let snapshot = LinearExport {
            project: LinearProjectExport {
                name: Some("Project".to_string()),
                slug: "proj".to_string(),
                endpoint: "https://api.linear.app/graphql".to_string(),
                exported_at: chrono::Utc::now(),
                issue_count: 0,
            },
            issues: Vec::new(),
        };

        let result = write_snapshot(&paths, &agent_cfg, snapshot).unwrap();

        assert_eq!(result.issue_count, 0);
        assert!(result.project_path.starts_with(paths.data_dir()));
        assert!(result.issues_path.starts_with(paths.data_dir()));
        assert!(result.project_path.exists());
        assert!(result.issues_path.exists());
    }
}
