//! On-bus contract for the agent's assembled system-file identity context.
//!
//! The substrate `system-context` extension resolves `AGENTS.md` + `system_files`
//! once at boot and publishes the assembled [`SystemContext`] on the retained
//! [`SYSTEM_CONTEXT_TOPIC`], before the orchestrator loop and any consumer
//! (TUI chat, issue runner, out-of-tree chat extensions) reads it.
//!
//! The payload + topic themselves live in `dar-orchestrator-api` — the
//! published, dependency-light bus crate — so SDK consumers can read the same
//! identity context without importing this `publish = false` resolver crate.
//! They are re-exported here for the in-tree resolver/publisher, which also
//! owns the [`From`] projection from a resolver result into the bus payload.

pub use orchestrator_api::{SystemContext, SystemContextFile, SYSTEM_CONTEXT_TOPIC};

impl From<crate::SystemContext> for SystemContext {
    /// Project a resolver result into the on-bus payload, dropping warnings
    /// (which the resolver caller logs) and keeping ordered paths + text.
    fn from(resolved: crate::SystemContext) -> Self {
        SystemContext {
            text: resolved.text,
            files: resolved
                .files
                .into_iter()
                .map(|f| SystemContextFile {
                    path: f.display_path,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ResolvedFile, SystemContext as Resolved};

    #[test]
    fn from_resolver_keeps_text_and_ordered_paths() {
        let resolved = Resolved {
            text: "BODY".to_string(),
            files: vec![
                ResolvedFile {
                    display_path: "AGENTS.md".to_string(),
                    contents: "a".to_string(),
                },
                ResolvedFile {
                    display_path: "SOUL.md".to_string(),
                    contents: "s".to_string(),
                },
            ],
            warnings: Vec::new(),
        };

        let payload: SystemContext = resolved.into();

        assert_eq!(payload.text, "BODY");
        assert_eq!(
            payload.files,
            vec![
                SystemContextFile {
                    path: "AGENTS.md".to_string()
                },
                SystemContextFile {
                    path: "SOUL.md".to_string()
                },
            ]
        );
        assert!(!payload.is_empty());
    }

    #[test]
    fn default_is_empty() {
        assert!(SystemContext::default().is_empty());
    }
}
