//! Bridge between the pure `system-files` resolver and the orchestrator.
//!
//! Resolves the agent's `AGENTS.md` + `system_files` into the on-bus
//! [`orchestrator_api::SystemContext`] payload, logging non-fatal warnings and
//! degrading gracefully: a resolution error (missing `required` file or a
//! containment violation) is logged and yields an empty context rather than
//! aborting boot — `doctor`/`self-check` are the gates that *fail* on those.

use std::path::Path;

use orchestrator_api::{SystemContext, SystemContextFile};
use system_files::{ResolveError, SystemFileEntry};

use crate::config::AgentConfig;

/// Resolve the agent's system context into the retained-topic payload.
///
/// Warnings are logged; a hard error is logged and collapses to an empty
/// context (boot continues — preflight is the gate that rejects bad config).
pub fn resolve_for(root: &Path, cfg: &AgentConfig) -> SystemContext {
    match resolve(root, cfg.system_files.as_deref()) {
        Ok(ctx) => ctx,
        Err(e) => {
            tracing::error!("resolving system context: {e}");
            SystemContext::default()
        }
    }
}

/// Resolve into the contract payload, surfacing any [`ResolveError`].
pub fn resolve(
    root: &Path,
    entries: Option<&[SystemFileEntry]>,
) -> Result<SystemContext, ResolveError> {
    let resolved = system_files::resolve(root, entries)?;
    for warning in &resolved.warnings {
        tracing::warn!("system file: {warning}");
    }
    Ok(SystemContext {
        text: resolved.text,
        files: resolved
            .files
            .into_iter()
            .map(|f| SystemContextFile {
                path: f.display_path,
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn resolves_agents_md_into_payload() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "identity").unwrap();

        let ctx = resolve(dir.path(), None).unwrap();

        assert_eq!(ctx.files.len(), 1);
        assert_eq!(ctx.files[0].path, "AGENTS.md");
        assert!(ctx.text.contains("identity"));
    }

    #[test]
    fn missing_required_surfaces_error() {
        let dir = TempDir::new().unwrap();
        let entries = vec![SystemFileEntry::Detailed {
            path: "missing.md".to_string(),
            required: true,
        }];
        let err = resolve(dir.path(), Some(&entries)).unwrap_err();
        assert!(matches!(err, ResolveError::MissingRequired { .. }));
    }

    #[test]
    fn resolve_for_degrades_to_empty_on_error() {
        let dir = TempDir::new().unwrap();
        let mut cfg = sample_cfg();
        cfg.system_files = Some(vec![SystemFileEntry::Detailed {
            path: "nope.md".to_string(),
            required: true,
        }]);
        let ctx = resolve_for(dir.path(), &cfg);
        assert!(ctx.is_empty());
    }

    fn sample_cfg() -> AgentConfig {
        serde_yaml::from_str(
            "id: a\nname: A\ntracker:\n  use: files\n  config:\n    path: ./issues\n  active_states: [todo]\n  terminal_states: [done]\nrunner:\n  use: fake\norchestrator:\n  poll_interval_ms: 1000\n  max_retries: 3\nworkspace:\n  root: ./workspaces\n",
        )
        .unwrap()
    }
}
