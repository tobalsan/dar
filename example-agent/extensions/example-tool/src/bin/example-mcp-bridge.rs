//! Standalone host MCP bridge over the `example-tool` registry, used by the
//! codex `app-server` end-to-end integration test (and ad-hoc manual probing).
//!
//! It builds a `ToolRegistry`, registers the toy `echo_upper` tool exactly as
//! the `example-tool` extension does, and serves the MCP stdio protocol — i.e.
//! the same registry + bridge surface the host owns, exercised without the full
//! per-agent composition. Not shipped in `dist`.
//!
//! Accepts an optional `--suffix <s>` that stands in for the
//! `extensions.example-tool.suffix` config the host bridge resolves from
//! `agent.yaml`. The e2e passes a known suffix and asserts it appears in the
//! tool result, so a regression that drops extension config on the bridge path
//! fails the test instead of silently passing.

use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tool_registry::{ToolRegistry, ToolRegistryHandle};

#[tokio::main]
async fn main() -> Result<()> {
    let suffix = parse_suffix(std::env::args().skip(1));
    let registry = ToolRegistry::new();
    example_tool::register_into_with_config(
        &registry,
        example_tool::ExampleToolConfig { suffix },
    )?;
    let registry: Arc<dyn ToolRegistryHandle> = Arc::new(registry);

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        let method = request.get("method").and_then(Value::as_str);
        let response = match method {
            Some("initialize") => json!({
                "jsonrpc": "2.0", "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "serverInfo": { "name": "agentropy-example", "version": "0.1.0" },
                    "capabilities": { "tools": {} }
                }
            }),
            Some("tools/list") => {
                let tools: Vec<Value> =
                    registry.list().iter().map(|s| s.to_mcp_tool()).collect();
                json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tools } })
            }
            Some("tools/call") => {
                let params = request.get("params");
                let name = params
                    .and_then(|p| p.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let args = params
                    .and_then(|p| p.get("arguments"))
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let outcome = registry.dispatch(name, args).await;
                json!({ "jsonrpc": "2.0", "id": id, "result": outcome.to_mcp_result() })
            }
            Some("ping") => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            _ => json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -32601, "message": "method not found" }
            }),
        };
        let mut out = serde_json::to_string(&response)?;
        out.push('\n');
        stdout.write_all(out.as_bytes()).await?;
        stdout.flush().await?;
    }
    Ok(())
}

/// Parse `--suffix <value>` (or `--suffix=<value>`) from the argument list,
/// standing in for the resolved `extensions.example-tool.suffix` config.
fn parse_suffix(args: impl Iterator<Item = String>) -> String {
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        if arg == "--suffix" {
            return args.next().unwrap_or_default();
        }
        if let Some(value) = arg.strip_prefix("--suffix=") {
            return value.to_string();
        }
    }
    String::new()
}
