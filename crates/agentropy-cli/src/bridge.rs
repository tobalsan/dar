//! Host-owned MCP bridge.
//!
//! A small stdio MCP server, spawned and owned by the host, that exposes the
//! extension [`ToolRegistry`] over the Model Context Protocol and executes tool
//! calls *in the host runtime* using each extension's config/secrets. Agent
//! backends (codex `app-server`, etc.) are pointed at this process via their MCP
//! server config; the agent only ever sees tool names, schemas, and secret-redacted
//! structured results — never the host's secrets.
//!
//! It is the same `agentropy` binary re-invoked as a hidden subcommand
//! (`agentropy __mcp-bridge --dir <agent>`). On start it loads the agent's
//! `.env` and runs the extension `register()` pass to populate the registry,
//! then serves MCP requests on stdin/stdout until EOF.
//!
//! Transport: newline-delimited JSON-RPC 2.0 (MCP stdio framing). Supported
//! methods: `initialize`, `tools/list`, `tools/call`. Notifications and unknown
//! methods are answered per JSON-RPC (`-32601` for unknown requests; ignored for
//! notifications).

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use host_api::Extension;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tool_registry::{Redactor, ToolCallObservation, ToolRegistryHandle, TOOL_REGISTRY_SERVICE};

/// MCP protocol version this bridge advertises.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Build the populated tool registry for `root` by running the extension
/// `register()` pass with the agent's `.env` loaded. Returns the resolved
/// registry handle (which fails the boot if any extension reports a duplicate
/// tool name).
pub async fn build_registry(
    root: &Path,
    plugins: Vec<Arc<dyn Extension>>,
) -> Result<(Arc<dyn ToolRegistryHandle>, Redactor)> {
    // Load the agent's secrets into this process so executors can use them.
    // Build a redactor from exactly those `.env`-loaded keys so the same
    // secrets `runner-core::scrub_loaded_env` strips from child spawns are also
    // masked out of anything this bridge logs (args/results) — extending the
    // scrub guarantee to the bridge process.
    let report = orchestrator::dotenv::load_agent_env(root)
        .with_context(|| format!("loading .env for {}", root.display()))?;
    let redactor = Redactor::from_env_keys(report.loaded);
    let services = crate::plugin_services(root, plugins).await?;
    let registry = services
        .get_named::<dyn ToolRegistryHandle>(TOOL_REGISTRY_SERVICE)
        .context("tool registry service not registered (tool-registry-host extension missing?)")?;
    Ok((registry, redactor))
}

/// Entry point for the `__mcp-bridge` subcommand: build the registry and serve
/// MCP over stdin/stdout until EOF.
pub async fn serve(root: &Path, plugins: Vec<Arc<dyn Extension>>) -> Result<()> {
    let (registry, redactor) = build_registry(root, plugins).await?;
    serve_stdio(registry, redactor, tokio::io::stdin(), tokio::io::stdout()).await
}

/// Drive the MCP request loop over arbitrary async byte streams (factored out so
/// integration tests can drive it over pipes).
pub async fn serve_stdio<R, W>(
    registry: Arc<dyn ToolRegistryHandle>,
    redactor: Redactor,
    input: R,
    mut output: W,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut lines = BufReader::new(input).lines();
    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(_) => {
                write_message(&mut output, &parse_error()).await?;
                continue;
            }
        };
        if let Some(response) = handle_message(&registry, &redactor, &request).await {
            write_message(&mut output, &response).await?;
        }
    }
    Ok(())
}

/// Handle one JSON-RPC message. Returns `Some(response)` for requests and `None`
/// for notifications (no `id`).
async fn handle_message(
    registry: &Arc<dyn ToolRegistryHandle>,
    redactor: &Redactor,
    request: &Value,
) -> Option<Value> {
    let method = request.get("method").and_then(Value::as_str);

    // Notifications carry no id and expect no response.
    let id = request.get("id").cloned()?;

    let response = match method {
        Some("initialize") => result(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "serverInfo": { "name": "agentropy", "version": env!("CARGO_PKG_VERSION") },
                "capabilities": { "tools": {} },
            }),
        ),
        Some("tools/list") => {
            let tools: Vec<Value> = registry.list().iter().map(|s| s.to_mcp_tool()).collect();
            result(id, json!({ "tools": tools }))
        }
        Some("tools/call") => handle_tools_call(registry, redactor, id, request).await,
        Some("ping") => result(id, json!({})),
        _ => error(id, -32601, "method not found"),
    };
    Some(response)
}

async fn handle_tools_call(
    registry: &Arc<dyn ToolRegistryHandle>,
    redactor: &Redactor,
    id: Value,
    request: &Value,
) -> Value {
    let params = request.get("params");
    let name = params.and_then(|p| p.get("name")).and_then(Value::as_str);
    let Some(name) = name else {
        return error(id, -32602, "invalid params: missing tool name");
    };
    let args = params
        .and_then(|p| p.get("arguments"))
        .cloned()
        .unwrap_or_else(|| json!({}));

    // A failed tool is a *result* (`isError: true`), not a JSON-RPC error — it
    // returns to the agent so the run continues. Dispatch through the observed
    // path so each call emits a redacted+truncated runtime log carrying tool
    // name, status, duration, and read/write metadata — no raw payload dumps.
    let (outcome, observation) = registry.dispatch_observed(name, args, redactor).await;
    emit_observation(&observation);
    result(id, outcome.redacted(redactor).to_mcp_result())
}

/// Emit a tool-call observation as a runtime log line. stdout is the MCP
/// JSON-RPC channel, so the human-readable line goes to stderr, which the host
/// captures as the MCP server's log stream. The line is already redacted and
/// truncated by `dispatch_observed`, so no host secret or raw payload appears.
fn emit_observation(observation: &ToolCallObservation) {
    eprintln!("[agentropy:tool] {}", observation.log_line());
}

fn result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn parse_error() -> Value {
    json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "parse error" } })
}

async fn write_message<W>(output: &mut W, message: &Value) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut line = serde_json::to_string(message)?;
    line.push('\n');
    output.write_all(line.as_bytes()).await?;
    output.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tool_registry::{ToolExecutor, ToolOutcome, ToolRegistry, ToolSpec};

    struct EchoUpper;

    #[async_trait::async_trait]
    impl ToolExecutor for EchoUpper {
        async fn execute(&self, args: Value) -> Result<ToolOutcome> {
            let text = args
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing 'text'"))?;
            Ok(ToolOutcome::ok(text.to_uppercase()))
        }
    }

    fn registry_with_echo() -> Arc<dyn ToolRegistryHandle> {
        let reg = ToolRegistry::new();
        reg.register_tool(
            ToolSpec::new(
                "echo_upper",
                "Uppercase the input text.",
                json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "required": ["text"],
                }),
            ),
            Arc::new(EchoUpper),
        )
        .unwrap();
        Arc::new(reg)
    }

    async fn roundtrip(requests: &[Value]) -> Vec<Value> {
        roundtrip_with(registry_with_echo(), Redactor::default(), requests).await
    }

    async fn roundtrip_with(
        registry: Arc<dyn ToolRegistryHandle>,
        redactor: Redactor,
        requests: &[Value],
    ) -> Vec<Value> {
        let mut input = String::new();
        for req in requests {
            input.push_str(&serde_json::to_string(req).unwrap());
            input.push('\n');
        }
        let mut output: Vec<u8> = Vec::new();
        serve_stdio(registry, redactor, input.as_bytes(), &mut output)
            .await
            .unwrap();
        String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn initialize_advertises_tools_capability() {
        let out = roundtrip(&[json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" })]).await;
        assert_eq!(out[0]["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert!(out[0]["result"]["capabilities"]["tools"].is_object());
    }

    #[tokio::test]
    async fn tools_list_returns_registered_specs() {
        let out = roundtrip(&[json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })]).await;
        let tools = out[0]["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "echo_upper");
        assert_eq!(
            tools[0]["inputSchema"]["properties"]["text"]["type"],
            "string"
        );
    }

    #[tokio::test]
    async fn tools_call_executes_in_host_and_returns_result() {
        let out = roundtrip(&[json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "echo_upper", "arguments": { "text": "hello from spike" } }
        })])
        .await;
        assert_eq!(out[0]["result"]["isError"], false);
        assert_eq!(out[0]["result"]["content"][0]["text"], "HELLO FROM SPIKE");
    }

    #[tokio::test]
    async fn tools_call_unknown_is_structured_error_not_transport_error() {
        let out = roundtrip(&[json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": { "name": "nope", "arguments": {} }
        })])
        .await;
        // Structured failure: a result with isError, NOT a JSON-RPC error.
        assert!(out[0].get("error").is_none());
        assert_eq!(out[0]["result"]["isError"], true);
        assert!(out[0]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown tool"));
        assert_eq!(
            out[0]["result"]["structuredContent"]["error"]["code"],
            "unknown_tool"
        );
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let out = roundtrip(&[json!({ "jsonrpc": "2.0", "id": 5, "method": "frobnicate" })]).await;
        assert_eq!(out[0]["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn notifications_get_no_response() {
        // A message with no id (notification) must not produce output.
        let registry = registry_with_echo();
        let input = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string();
        let mut output: Vec<u8> = Vec::new();
        serve_stdio(
            registry,
            Redactor::default(),
            format!("{input}\n").as_bytes(),
            &mut output,
        )
        .await
        .unwrap();
        assert!(output.is_empty());
    }

    #[tokio::test]
    async fn tools_list_surfaces_read_write_metadata() {
        // echo_upper carries no access flags -> readOnlyHint false, destructive false.
        let out = roundtrip(&[json!({ "jsonrpc": "2.0", "id": 6, "method": "tools/list" })]).await;
        let tool = &out[0]["result"]["tools"][0];
        assert!(tool["annotations"].is_object());
        assert_eq!(tool["annotations"]["readOnlyHint"], false);
        assert_eq!(tool["annotations"]["destructiveHint"], false);
    }

    struct LeaksSecret(String);

    #[async_trait::async_trait]
    impl ToolExecutor for LeaksSecret {
        async fn execute(&self, _args: Value) -> Result<ToolOutcome> {
            Ok(ToolOutcome::ok(format!("used token {} ok", self.0)))
        }
    }

    struct LeaksCodedSecret(String);

    #[async_trait::async_trait]
    impl ToolExecutor for LeaksCodedSecret {
        async fn execute(&self, _args: Value) -> Result<ToolOutcome> {
            Ok(ToolOutcome::error_code(
                "leaky_error",
                format!("failed with {}", self.0),
                Some(format!("retry without {}", self.0)),
            ))
        }
    }

    fn leaky_registry(name: &str, executor: Arc<dyn ToolExecutor>) -> Arc<dyn ToolRegistryHandle> {
        let reg = ToolRegistry::new();
        reg.register_tool(
            ToolSpec::new(name, "desc", json!({ "type": "object" })).writes(),
            executor,
        )
        .unwrap();
        Arc::new(reg)
    }

    #[tokio::test]
    async fn observed_dispatch_redacts_secret_in_args_and_result() {
        let secret = "shhh-bridge-secret-value-123".to_string();
        let reg = ToolRegistry::new();
        reg.register_tool(
            ToolSpec::new("leaky", "desc", json!({ "type": "object" })).writes(),
            Arc::new(LeaksSecret(secret.clone())),
        )
        .unwrap();
        let registry: Arc<dyn ToolRegistryHandle> = Arc::new(reg);
        let redactor = Redactor::from_secret_values([secret.clone()]);

        let (outcome, observation) = registry
            .dispatch_observed("leaky", json!({ "token": secret.clone() }), &redactor)
            .await;

        // Agent still sees the real result; observation is a side channel.
        assert!(outcome.text.contains(&secret));
        // The log-facing summary never contains the secret.
        assert!(!observation.args_summary.contains(&secret));
        assert!(!observation.result_summary.contains(&secret));
        // Required fields present.
        let line = observation.log_line();
        assert!(line.contains("tool=leaky"));
        assert!(line.contains("status=ok"));
        assert!(line.contains("duration_ms="));
        assert!(line.contains("access=write"));
    }

    #[tokio::test]
    async fn tools_call_redacts_secret_in_agent_facing_success_result() {
        let secret = "shhh-agent-result-secret-123".to_string();
        let registry = leaky_registry("leaky", Arc::new(LeaksSecret(secret.clone())));
        let out = roundtrip_with(
            registry,
            Redactor::from_secret_values([secret.clone()]),
            &[json!({
                "jsonrpc": "2.0",
                "id": 10,
                "method": "tools/call",
                "params": { "name": "leaky", "arguments": {} }
            })],
        )
        .await;

        let text = out[0]["result"]["content"][0]["text"].as_str().unwrap();
        assert_eq!(out[0]["result"]["isError"], false);
        assert!(!text.contains(&secret));
        assert!(text.contains("[REDACTED]"));
    }

    #[tokio::test]
    async fn tools_call_redacts_secret_in_agent_facing_structured_error() {
        let secret = "shhh-agent-error-secret-123".to_string();
        let registry = leaky_registry("leaky_error", Arc::new(LeaksCodedSecret(secret.clone())));
        let out = roundtrip_with(
            registry,
            Redactor::from_secret_values([secret.clone()]),
            &[json!({
                "jsonrpc": "2.0",
                "id": 11,
                "method": "tools/call",
                "params": { "name": "leaky_error", "arguments": {} }
            })],
        )
        .await;

        let result = &out[0]["result"];
        assert_eq!(result["isError"], true);
        assert_eq!(result["structuredContent"]["error"]["code"], "leaky_error");
        for value in [
            &result["content"][0]["text"],
            &result["structuredContent"]["error"]["message"],
            &result["structuredContent"]["error"]["hint"],
        ] {
            let text = value.as_str().unwrap();
            assert!(!text.contains(&secret));
            assert!(text.contains("[REDACTED]"));
        }
    }

    #[tokio::test]
    async fn malformed_json_yields_parse_error_and_continues() {
        let registry = registry_with_echo();
        let input = format!(
            "not json\n{}\n",
            json!({ "jsonrpc": "2.0", "id": 9, "method": "tools/list" })
        );
        let mut output: Vec<u8> = Vec::new();
        serve_stdio(registry, Redactor::default(), input.as_bytes(), &mut output)
            .await
            .unwrap();
        let lines: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines[0]["error"]["code"], -32700);
        // The bridge kept serving after the bad line.
        assert_eq!(lines[1]["result"]["tools"][0]["name"], "echo_upper");
    }
}
