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
pub const BRIDGE_SERVER_NAME: &str = "dar";

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

/// Build the `-c mcp_servers.dar.*` flags advertising the host MCP bridge
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

/// Build the `mcp.dar` block pointing at the host MCP bridge. opencode
/// spawns a `type: "local"` server as `command[0] command[1..]` over stdio; the
/// host-registered tools surface namespaced `dar_<tool>` and their results
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

/// The full opencode config document shared by the issue-worker runner and the
/// TUI chat backend, so both spawn a byte-for-byte identical opencode child:
/// the `$schema`, the ~17-key permission allow-list, the optional `model`, and
/// the optional `mcp.dar` host-bridge block. The per-extension XDG /
/// `OPENCODE_CONFIG*` env assembly stays in each extension.
pub fn opencode_config(model: Option<&str>, bridge: Option<&HostToolBridge>) -> Value {
    let mut config = json!({
        "$schema": "https://opencode.ai/config.json",
        "permission": {
            "*": "allow",
            "bash": "allow",
            "doom_loop": "allow",
            "edit": "allow",
            "external_directory": "allow",
            "glob": "allow",
            "grep": "allow",
            "list": "allow",
            "lsp": "allow",
            "question": "allow",
            "read": "allow",
            "skill": "allow",
            "task": "allow",
            "todowrite": "allow",
            "todoread": "allow",
            "webfetch": "allow",
            "websearch": "allow",
            "write": "allow",
        },
    });
    if let Some(model) = model {
        config["model"] = Value::String(model.to_string());
    }
    // Wire the host MCP bridge so the opencode agent calls the same registry
    // tools an issue worker does; tools surface namespaced `dar_<tool>`
    // and results return over SSE in the same session.
    if let Some(bridge) = bridge {
        config["mcp"] = opencode_mcp_block(bridge);
    }
    config
}

/// Write the shared [`opencode_config`] to `<session_dir>/config/opencode.json`
/// (pretty-printed), the on-disk path both the runner and chat backend point
/// `OPENCODE_CONFIG` at.
pub fn write_opencode_config(
    session_dir: &Path,
    model: Option<&str>,
    bridge: Option<&HostToolBridge>,
) -> Result<()> {
    let path = session_dir.join("config").join("opencode.json");
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&opencode_config(model, bridge))?,
    )
    .with_context(|| format!("writing opencode config {}", path.display()))
}

// ---------------------------------------------------------------------------
// codex (app-server JSON-RPC 2.0 request builders)
// ---------------------------------------------------------------------------

/// `initialize` request. Shared verbatim by `runner-codex` and `chat-codex` so
/// both handshake identically with `codex app-server`.
pub fn make_initialize(id: u64, workspace: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "clientInfo": { "name": "dar", "version": "0.1.0" },
            "capabilities": { "experimentalApi": true },
            "cwd": workspace,
        }
    })
}

/// `initialized` notification (no response expected).
pub fn make_initialized() -> Value {
    json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} })
}

/// `thread/start` request.
pub fn make_thread_start(id: u64, workspace: &str, model: Option<&str>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "thread/start",
        "params": {
            "model": model,
            "cwd": workspace,
            "approvalPolicy": "never",
            "sandbox": "danger-full-access",
            "serviceName": "dar",
        }
    })
}

/// `turn/start` request. The optional `effort` field is OMITTED when `None` and
/// emitted when `Some` (an absent optional field and an explicit `null` are
/// equivalent to codex app-server). `runner-codex` passes the issue's
/// `thinking` effort; `chat-codex` always passes `None`.
pub fn make_turn_start(
    id: u64,
    thread_id: &str,
    workspace: &str,
    prompt: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> Value {
    let mut params = json!({
        "threadId": thread_id,
        "input": [{ "type": "text", "text": prompt }],
        "cwd": workspace,
        "model": model,
        "approvalPolicy": "never",
        "sandboxPolicy": { "type": "dangerFullAccess" },
    });
    if let Some(effort) = effort {
        params["effort"] = Value::String(effort.to_string());
    }
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "turn/start",
        "params": params,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bridge() -> HostToolBridge {
        HostToolBridge {
            command: "/opt/dar".to_string(),
            args: vec![
                "__mcp-bridge".to_string(),
                "--dir".to_string(),
                "/tmp/agent".to_string(),
            ],
        }
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
            "/opt/dar"
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
            "mcp_servers.dar.command=\"/opt/dar\""
        );
        assert_eq!(rendered[2], "-c");
        assert_eq!(
            rendered[3],
            "mcp_servers.dar.args=[\"__mcp-bridge\", \"--dir\", \"/tmp/agent\"]"
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
            "mcp_servers.dar.command=\"/path/with \\\"quote\\\"\""
        );
        assert_eq!(rendered[3], "mcp_servers.dar.args=[\"a\\\\b\"]");
    }

    #[test]
    fn opencode_mcp_block_folds_command_and_args() {
        let block = opencode_mcp_block(&bridge());
        let server = &block[BRIDGE_SERVER_NAME];
        assert_eq!(server["type"], "local");
        assert_eq!(server["enabled"], true);
        assert_eq!(
            server["command"],
            json!(["/opt/dar", "__mcp-bridge", "--dir", "/tmp/agent"])
        );
    }

    #[test]
    fn opencode_config_allows_permissions_and_uses_model_when_set() {
        let config = opencode_config(Some("anthropic/claude-sonnet"), None);
        assert_eq!(config["permission"]["*"], "allow");
        assert_eq!(config["permission"]["bash"], "allow");
        assert_eq!(config["permission"]["external_directory"], "allow");
        assert_eq!(config["permission"]["edit"], "allow");
        assert_eq!(config["permission"]["question"], "allow");
        assert_eq!(config["permission"]["webfetch"], "allow");
        assert_eq!(config["model"], "anthropic/claude-sonnet");
        assert!(opencode_config(None, None).get("model").is_none());
    }

    #[test]
    fn opencode_config_has_no_mcp_block_without_bridge() {
        assert!(opencode_config(Some("anthropic/claude-sonnet"), None)
            .get("mcp")
            .is_none());
    }

    #[test]
    fn opencode_config_writes_dar_mcp_block_when_bridge_present() {
        let config = opencode_config(Some("anthropic/claude-sonnet"), Some(&bridge()));
        let server = &config["mcp"][BRIDGE_SERVER_NAME];
        assert_eq!(server["type"], "local");
        assert_eq!(server["enabled"], true);
        assert_eq!(
            server["command"],
            json!(["/opt/dar", "__mcp-bridge", "--dir", "/tmp/agent"])
        );
        assert_eq!(config["permission"]["*"], "allow");
    }

    #[test]
    fn write_opencode_config_persists_mcp_block_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        write_opencode_config(dir.path(), Some("anthropic/claude-sonnet"), Some(&bridge()))
            .unwrap();
        let written = std::fs::read_to_string(dir.path().join("config/opencode.json")).unwrap();
        let config: Value = serde_json::from_str(&written).unwrap();
        assert_eq!(
            config["mcp"][BRIDGE_SERVER_NAME]["command"],
            json!(["/opt/dar", "__mcp-bridge", "--dir", "/tmp/agent"])
        );
        assert_eq!(config["permission"]["*"], "allow");
    }

    #[test]
    fn initialize_request_shape() {
        let req = make_initialize(1, "/ws/ISSUE-1");
        assert_eq!(req["jsonrpc"], "2.0");
        assert_eq!(req["id"], 1);
        assert_eq!(req["method"], "initialize");
        assert_eq!(req["params"]["clientInfo"]["name"], "dar");
        assert_eq!(req["params"]["capabilities"]["experimentalApi"], true);
        assert_eq!(req["params"]["cwd"], "/ws/ISSUE-1");
    }

    #[test]
    fn initialized_notification_shape() {
        let req = make_initialized();
        assert_eq!(req["method"], "initialized");
        assert!(req.get("id").is_none());
    }

    #[test]
    fn thread_start_request_shape() {
        let req = make_thread_start(2, "/ws/ISSUE-1", Some("o3"));
        assert_eq!(req["method"], "thread/start");
        assert_eq!(req["params"]["approvalPolicy"], "never");
        assert_eq!(req["params"]["sandbox"], "danger-full-access");
        assert_eq!(req["params"]["serviceName"], "dar");
        assert_eq!(req["params"]["model"], "o3");
        assert_eq!(req["params"]["cwd"], "/ws/ISSUE-1");
    }

    #[test]
    fn thread_start_null_model_when_none() {
        assert!(make_thread_start(2, "/ws/ISSUE-1", None)["params"]["model"].is_null());
    }

    #[test]
    fn turn_start_request_shape_emits_effort_when_some() {
        let req = make_turn_start(
            3,
            "t1",
            "/ws/ISSUE-1",
            "do something",
            Some("o3"),
            Some("high"),
        );
        assert_eq!(req["method"], "turn/start");
        assert_eq!(req["params"]["threadId"], "t1");
        assert_eq!(req["params"]["input"][0]["type"], "text");
        assert_eq!(req["params"]["input"][0]["text"], "do something");
        assert_eq!(req["params"]["approvalPolicy"], "never");
        assert_eq!(req["params"]["sandboxPolicy"]["type"], "dangerFullAccess");
        assert_eq!(req["params"]["model"], "o3");
        assert_eq!(req["params"]["effort"], "high");
    }

    #[test]
    fn turn_start_omits_effort_key_when_none() {
        let req = make_turn_start(3, "t1", "/ws/ISSUE-1", "do", None, None);
        // An absent optional field and an explicit null are equivalent to codex
        // app-server; the unified builder omits the key when effort is None.
        assert!(req["params"].get("effort").is_none());
        assert!(req["params"]["model"].is_null());
    }
}
