//! `reload_secrets` — an agent-invokable host tool that refreshes rotated
//! secrets without restarting the host.
//!
//! When an agent rotates its `LINEAR_API_KEY` / `LINEAR_OAUTH_TOKEN` (or any
//! other `.env`-loaded secret) in `<agent-root>/.env`, two caches would
//! otherwise freeze the old value until a full process restart:
//!
//! 1. `.env` is read once at extension start and copied into the process env.
//! 2. The Linear tracker bakes its auth header into a struct at construction.
//!
//! This tool is registered against the shared [`ToolRegistry`] during the
//! orchestrator's `register()` pass, so it is reachable in every process that
//! builds the registry — including the host-owned `__mcp-bridge` subprocess the
//! agent's runner talks to. On call it:
//!
//! - **re-reads `.env`** ([`crate::dotenv::reload_agent_env`]), overriding only
//!   the keys originally loaded from the file (never genuine process env). This
//!   alone fixes the `linear_graphql` MCP tool, which resolves its auth header
//!   fresh from the env on every call — in the same bridge process.
//! - **swaps the live tracker's cached token** when running inside `dar run`:
//!   it publishes a [`ControlMsg::ReloadSecrets`] on the control bus (single
//!   writer preserved — the orchestrator performs the swap) and reports the
//!   result. In the bridge subprocess there is no orchestrator/bus, so this
//!   step is a no-op and only the env re-read applies.
//!
//! Secret *values* never appear in the tool result or logs — only key names and
//! counts, mirroring the `scrub_loaded_env` / `Redactor` guarantees elsewhere.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Result;
use host_api::EventBus;
use serde_json::{json, Value};
use tool_registry::{ToolExecutor, ToolOutcome, ToolRegistryHandle, ToolSpec};

use orchestrator_api::{reply_channel, ControlMsg, CONTROL_TOPIC};

/// The tool name agents call by.
pub const TOOL_NAME: &str = "reload_secrets";

/// Process-global handle to the live control bus, populated by the orchestrator
/// during `start()` (only in the `dar run` host process). The `reload_secrets`
/// tool consults it: when present it publishes a `ReloadSecrets` control
/// message so the running orchestrator swaps its tracker's cached token; when
/// absent (e.g. the `__mcp-bridge` subprocess, which never runs `start()`) the
/// tool only re-reads `.env`.
fn control_bus() -> &'static OnceLock<Arc<EventBus>> {
    static BUS: OnceLock<Arc<EventBus>> = OnceLock::new();
    &BUS
}

/// Register the live control bus so `reload_secrets` can trigger an in-host
/// tracker token swap. Called once from the orchestrator's `start()`. A second
/// call is ignored (the first live bus wins).
pub fn set_control_bus(bus: Arc<EventBus>) {
    let _ = control_bus().set(bus);
}

/// The MCP/registry tool spec (name + description + input schema).
pub fn spec() -> ToolSpec {
    ToolSpec::new(
        TOOL_NAME,
        "Reload secrets from the agent's .env without restarting the host. \
         Call this after rotating LINEAR_API_KEY / LINEAR_OAUTH_TOKEN (or any \
         other .env secret) so the next Linear request uses the new token. \
         Re-reads .env (overriding only keys loaded from the file) and swaps \
         the Linear client's cached token in place. Secret values are never \
         returned.",
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        }),
    )
    // Not a read; it mutates host secret state.
    .with_access(false, true)
}

/// Register the `reload_secrets` tool against the shared registry, capturing the
/// agent root to read `.env` from. Called from the orchestrator's `register()`.
pub fn register_into(registry: &dyn ToolRegistryHandle, root: PathBuf) -> Result<()> {
    registry.register_tool(spec(), Arc::new(ReloadSecretsTool { root }))
}

struct ReloadSecretsTool {
    root: PathBuf,
}

#[async_trait::async_trait]
impl ToolExecutor for ReloadSecretsTool {
    async fn execute(&self, _args: Value) -> Result<ToolOutcome> {
        // 1) Re-read .env, overriding only keys we originally loaded from it.
        let report = match crate::dotenv::reload_agent_env(&self.root) {
            Ok(report) => report,
            Err(err) => {
                return Ok(ToolOutcome::error_code(
                    "reload_env_failed",
                    format!("reload_secrets failed reading .env: {err}"),
                    None::<String>,
                ));
            }
        };

        // 2) When running inside `dar run`, ask the orchestrator (single writer)
        //    to swap the tracker's cached token. Absent bus → bridge process,
        //    where the env re-read above already refreshes `linear_graphql`.
        let tracker_status = match control_bus().get() {
            Some(bus) => {
                let (reply, rx) = reply_channel();
                match bus.publish(CONTROL_TOPIC, ControlMsg::ReloadSecrets { reply }) {
                    Ok(()) => match tokio::time::timeout(Duration::from_secs(10), rx).await {
                        Ok(Ok(reply)) if reply.ok => "tracker refreshed".to_string(),
                        Ok(Ok(reply)) => format!("tracker refresh failed: {}", reply.message),
                        Ok(Err(_)) | Err(_) => "tracker refresh timed out or dropped".to_string(),
                    },
                    Err(err) => format!("tracker refresh not delivered: {err}"),
                }
            }
            None => "no live tracker in this process".to_string(),
        };

        let summary = if report.found {
            format!(
                "reloaded {} key(s) from .env ({} left as external process env); {tracker_status}",
                report.reloaded.len(),
                report.skipped_external.len(),
            )
        } else {
            format!(
                "no .env found at {}; {tracker_status}",
                report.path.display()
            )
        };
        Ok(ToolOutcome::ok(summary))
    }
}
