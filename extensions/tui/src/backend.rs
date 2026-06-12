//! Lazy chat-backend resolution (run at first submit, never at boot):
//! `extensions.tui.chat.backend` config override → follow the orchestrator
//! snapshot's `runner` when a `dyn ChatBackend` is registered under that id
//! (skipped silently when the topic is absent, the first tick has not
//! happened, or the runner is empty) → fallback `"pi"` with a transcript
//! notice when the runner was incompatible → nothing registered = chat
//! disabled with a banner. None of these outcomes is a boot failure.

use cap_chat::{ChatBackend, CHAT_FALLBACK_BACKEND};
use host_api::{EventBus, ServiceRegistry};
use orchestrator_api::{RunSnapshot, RUN_SNAPSHOT_TOPIC};

/// Outcome of [`resolve`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Open the session on this backend id; `notice` is a transcript block
    /// shown alongside (the incompatible-runner fallback explanation).
    Backend { id: String, notice: Option<String> },
    /// No usable chat backend is registered: disable the input with a banner.
    Disabled,
}

pub fn resolve(configured: Option<&str>, services: &ServiceRegistry, bus: &EventBus) -> Resolution {
    // 1. The explicit config override wins unconditionally. If it names an
    //    unregistered backend the open fails into an error block (M2 path).
    if let Some(id) = configured {
        return Resolution::Backend {
            id: id.to_string(),
            notice: None,
        };
    }
    let registered = |id: &str| services.get_named::<dyn ChatBackend>(id).is_ok();
    // 2. Follow the agent's runner — but only off a real snapshot: skip
    //    silently when the topic is absent (orchestrator not linked), no tick
    //    has been published yet (version == 0), or the runner id is empty.
    let runner = bus
        .read_retained::<RunSnapshot>(RUN_SNAPSHOT_TOPIC)
        .ok()
        .filter(|snapshot| snapshot.version > 0)
        .map(|snapshot| snapshot.agent.runner)
        .filter(|runner| !runner.is_empty());
    if let Some(runner) = runner {
        if registered(&runner) {
            return Resolution::Backend {
                id: runner,
                notice: None,
            };
        }
        // 3. Incompatible runner: fall back to pi, but tell the operator.
        if registered(CHAT_FALLBACK_BACKEND) {
            return Resolution::Backend {
                id: CHAT_FALLBACK_BACKEND.to_string(),
                notice: Some(format!(
                    "runner \"{runner}\" has no interactive chat backend; \
                     chatting via {CHAT_FALLBACK_BACKEND}"
                )),
            };
        }
        return Resolution::Disabled;
    }
    // 4. No runner to follow: plain fallback, or disabled when even the
    //    fallback backend is not registered.
    if registered(CHAT_FALLBACK_BACKEND) {
        return Resolution::Backend {
            id: CHAT_FALLBACK_BACKEND.to_string(),
            notice: None,
        };
    }
    Resolution::Disabled
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cap_chat::{ChatEvent, ChatSession, ChatSessionParams};

    use super::*;

    /// Registry stand-in: resolution only asks "is this id registered?",
    /// so the backend never has to open anything.
    struct DummyBackend;

    impl ChatBackend for DummyBackend {
        fn open<'a>(
            &'a self,
            _params: ChatSessionParams,
            _tx: tokio::sync::mpsc::Sender<ChatEvent>,
        ) -> cap_chat::BoxFuture<'a, anyhow::Result<Box<dyn ChatSession>>> {
            Box::pin(async { anyhow::bail!("dummy backend never opens") })
        }
    }

    fn registry(ids: &[&str]) -> ServiceRegistry {
        let mut services = ServiceRegistry::default();
        for id in ids {
            services
                .register::<dyn ChatBackend>(*id, Arc::new(DummyBackend))
                .unwrap();
        }
        services
    }

    fn bus_with_snapshot(version: u64, runner: &str) -> EventBus {
        let mut bus = EventBus::new();
        let mut snapshot = RunSnapshot::empty();
        snapshot.version = version;
        snapshot.agent.runner = runner.to_string();
        bus.register_retained::<RunSnapshot>(RUN_SNAPSHOT_TOPIC, snapshot)
            .unwrap();
        bus
    }

    fn backend(id: &str) -> Resolution {
        Resolution::Backend {
            id: id.to_string(),
            notice: None,
        }
    }

    #[test]
    fn config_override_wins_over_runner_follow() {
        let services = registry(&["pi", "codex"]);
        let bus = bus_with_snapshot(5, "codex");
        assert_eq!(
            resolve(Some("pi"), &services, &bus),
            backend("pi"),
            "config beats a followable runner"
        );
        // Even an unregistered override is taken as-is; the open reports it.
        assert_eq!(
            resolve(Some("claude"), &services, &bus),
            backend("claude")
        );
    }

    #[test]
    fn runner_is_followed_when_its_chat_backend_is_registered() {
        let services = registry(&["pi", "codex"]);
        let bus = bus_with_snapshot(3, "codex");
        assert_eq!(resolve(None, &services, &bus), backend("codex"));
    }

    #[test]
    fn version_zero_snapshot_falls_back_to_pi_silently() {
        // The runner would be followable, but no tick has published yet.
        let services = registry(&["pi", "codex"]);
        let bus = bus_with_snapshot(0, "codex");
        assert_eq!(resolve(None, &services, &bus), backend("pi"));
    }

    #[test]
    fn absent_topic_and_empty_runner_fall_back_to_pi_silently() {
        let services = registry(&["pi"]);
        // Orchestrator not linked: the snapshot topic does not exist.
        assert_eq!(resolve(None, &services, &EventBus::new()), backend("pi"));
        // Ticked snapshot with an empty runner id.
        let bus = bus_with_snapshot(2, "");
        assert_eq!(resolve(None, &services, &bus), backend("pi"));
    }

    #[test]
    fn incompatible_runner_falls_back_to_pi_with_a_notice() {
        let services = registry(&["pi"]);
        let bus = bus_with_snapshot(7, "claude-code");
        match resolve(None, &services, &bus) {
            Resolution::Backend { id, notice: Some(notice) } => {
                assert_eq!(id, "pi");
                assert_eq!(
                    notice,
                    "runner \"claude-code\" has no interactive chat backend; \
                     chatting via pi"
                );
            }
            other => panic!("expected pi fallback with notice, got {other:?}"),
        }
    }

    #[test]
    fn empty_registry_disables_chat() {
        let services = registry(&[]);
        // No snapshot topic at all.
        assert_eq!(
            resolve(None, &services, &EventBus::new()),
            Resolution::Disabled
        );
        // Followable runner named, but neither it nor pi is registered.
        let bus = bus_with_snapshot(4, "claude-code");
        assert_eq!(resolve(None, &services, &bus), Resolution::Disabled);
    }
}
