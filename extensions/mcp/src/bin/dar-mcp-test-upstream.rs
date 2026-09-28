//! Test-only stdio MCP server used by `extensions/mcp` integration tests.
//!
//! Tools: `echo` (returns `text` plus `$ECHO_SECRET`, if set) and `bad` (an
//! invalid input schema the host registry must reject). With `STALL=1` it reads
//! requests but never answers.

use std::io::{BufRead, Write};

use serde_json::{json, Value};

fn main() {
    let stall = std::env::var("STALL").as_deref() == Ok("1");
    let secret = std::env::var("ECHO_SECRET").unwrap_or_default();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if stall {
            continue;
        }
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = request.get("id").cloned() else {
            continue; // notification
        };
        let result = match request["method"].as_str() {
            Some("initialize") => json!({
                "protocolVersion": request["params"]["protocolVersion"],
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "test-upstream", "version": "0" }
            }),
            Some("tools/list") => json!({ "tools": [
                { "name": "echo", "description": "echo text",
                  "inputSchema": { "type": "object", "properties": { "text": { "type": "string" } } } },
                { "name": "bad", "description": "invalid schema",
                  "inputSchema": { "type": "object", "properties": { "x": { "type": 7 } } } }
            ]}),
            Some("tools/call") => {
                let text = request["params"]["arguments"]["text"]
                    .as_str()
                    .unwrap_or("");
                json!({ "content": [{ "type": "text", "text": format!("{text} {secret}").trim() }] })
            }
            _ => {
                let error = json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "unknown method" } });
                writeln!(stdout, "{error}").unwrap();
                stdout.flush().unwrap();
                continue;
            }
        };
        writeln!(
            stdout,
            "{}",
            json!({ "jsonrpc": "2.0", "id": id, "result": result })
        )
        .unwrap();
        stdout.flush().unwrap();
    }
}
