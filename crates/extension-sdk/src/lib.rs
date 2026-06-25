//! Public SDK for writing third-party dar extensions.
//!
//! This crate is the stable extension-author surface. Prefer depending on this
//! crate instead of individual dar workspace crates.

use std::path::Path;
use std::sync::{Mutex, OnceLock};

pub use host_api::{
    BoxFuture, ConfigStore, EventBus, Extension, HostPaths, RegisterCtx, ServiceRegistry,
    ShutdownToken, StartCtx,
};

pub mod chat {
    pub use cap_chat::{
        BoxFuture, ChatBackend, ChatEvent, ChatRole, ChatSession, ChatSessionParams,
        ChatSessionParamsBuilder, HostToolBridge, CHAT_FALLBACK_BACKEND,
    };
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
