//! Public SDK for writing third-party dar extensions.
//!
//! This crate is the stable extension-author surface. Prefer depending on this
//! crate instead of individual dar workspace crates.

use std::path::Path;
use std::sync::{Mutex, OnceLock};

// `Extension::agent_singleton()` defaults to `false`; override it to `true`
// only for extensions that hold a singleton external connection (a
// scheduler's polling loop, a Telegram/IRC bridge) — the host skips such
// extensions when booting a non-default `--workflow` process so the same
// agent identity never opens that connection twice.
pub use dar_artifacts as artifacts;

pub use host_api::{
    BoxFuture, ConfigStore, EnvReloadConsumer, EnvReloadConsumers, EventBus, Extension, HostPaths,
    RegisterCtx, ServiceRegistry, ShutdownToken, StartCtx, ENV_RELOAD_CONSUMERS_SERVICE,
};

pub mod chat {
    use std::path::Path;

    use host_api::StartCtx;
    use orchestrator_api::{RunSnapshot, RUN_SNAPSHOT_TOPIC};

    pub use cap_chat::{
        ArtifactReady, BoxFuture, ChatBackend, ChatEvent, ChatRole, ChatSession, ChatSessionParams,
        ChatSessionParamsBuilder, HostToolBridge, CHAT_FALLBACK_BACKEND,
    };
    pub use orchestrator_api::{SystemContext, SystemContextFile, SYSTEM_CONTEXT_TOPIC};

    /// Resolve the chat-backend id for an agent-facing chat surface, with the
    /// exact same precedence the TUI uses:
    ///
    /// 1. an explicit non-empty `configured` override wins;
    /// 2. else follow the orchestrator's selected runner when it is registered
    ///    as a `dyn ChatBackend` (only off a real snapshot: `version > 0` and a
    ///    non-empty runner id);
    /// 3. else fall back to the stock [`CHAT_FALLBACK_BACKEND`] (`pi`).
    ///
    /// Returning an explicit override even when it is not registered mirrors the
    /// TUI: opening that id is the surface's chance to report
    /// "backend not registered". Runner-derived ids are checked before use so a
    /// runner without a chat backend can still fall back cleanly.
    pub fn resolve_agent_backend(ctx: &StartCtx, configured: Option<&str>) -> String {
        let registered = |id: &str| ctx.host.services.get::<dyn ChatBackend>(id).is_ok();
        if let Some(id) = configured.filter(|id| !id.is_empty()) {
            return id.to_string();
        }
        let runner = ctx
            .host
            .bus
            .read_retained::<RunSnapshot>(RUN_SNAPSHOT_TOPIC)
            .ok()
            .filter(|s| s.version > 0)
            .map(|s| s.agent.runner)
            .filter(|r| !r.is_empty());
        if let Some(runner) = runner {
            if registered(&runner) {
                return runner;
            }
        }
        CHAT_FALLBACK_BACKEND.to_string()
    }

    /// Build the [`ChatSessionParams`] every agent-facing chat surface should
    /// open with, so TUI, IRC, Telegram, and any future web/Discord surface all
    /// talk to the same agent identity. Sourced entirely from retained bus
    /// state + the host service registry:
    ///
    /// * **model / provider** — from the retained [`RunSnapshot`] when a real
    ///   one has been published (`version > 0`), else left unset;
    /// * **system_prompt** — the retained [`SystemContext`] assembly
    ///   ([`SYSTEM_CONTEXT_TOPIC`]); an absent topic or an empty assembly
    ///   yields `None`, so the session opens exactly as before with no system
    ///   turn injected (matching the TUI's graceful-degrade behavior);
    /// * **host tool bridge** — the hidden `__mcp-bridge` descriptor, or `None`
    ///   when no tool registry is present;
    /// * **cwd** — the agent root;
    /// * **session dir** — the caller's per-surface `session_dir`.
    ///
    /// The `command` is left empty (`""`); backends that need an explicit
    /// command (none of the stock ones do) can override via the returned
    /// builder before `.build()`.
    pub fn agent_session_params(ctx: &StartCtx, session_dir: &Path) -> ChatSessionParamsBuilder {
        let snapshot = ctx
            .host
            .bus
            .read_retained::<RunSnapshot>(RUN_SNAPSHOT_TOPIC)
            .ok()
            .filter(|s| s.version > 0);
        let model = snapshot.as_ref().and_then(|s| s.agent.model.clone());
        let provider = snapshot.as_ref().and_then(|s| s.agent.provider.clone());
        let system_prompt = ctx
            .host
            .bus
            .read_retained::<SystemContext>(SYSTEM_CONTEXT_TOPIC)
            .ok()
            .filter(|sc| !sc.is_empty())
            .map(|sc| sc.text);
        ChatSessionParams::builder("", ctx.paths.root(), session_dir)
            .model(model)
            .provider(provider)
            .system_prompt(system_prompt)
            .host_tool_bridge(crate::tools::host_tool_bridge(
                &ctx.host.services,
                ctx.paths.root(),
            ))
    }
}

pub mod orchestrator {
    pub use orchestrator_api::{RunSnapshot, RUN_SNAPSHOT_TOPIC};
}

pub mod log {
    use super::{Mutex, OnceLock};

    /// Structured extension event logger: `(issue, event, message)`.
    pub type EventHook = fn(&str, &str, &str);

    static EVENT_HOOK: OnceLock<Mutex<Option<EventHook>>> = OnceLock::new();

    fn hook_slot() -> &'static Mutex<Option<EventHook>> {
        EVENT_HOOK.get_or_init(|| Mutex::new(None))
    }

    /// Install the host event logger used by SDK-based extensions.
    pub fn set_event_hook(hook: EventHook) {
        *hook_slot().lock().expect("extension sdk log hook poisoned") = Some(hook);
    }

    /// Emit one structured extension event.
    pub fn event(issue: &str, event: &str, message: &str) {
        let hook = *hook_slot().lock().expect("extension sdk log hook poisoned");
        match hook {
            Some(f) => f(issue, event, message),
            None => tracing::info!(issue = %issue, event = %event, "{message}"),
        }
    }
}

pub mod tools {
    use super::{Path, ServiceRegistry};
    use cap_chat::HostToolBridge;
    pub use tool_registry::{
        ToolExecutor, ToolOutcome, ToolRegistryHandle, ToolSpec, TOOL_REGISTRY_SERVICE,
    };

    /// Resolve the hidden host MCP bridge command for a chat or runner spawn.
    ///
    /// Keep this in sync with `runner_core::host_tool_bridge`; both helpers
    /// intentionally emit the same `__mcp-bridge --dir <agent-root>` shape.
    ///
    /// Returns `None` when no tool registry is present or it has no tools.
    pub fn host_tool_bridge(
        services: &ServiceRegistry,
        agent_root: &Path,
    ) -> Option<HostToolBridge> {
        let registry = services
            .get_named::<dyn ToolRegistryHandle>(TOOL_REGISTRY_SERVICE)
            .ok()?;
        if registry.is_empty() {
            return None;
        }
        let command = std::env::current_exe().ok()?.to_string_lossy().into_owned();
        Some(HostToolBridge {
            command,
            args: vec![
                "__mcp-bridge".to_string(),
                "--dir".to_string(),
                agent_root.display().to_string(),
            ],
        })
    }
}
