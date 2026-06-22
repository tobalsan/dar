//! Shared host MCP bridge config writers.
//!
//! Both issue-worker runners (`runner-pi`, `runner-codex`, `runner-opencode`)
//! and interactive TUI chat backends (`chat-pi`, `chat-codex`, `chat-opencode`)
//! advertise the **same** host MCP bridge to their backend so an agent can call
//! host-registered extension tools. Keeping the per-backend config shape in one
//! place guarantees chat and worker spawns stay byte-for-byte identical — a chat
//! agent sees exactly the registry tools an issue worker does.

use std::ffi::OsString;
use std::path::Path;

use anyhow::{Context, Result};
use cap_runner::HostToolBridge;
use host_api::ServiceRegistry;
use serde_json::{json, Value};
use tool_registry::{ToolRegistryHandle, TOOL_REGISTRY_SERVICE};

/// Name of the host MCP bridge as advertised to every backend. Stable so pi's
/// per-session metadata cache, `MCP_DIRECT_TOOLS`, codex `mcp_servers.<name>`,
/// and opencode `mcp.<name>` all refer to the same server.
pub const BRIDGE_SERVER_NAME: &str = "agentropy";

/// Extra backend CLI args plus process env produced when wiring the host MCP
/// bridge: `(args, env)`.
pub type BridgeInvocation = (Vec<OsString>, Vec<(OsString, OsString)>);

/// Resolve the hidden host MCP bridge command for a runner/chat spawn. Returns
/// `None` when no tool registry is present or it has no tools, preserving the
/// cheap no-tools path for agents that do not use runtime tools.
pub fn host_tool_bridge(services: &ServiceRegistry, agent_root: &Path) -> Option<HostToolBridge> {
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

// ---------------------------------------------------------------------------
// pi (--mcp-config)
// ---------------------------------------------------------------------------

/// Per-session pi settings sub-tree that makes the host bridge's tools reliable
/// on the first turn:
/// - `directTools: true` promotes the bridge's tools to first-class pi tools.
/// - `disableProxyTool: false` keeps the always-present proxy `mcp` tool as the
///   cold-start floor (registered synchronously at load, lazy-connects on call),
///   so the host tool is reachable on the very first turn even before the async
///   server bootstrap finishes.
fn pi_bridge_settings() -> Value {
    json!({
        "toolPrefix": "none",
        "directTools": true,
        "disableProxyTool": false,
    })
}

/// The pi `--mcp-config` document advertising the host bridge as a stdio MCP
/// server. Matches pi-mcp-adapter's schema: `{ "mcpServers": { <name>: { command,
/// args } }, "settings": { ... } }`. The server is marked `lifecycle: "eager"`
/// so pi connects (and warms the per-session metadata cache) during
/// `session_start` rather than waiting for the first call.
fn pi_bridge_mcp_config(bridge: &HostToolBridge) -> Value {
    json!({
        "mcpServers": {
            BRIDGE_SERVER_NAME: {
                "command": bridge.command,
                "args": bridge.args,
                "lifecycle": "eager",
            }
        },
        "settings": pi_bridge_settings(),
    })
}

/// Materialize the per-session pi MCP config for the host bridge, returning the
/// extra `pi` CLI args (`--mcp-config <file>`) and process env (`MCP_DIRECT_TOOLS`)
/// to apply. Writes `<session_dir>/mcp-config.json`.
pub fn pi_mcp_config_args(session_dir: &Path, bridge: &HostToolBridge) -> Result<BridgeInvocation> {
    let config_path = session_dir.join("mcp-config.json");
    let config = pi_bridge_mcp_config(bridge);
    std::fs::write(&config_path, format!("{config:#}\n"))
        .with_context(|| format!("writing pi mcp config {}", config_path.display()))?;

    let args = vec![OsString::from("--mcp-config"), config_path.into_os_string()];
    let env = vec![(
        OsString::from("MCP_DIRECT_TOOLS"),
        OsString::from(BRIDGE_SERVER_NAME),
    )];
    Ok((args, env))
}

// ---------------------------------------------------------------------------
// codex (-c mcp_servers.<name>.*)
// ---------------------------------------------------------------------------

/// Render a string as a TOML basic string literal (quoted, escaped).
fn toml_string(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Build the `-c mcp_servers.agentropy.*` flags advertising the host MCP bridge
/// to `codex app-server`. The agent reaches the bridge by spawning
/// `command args...` over stdio; tool results return in-session.
pub fn codex_mcp_bridge_args(bridge: &HostToolBridge) -> Vec<OsString> {
    // codex parses `-c key=value` values as TOML; render command/args as TOML
    // string + array literals.
    let command_toml = toml_string(&bridge.command);
    let args_toml = format!(
        "[{}]",
        bridge
            .args
            .iter()
            .map(|a| toml_string(a))
            .collect::<Vec<_>>()
            .join(", ")
    );
    vec![
        OsString::from("-c"),
        OsString::from(format!(
            "mcp_servers.{BRIDGE_SERVER_NAME}.command={command_toml}"
        )),
        OsString::from("-c"),
        OsString::from(format!("mcp_servers.{BRIDGE_SERVER_NAME}.args={args_toml}")),
    ]
}

// ---------------------------------------------------------------------------
// opencode (mcp.<name> block)
// ---------------------------------------------------------------------------

/// Build the `mcp.agentropy` block pointing at the host MCP bridge. opencode
/// spawns a `type: "local"` server as `command[0] command[1..]` over stdio; the
/// host-registered tools surface namespaced `agentropy_<tool>` and their results
/// return over SSE in the same session.
pub fn opencode_mcp_block(bridge: &HostToolBridge) -> Value {
    let mut command = Vec::with_capacity(1 + bridge.args.len());
    command.push(Value::String(bridge.command.clone()));
    command.extend(bridge.args.iter().map(|a| Value::String(a.clone())));
    json!({
        BRIDGE_SERVER_NAME: {
            "type": "local",
            "command": command,
            "enabled": true,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn bridge() -> HostToolBridge {
        HostToolBridge {
            command: "/opt/agentropy".to_string(),
            args: vec![
                "__mcp-bridge".to_string(),
                "--dir".to_string(),
                "/tmp/agent".to_string(),
            ],
        }
    }

    #[test]
    fn host_tool_bridge_none_without_registry_or_tools() {
        let root = tempfile::tempdir().unwrap();
        let services = ServiceRegistry::default();
        assert!(host_tool_bridge(&services, root.path()).is_none());

        let mut services = ServiceRegistry::default();
        services
            .service::<dyn ToolRegistryHandle>(
                TOOL_REGISTRY_SERVICE,
                Arc::new(tool_registry::ToolRegistry::new()),
            )
            .unwrap();
        assert!(host_tool_bridge(&services, root.path()).is_none());
    }

    #[test]
    fn host_tool_bridge_some_when_registry_has_tool() {
        struct Noop;
        #[async_trait::async_trait]
        impl tool_registry::ToolExecutor for Noop {
            async fn execute(&self, _args: Value) -> anyhow::Result<tool_registry::ToolOutcome> {
                Ok(tool_registry::ToolOutcome::ok("ok"))
            }
        }

        let root = tempfile::tempdir().unwrap();
        let registry = tool_registry::ToolRegistry::new();
        registry
            .register_tool(
                tool_registry::ToolSpec::new("noop", "noop", json!({ "type": "object" })),
                Arc::new(Noop),
            )
            .unwrap();
        let mut services = ServiceRegistry::default();
        services
            .service::<dyn ToolRegistryHandle>(TOOL_REGISTRY_SERVICE, Arc::new(registry))
            .unwrap();

        let bridge = host_tool_bridge(&services, root.path()).unwrap();
        assert_eq!(bridge.args[0], "__mcp-bridge");
        assert_eq!(bridge.args[1], "--dir");
        assert_eq!(bridge.args[2], root.path().display().to_string());
    }

    #[test]
    fn pi_mcp_config_args_writes_config_and_returns_flag_and_env() {
        let dir = tempfile::tempdir().unwrap();
        let (args, env) = pi_mcp_config_args(dir.path(), &bridge()).unwrap();
        assert_eq!(args[0], OsString::from("--mcp-config"));
        let config_path = dir.path().join("mcp-config.json");
        assert_eq!(args[1], config_path.as_os_str());
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(
            written["mcpServers"][BRIDGE_SERVER_NAME]["command"],
            "/opt/agentropy"
        );
        assert_eq!(
            written["mcpServers"][BRIDGE_SERVER_NAME]["args"],
            json!(["__mcp-bridge", "--dir", "/tmp/agent"])
        );
        assert_eq!(
            written["mcpServers"][BRIDGE_SERVER_NAME]["lifecycle"],
            "eager"
        );
        assert_eq!(env[0].0, OsString::from("MCP_DIRECT_TOOLS"));
        assert_eq!(env[0].1, OsString::from(BRIDGE_SERVER_NAME));
    }

    #[test]
    fn codex_mcp_bridge_args_render_toml_command_and_args() {
        let rendered: Vec<String> = codex_mcp_bridge_args(&bridge())
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(rendered[0], "-c");
        assert_eq!(
            rendered[1],
            "mcp_servers.agentropy.command=\"/opt/agentropy\""
        );
        assert_eq!(rendered[2], "-c");
        assert_eq!(
            rendered[3],
            "mcp_servers.agentropy.args=[\"__mcp-bridge\", \"--dir\", \"/tmp/agent\"]"
        );
    }

    #[test]
    fn codex_mcp_bridge_args_escape_quotes_and_backslashes() {
        let b = HostToolBridge {
            command: "/path/with \"quote\"".to_string(),
            args: vec!["a\\b".to_string()],
        };
        let rendered: Vec<String> = codex_mcp_bridge_args(&b)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            rendered[1],
            "mcp_servers.agentropy.command=\"/path/with \\\"quote\\\"\""
        );
        assert_eq!(rendered[3], "mcp_servers.agentropy.args=[\"a\\\\b\"]");
    }

    #[test]
    fn opencode_mcp_block_folds_command_and_args() {
        let block = opencode_mcp_block(&bridge());
        let server = &block[BRIDGE_SERVER_NAME];
        assert_eq!(server["type"], "local");
        assert_eq!(server["enabled"], true);
        assert_eq!(
            server["command"],
            json!(["/opt/agentropy", "__mcp-bridge", "--dir", "/tmp/agent"])
        );
    }
}
