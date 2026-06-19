//! End-to-end integration test for the codex `app-server` + host MCP bridge path.
//!
//! Drives a real `codex app-server` (JSON-RPC 2.0 over stdio) wired to the
//! host-owned `example-mcp-bridge` via `-c mcp_servers.agentropy.command=...` —
//! the exact transport `runner-codex` uses (NOT `codex exec`). It asks the agent
//! to call the toy `echo_upper` tool and asserts the tool call routes back to
//! the bridge, executes in-host, and returns `HELLO FROM SPIKE` inside the same
//! thread/turn.
//!
//! Gated: skips (passes) unless `codex` is installed and authed and
//! `AGENTROPY_CODEX_E2E=1` is set, so CI without a codex login is unaffected.
//!
//! Run it explicitly with:
//!   AGENTROPY_CODEX_E2E=1 cargo test -p example-tool --test codex_app_server_e2e -- --nocapture

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

fn codex_available() -> bool {
    std::env::var("AGENTROPY_CODEX_E2E").as_deref() == Ok("1")
        && Command::new("codex")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
}

#[test]
fn codex_app_server_calls_echo_upper_through_host_bridge() {
    if !codex_available() {
        eprintln!("skipping codex e2e: set AGENTROPY_CODEX_E2E=1 with codex installed + authed");
        return;
    }

    let bridge = env!("CARGO_BIN_EXE_example-mcp-bridge");
    let workspace = tempfile::tempdir().unwrap();

    let mut child = Command::new("codex")
        .arg("app-server")
        .args(["-c", "approval_policy=\"never\""])
        .args(["-c", "sandbox_permissions=[\"disk-full-read-access\"]"])
        .args(["-c", &format!("mcp_servers.agentropy.command={bridge:?}")])
        .current_dir(workspace.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn codex app-server");

    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    let mut send = |value: Value| {
        let line = value.to_string() + "\n";
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.flush().unwrap();
    };

    send(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "clientInfo": { "name": "agentropy-test", "version": "0.1.0" },
            "capabilities": { "experimentalApi": true },
            "cwd": workspace.path(),
        }
    }));
    send(json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }));
    send(json!({
        "jsonrpc": "2.0", "id": 2, "method": "thread/start",
        "params": {
            "cwd": workspace.path(),
            "approvalPolicy": "never",
            "sandbox": "danger-full-access",
        }
    }));

    // codex app-server returns the id nested as `result.thread.id` (the same
    // shape the production runner extracts in extract_thread_id).
    let thread_id = read_until(&mut reader, Duration::from_secs(60), |msg| {
        (msg.get("id") == Some(&json!(2)))
            .then(|| msg["result"]["thread"]["id"].as_str().map(str::to_string))
            .flatten()
    })
    .expect("thread/start returned a thread id");

    send(json!({
        "jsonrpc": "2.0", "id": 3, "method": "turn/start",
        "params": {
            "threadId": thread_id,
            "input": [{
                "type": "text",
                "text": "Call the echo_upper tool with {\"text\":\"hello from spike\"} \
                         and then reply with exactly the tool's output and nothing else."
            }],
            "cwd": workspace.path(),
            "approvalPolicy": "never",
            "sandboxPolicy": { "type": "dangerFullAccess" },
        }
    }));

    // The tool result returns inside the same thread; look for HELLO FROM SPIKE
    // anywhere in the streamed items/turn output.
    let found = read_until(&mut reader, Duration::from_secs(180), |msg| {
        msg.to_string()
            .contains("HELLO FROM SPIKE")
            .then_some(true)
    })
    .unwrap_or(false);

    let _ = child.kill();
    assert!(
        found,
        "expected the echo_upper tool result HELLO FROM SPIKE to return in-session"
    );
}

/// Read JSON-RPC lines until `pick` yields a value or the deadline passes.
fn read_until<R: BufRead, T>(
    reader: &mut R,
    timeout: Duration,
    mut pick: impl FnMut(&Value) -> Option<T>,
) -> Option<T> {
    let deadline = Instant::now() + timeout;
    let mut line = String::new();
    while Instant::now() < deadline {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return None,
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(msg) = serde_json::from_str::<Value>(trimmed) {
                    if let Some(value) = pick(&msg) {
                        return Some(value);
                    }
                }
            }
            Err(_) => return None,
        }
    }
    None
}
