//! Ratatui foreground (`foreground: tui`): interactive operator chat against
//! the agent folder, backed by a `cap-chat` backend resolved from the typed
//! service registry (config override → `runner.use`-follow → `"pi"`; see
//! `backend::resolve`), plus a Logs tab tailing `host.log-events` in
//! frontend-log's line format and a Dash tab over the orchestrator's
//! retained `RunSnapshot` (Tab/Shift+Tab cycle the tabs).
//!
//! **Dash tab presence — the deliberate spec reading.** The spec says the
//! dashboard tab is "present/active only if orchestrator enabled". This
//! crate reads that as: at startup the foreground tries
//! `subscribe_retained::<RunSnapshot>(RUN_SNAPSHOT_TOPIC)` — on Ok the Dash
//! tab joins the tab list, on Err (orchestrator not linked in dist, so the
//! topic was never registered) the tab is ABSENT entirely, not a disabled
//! placeholder. Its `p`/`r`/`s` keys publish `ControlMsg::{Pause,Resume,
//! Stop}` on `orchestrator.control` fire-and-forget and mutate no local
//! state: the orchestrator stays the single writer of run state, so the
//! paused badge flips only once the next retained snapshot reflects it.
//!
//! This extension registers NO bus topics and no services — it is a pure
//! consumer (`host.log-events` / `host.app-done` / `host.startup-banner` stay
//! owned by `frontend-log`; `orchestrator.run-snapshot` /
//! `orchestrator.control` by the orchestrator). On a non-interactive stdout
//! it degrades to the exact `frontend-log` line loop so piped/CI runs keep
//! working byte-for-byte.

mod app;
mod archive;
mod backend;
mod chat;
mod dash;
mod editor;
mod foreground;
mod input;
mod logs;
mod tools;
mod view;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use host_api::{Extension, RegisterCtx};
use tool_registry::{ToolRegistryHandle, TOOL_REGISTRY_SERVICE};

/// `extensions.tui` section of agent.yaml.
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TuiConfig {
    pub chat: ChatConfig,
}

/// `extensions.tui.chat`: which chat backend to drive and how.
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChatConfig {
    /// Registered `dyn ChatBackend` id; by default the agent's runner is
    /// followed when it has a chat backend, falling back to `"pi"`.
    pub backend: Option<String>,
    /// Backend binary override; empty/absent uses the backend default.
    pub command: Option<String>,
    /// Sessions dir override (relative to the agent root unless absolute);
    /// absent uses the shared default `data/chat/sessions`. No `agent.yaml` change is
    /// required to get the default behavior.
    pub sessions_dir: Option<String>,
    /// Per-turn timeout in seconds; absent uses [`DEFAULT_TURN_TIMEOUT`]
    /// (60 minutes). The TUI aborts the in-flight turn after this long and
    /// shows a retry notice. Validated at register time: must be > 0 and
    /// no larger than [`MAX_TURN_TIMEOUT_SECS`].
    pub turn_timeout_secs: Option<u64>,
}

/// Default per-turn timeout when `turn_timeout_secs` is omitted: 60 minutes.
pub const DEFAULT_TURN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3600);

/// Upper bound on a configured `turn_timeout_secs` (24 hours). A larger value
/// is almost certainly a mistake, so it is rejected at boot rather than
/// silently accepted.
pub const MAX_TURN_TIMEOUT_SECS: u64 = 86_400;

impl ChatConfig {
    /// The resolved per-turn timeout: the configured override or the default.
    pub fn turn_timeout(&self) -> std::time::Duration {
        self.turn_timeout_secs
            .map(std::time::Duration::from_secs)
            .unwrap_or(DEFAULT_TURN_TIMEOUT)
    }

    /// Reject nonsensical timeouts (zero or absurdly large) so a malformed
    /// `agent.yaml` fails cleanly at boot/doctor instead of at first use.
    fn validate(&self) -> Result<()> {
        if let Some(secs) = self.turn_timeout_secs {
            anyhow::ensure!(
                secs > 0,
                "extensions.tui.chat.turn_timeout_secs must be greater than 0"
            );
            anyhow::ensure!(
                secs <= MAX_TURN_TIMEOUT_SECS,
                "extensions.tui.chat.turn_timeout_secs must be <= {MAX_TURN_TIMEOUT_SECS} (24h)"
            );
        }
        Ok(())
    }
}

/// Resolve the TUI sessions dir from the chat config: the configured override
/// (relative paths anchored at the agent root) or the default
/// `data/chat/sessions`. Shared by the foreground (session open/resume) and the
/// `session_list` recall tool so both read the exact same corpus.
///
/// The default path is host-controlled (`<root>/data/chat/sessions`), so it is
/// composed directly rather than via the data-dir containment check, which
/// canonicalizes the `data/` parent — that parent need not exist yet at register
/// time (the foreground creates it lazily on first session open).
fn sessions_dir(config: &TuiConfig, paths: &host_api::HostPaths) -> Result<PathBuf> {
    match config
        .chat
        .sessions_dir
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        Some(dir) => {
            let path = std::path::Path::new(dir);
            Ok(if path.is_absolute() {
                path.to_path_buf()
            } else {
                paths.root().join(path)
            })
        }
        None => Ok(paths.root().join("data").join("chat").join("sessions")),
    }
}

pub struct TuiExtension;

impl Extension for TuiExtension {
    fn id(&self) -> &'static str {
        "tui"
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            // Parse + validate config here so a malformed `extensions.tui`
            // section is a clean boot failure, not a surprise at first use.
            let config = match ctx.config.get("tui") {
                Some(value) => serde_json::from_value::<TuiConfig>(value.clone())
                    .context("invalid extensions.tui config")?,
                None => TuiConfig::default(),
            };
            config.chat.validate()?;
            // Register recall tools (`session_list`) against the shared tool
            // registry, but only when the registry service is present — the
            // same conditional wiring as the foreground's `host_tool_bridge`.
            // The tools reach the agent through the existing host MCP bridge;
            // this adds no new transport. With no registry service the tools
            // simply aren't registered and the agent never sees them.
            if let Ok(registry) = ctx
                .services
                .get_named::<dyn ToolRegistryHandle>(TOOL_REGISTRY_SERVICE)
            {
                let sessions_dir = sessions_dir(&config, &ctx.paths)?;
                tools::register_into(registry.as_ref(), sessions_dir)?;
            }
            ctx.foreground.foreground_raw_mode(
                "tui",
                Arc::new(move || Box::new(foreground::TuiForeground::new(config.clone()))),
            )?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use host_api::{LogEvent, APP_DONE_TOPIC, LOG_EVENTS_TOPIC, STARTUP_BANNER_TOPIC};

    use super::*;

    fn register_ctx(config: host_api::ConfigStore) -> RegisterCtx {
        let temp = tempfile::tempdir().unwrap();
        let paths = host_api::HostPaths::new(temp.path()).unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(false);
        RegisterCtx {
            bus: host_api::EventBus::new(),
            http: host_api::HttpRegistry::disabled(),
            foreground: host_api::ForegroundRegistry::default(),
            services: host_api::ServiceRegistry::default(),
            paths,
            config,
            shutdown: host_api::ShutdownToken::new(rx),
        }
    }

    #[tokio::test]
    async fn registers_recall_tools_when_registry_service_present() {
        let mut ctx = register_ctx(host_api::ConfigStore::default());
        let registry: Arc<dyn ToolRegistryHandle> = Arc::new(tool_registry::ToolRegistry::new());
        ctx.services
            .service::<dyn ToolRegistryHandle>(TOOL_REGISTRY_SERVICE, registry.clone())
            .unwrap();

        TuiExtension.register(&mut ctx).await.unwrap();

        let names: Vec<String> = registry.list().into_iter().map(|s| s.name).collect();
        for tool in ["session_list", "session_search", "session_read"] {
            assert!(
                names.contains(&tool.to_string()),
                "{tool} must be registered when the registry is present: {names:?}"
            );
        }
    }

    #[tokio::test]
    async fn registers_nothing_extra_when_registry_service_absent() {
        // No registry service registered: register() must still succeed and the
        // foreground stays available; the tool simply isn't registered.
        let mut ctx = register_ctx(host_api::ConfigStore::default());
        TuiExtension.register(&mut ctx).await.unwrap();
        assert!(ctx
            .services
            .get_named::<dyn ToolRegistryHandle>(TOOL_REGISTRY_SERVICE)
            .is_err());
        assert!(ctx.foreground.select(Some("tui")).unwrap().is_some());
    }

    #[tokio::test]
    async fn registers_tui_foreground_and_no_topics() {
        let mut ctx = register_ctx(host_api::ConfigStore::default());
        TuiExtension.register(&mut ctx).await.unwrap();

        assert!(ctx.foreground.select(Some("tui")).unwrap().is_some());
        assert!(ctx.foreground.select(Some("missing")).is_err());
        // tui is consume-only: it must not register (own) any bus topics.
        assert!(ctx.bus.subscribe::<LogEvent>(LOG_EVENTS_TOPIC).is_err());
        assert!(ctx.bus.subscribe_retained::<bool>(APP_DONE_TOPIC).is_err());
        assert!(ctx
            .bus
            .subscribe_retained::<Option<LogEvent>>(STARTUP_BANNER_TOPIC)
            .is_err());
    }

    #[tokio::test]
    async fn valid_config_parses_at_register() {
        let mut values = std::collections::HashMap::new();
        values.insert(
            "tui".to_string(),
            serde_json::json!({
                "chat": { "backend": "pi", "command": "pi", "turn_timeout_secs": 3600 }
            }),
        );
        let mut ctx = register_ctx(host_api::ConfigStore::from_values(values));
        TuiExtension.register(&mut ctx).await.unwrap();
        assert!(ctx.foreground.select(Some("tui")).unwrap().is_some());
    }

    #[test]
    fn turn_timeout_defaults_when_omitted() {
        assert_eq!(ChatConfig::default().turn_timeout(), DEFAULT_TURN_TIMEOUT);
        assert_eq!(DEFAULT_TURN_TIMEOUT, std::time::Duration::from_secs(3600));
    }

    #[test]
    fn turn_timeout_secs_overrides_default() {
        let config = ChatConfig {
            turn_timeout_secs: Some(7200),
            ..Default::default()
        };
        assert_eq!(config.turn_timeout(), std::time::Duration::from_secs(7200));
    }

    #[test]
    fn validate_rejects_zero_and_oversized_timeouts() {
        for bad in [0, MAX_TURN_TIMEOUT_SECS + 1] {
            let config = ChatConfig {
                turn_timeout_secs: Some(bad),
                ..Default::default()
            };
            assert!(
                config.validate().is_err(),
                "turn_timeout_secs={bad} must be rejected"
            );
        }
        // Bounds and omission are accepted.
        for ok in [None, Some(1), Some(MAX_TURN_TIMEOUT_SECS)] {
            let config = ChatConfig {
                turn_timeout_secs: ok,
                ..Default::default()
            };
            assert!(config.validate().is_ok(), "turn_timeout_secs={ok:?} ok");
        }
    }

    #[tokio::test]
    async fn invalid_turn_timeout_fails_register() {
        let mut values = std::collections::HashMap::new();
        values.insert(
            "tui".to_string(),
            serde_json::json!({ "chat": { "turn_timeout_secs": 0 } }),
        );
        let mut ctx = register_ctx(host_api::ConfigStore::from_values(values));
        let err = TuiExtension.register(&mut ctx).await.unwrap_err();
        assert!(err.to_string().contains("turn_timeout_secs"));
    }

    #[tokio::test]
    async fn malformed_config_fails_register() {
        // Wrong type and unknown key (deny_unknown_fields) both fail the boot.
        for bad in [
            serde_json::json!({ "chat": { "backend": 5 } }),
            serde_json::json!({ "chat": { "bckend": "pi" } }),
            serde_json::json!({ "caht": {} }),
        ] {
            let mut values = std::collections::HashMap::new();
            values.insert("tui".to_string(), bad);
            let mut ctx = register_ctx(host_api::ConfigStore::from_values(values));
            let err = TuiExtension.register(&mut ctx).await.unwrap_err();
            assert!(err.to_string().contains("invalid extensions.tui config"));
        }
    }
}
