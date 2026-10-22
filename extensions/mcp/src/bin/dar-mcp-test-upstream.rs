//! Test-only stdio MCP server used by `extensions/mcp` integration tests.
//!
//! Tools: `echo` (returns `text` plus `$ECHO_SECRET`, if set) and `bad` (an
//! invalid input schema the host registry must reject). With `STALL=1` it reads
//! requests but never answers.

use std::io::{BufRead, Write};

use serde_json::{json, Value};

fn main() {
    let stall = std::env::var("STALL").as_deref() == Ok("1");
    let apps = std::env::var("APPS").is_ok();
    let blob = std::env::var("APPS").as_deref() == Ok("blob");
    let mut ui_advertised = false;
    let mut ui_reads = 0;
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
        ui_advertised |= request
            .pointer("/params/capabilities/extensions/io.modelcontextprotocol~1ui/mimeTypes")
            == Some(&json!(["text/html;profile=mcp-app"]));
        if apps
            && request["method"] == "tools/call"
            && request["params"]["arguments"]["text"] == "rpc-error"
        {
            writeln!(stdout, "{}", json!({"jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":"upstream draw failed"}})).unwrap();
            stdout.flush().unwrap();
            continue;
        }
        if request["method"] == "resources/read" {
            ui_reads += 1;
        }
        let result = match request["method"].as_str() {
            Some("tools/list") if apps && ui_advertised && blob => json!({"tools":[
                {"name":"blob","inputSchema":{"type":"object"},"_meta":{"ui":{"resourceUri":"ui://canvas/blob"}}},
                {"name":"invalid-utf8","inputSchema":{"type":"object"},"_meta":{"ui":{"resourceUri":"ui://canvas/invalid-utf8"}}},
                {"name":"invalid-base64","inputSchema":{"type":"object"},"_meta":{"ui":{"resourceUri":"ui://canvas/invalid-base64"}}}
            ]}),
            Some("resources/read") if apps && blob => json!({"contents":[{
                "uri":request["params"]["uri"],"mimeType":"text/html;profile=mcp-app",
                "blob":match request["params"]["uri"].as_str() {
                    Some("ui://canvas/invalid-utf8") => "/w==",
                    Some("ui://canvas/invalid-base64") => "not base64!",
                    _ => "PGgxPkJsb2I8L2gxPg=="
                },"_meta":{"ui":{"prefersBorder":true}}
            }]}),
            Some("tools/list") if apps && ui_advertised => json!({"tools":[
                {"name":"draw","inputSchema":{"type":"object"},"_meta":{"ui":{"resourceUri":"ui://canvas/app","visibility":["model","app"]}}},
                {"name":"refresh","inputSchema":{"type":"object"},"_meta":{"ui":{"visibility":["app"]}}},
                {"name":"model-only","inputSchema":{"type":"object"},"_meta":{"ui":{"visibility":["model"]}}},
                {"name":"legacy","inputSchema":{"type":"object"},"_meta":{"ui/resourceUri":"ui://canvas/app"}}
            ]}),
            Some("resources/read") if apps => {
                json!({"contents":[{"uri":"ui://canvas/app","mimeType":"text/html;profile=mcp-app","text":"<html><body>Canvas</body></html>","_meta":{"ui":{"csp":{"connectDomains":["https://canvas.example"]}},"readCount":ui_reads}}]})
            }
            Some("tools/call") if apps => {
                json!({"content":[{"type":"text","text":request["params"]["arguments"]["text"]}],"structuredContent":{"revision":7},"_meta":{"private":"retained"}})
            }
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
