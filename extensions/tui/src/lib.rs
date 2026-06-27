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
mod view;

use std::sync::Arc;

use anyhow::{Context as _, Result};
use host_api::{Extension, RegisterCtx};

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
    /// absent uses the default `data/tui/sessions`. No `agent.yaml` change is
    /// required to get the default behavior.
    pub sessions_dir: Option<String>,
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
                "chat": { "backend": "pi", "command": "pi" }
            }),
        );
        let mut ctx = register_ctx(host_api::ConfigStore::from_values(values));
        TuiExtension.register(&mut ctx).await.unwrap();
        assert!(ctx.foreground.select(Some("tui")).unwrap().is_some());
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
