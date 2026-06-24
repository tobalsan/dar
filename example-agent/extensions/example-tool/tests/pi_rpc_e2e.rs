//! End-to-end integration test for the `pi --mode rpc` + host MCP bridge path.
//!
//! Drives a real `pi --mode rpc` child (the exact protocol `runner-pi` drives)
//! wired to the host-owned `example-mcp-bridge` via `--mcp-config <file>` — the
//! same `{ "mcpServers": { dar: { command, args } } }` document the
//! production runner writes. It asks the agent to call the toy `echo_upper` tool
//! and asserts the call routes back to the bridge, executes in-host, and returns
//! `HELLO FROM SPIKE [via-config]` inside the same `--mode rpc` session on the
//! `tool_execution_end` event.
//!
//! The non-empty `--suffix` passed to the bridge stands in for resolved
//! `extensions.example-tool.suffix` config: it only appears in the result if the
//! bridge threaded extension config into the tool's register() pass, guarding
//! the config-parity contract on the pi path too.
//!
//! Gated: skips (passes) unless `pi` is installed (with the MCP adapter that
//! registers `--mcp-config`) and `DAR_PI_E2E=1` is set, so CI without a pi
//! login is unaffected.
//!
//! Run it explicitly with:
//!   DAR_PI_E2E=1 cargo test -p example-tool --test pi_rpc_e2e -- --nocapture

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// Matches `runner-pi`'s `BRIDGE_SERVER_NAME`.
const BRIDGE_SERVER_NAME: &str = "dar";

fn pi_available() -> bool {
    std::env::var("DAR_PI_E2E").as_deref() == Ok("1")
        && Command::new("pi")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
}

#[test]
fn pi_rpc_calls_echo_upper_through_host_bridge() {
    if !pi_available() {
        eprintln!("skipping pi e2e: set DAR_PI_E2E=1 with pi installed + authed");
        return;
    }

    let bridge = env!("CARGO_BIN_EXE_example-mcp-bridge");
    let workspace = tempfile::tempdir().unwrap();
    let session_dir = workspace.path().join("sess");
    std::fs::create_dir_all(&session_dir).unwrap();

    const SUFFIX: &str = " [via-config]";
    let expected = format!("HELLO FROM SPIKE{SUFFIX}");

    // The exact `--mcp-config` document runner-pi writes: advertise the host
    // bridge as an eager stdio MCP server, names unprefixed, direct tools on,
    // proxy floor retained.
    let mcp_config = json!({
        "mcpServers": {
            BRIDGE_SERVER_NAME: {
                "command": bridge,
                "args": ["--suffix", SUFFIX],
                "lifecycle": "eager",
            }
        },
        "settings": {
            "toolPrefix": "none",
            "directTools": true,
            "disableProxyTool": false,
        }
    });
    let config_path = workspace.path().join("mcp-config.json");
    std::fs::write(&config_path, format!("{mcp_config:#}\n")).unwrap();

    let mut cmd = Command::new("pi");
    cmd.arg("--mode")
        .arg("rpc")
        .arg("--session-dir")
        .arg(&session_dir)
        .arg("--mcp-config")
        .arg(&config_path)
        // Scope direct-tool promotion to our server (mirrors runner-pi).
        .env("MCP_DIRECT_TOOLS", BRIDGE_SERVER_NAME)
        .current_dir(workspace.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Optional provider override for environments with a specific pi login.
    if let Ok(provider) = std::env::var("DAR_PI_E2E_PROVIDER") {
        cmd.arg("--provider").arg(provider);
    }

    let mut child = cmd.spawn().expect("spawn pi --mode rpc");
    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    // One `{"type":"prompt"}` JSONL line — exactly what runner-pi feeds.
    let prompt = json!({
        "id": "t1",
        "type": "prompt",
        "message": "Use the echo_upper host tool (call it directly, or via the mcp \
                    gateway tool) with text \"hello from spike\". Then reply with \
                    exactly the tool's output text and nothing else."
    });
    stdin
        .write_all((prompt.to_string() + "\n").as_bytes())
        .unwrap();
    stdin.flush().unwrap();

    // The tool result returns in-session on `tool_execution_end`. Assert it is
    // the in-host echo_upper result carrying the configured suffix.
    let found = read_until(&mut reader, Duration::from_secs(180), |msg| {
        let is_exec_end = msg.get("type").and_then(Value::as_str) == Some("tool_execution_end");
        if !is_exec_end {
            return None;
        }
        let text = msg
            .get("result")
            .and_then(|r| r.get("content"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|c| c.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("");
        text.contains(&expected).then_some(true)
    })
    .unwrap_or(false);

    // Reap the child so the test does not leave a zombie pi process. Dropping
    // stdin sends EOF (pi's clean-quit signal) before the kill/wait.
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        found,
        "expected the configured echo_upper result {expected:?} to return in-session \
         on tool_execution_end"
    );
}

/// Read JSONL lines until `pick` yields a value or the deadline passes.
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
