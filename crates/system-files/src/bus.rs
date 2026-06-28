//! On-bus contract for the agent's assembled system-file identity context.
//!
//! The substrate `system-context` extension resolves `AGENTS.md` + `system_files`
//! once at boot and publishes the assembled [`SystemContext`] on the retained
//! [`SYSTEM_CONTEXT_TOPIC`], before the orchestrator loop and any consumer
//! (TUI chat, issue runner) reads it. Keeping the topic/type here — neutral and
//! dependency-light — lets every surface share one identity without importing
//! the orchestrator or a dedicated capability crate.

use serde::{Deserialize, Serialize};

/// Retained topic carrying the agent's assembled system-file identity context.
/// Registered and published by the `system-context` substrate extension at boot,
/// before consumers start, so every surface reads the same `AGENTS.md` +
/// `system_files` assembly.
pub const SYSTEM_CONTEXT_TOPIC: &str = "system.context";

/// One file that contributed to the assembled [`SystemContext`], in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemContextFile {
    /// Root-relative, forward-slash path shown in the tagged block.
    pub path: String,
}

/// Retained bus payload: the agent's assembled, path-tagged system context.
///
/// `text` is the full assembly (`AGENTS.md` first, then `system_files`); `files`
/// lists the contributing paths in assembly order. Defaults to empty so the
/// retained topic can be registered with an inert initial value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemContext {
    pub text: String,
    pub files: Vec<SystemContextFile>,
}

impl SystemContext {
    /// `true` when no files resolved (empty identity context).
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

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
