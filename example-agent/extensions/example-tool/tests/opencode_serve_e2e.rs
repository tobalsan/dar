//! End-to-end integration test for the `opencode serve` + host MCP bridge path.
//!
//! Drives a real `opencode serve` child (the exact HTTP/SSE protocol
//! `runner-opencode` drives) wired to the host-owned `example-mcp-bridge` via an
//! `mcp.agentropy` block in `OPENCODE_CONFIG_CONTENT` — the same
//! `{ "mcp": { "agentropy": { "type": "local", "command": [...] } } }` document
//! the production runner writes. opencode surfaces the host tools namespaced
//! `agentropy_<tool>` and routes the call back to the bridge over stdio.
//!
//! Two paths are exercised, both in the SAME session to prove session survival:
//!   1. Happy path — ask the agent to call `echo_upper` with valid input and
//!      assert the in-host result `HELLO FROM SPIKE [via-config]` returns over
//!      SSE inside the session (the `message.part.updated` tool part completes).
//!   2. Failure path — ask the agent to call `echo_upper` with a malformed
//!      argument (no `text`). The bridge returns a structured `isError` result
//!      (NOT a transport fault); the session does not stall and stays usable for
//!      a follow-up turn that completes normally.
//!
//! The non-empty `--suffix` passed to the bridge stands in for resolved
//! `extensions.example-tool.suffix` config: it only appears in the result if the
//! bridge threaded extension config into the tool's register() pass, guarding
//! the config-parity contract on the opencode path too.
//!
//! Gated: skips (passes) unless `opencode` is installed and authed and
//! `AGENTROPY_OPENCODE_E2E=1` is set, so CI without an opencode login is
//! unaffected.
//!
//! Run it explicitly with:
//!   AGENTROPY_OPENCODE_E2E=1 cargo test -p example-tool --test opencode_serve_e2e -- --nocapture

use std::ffi::OsString;
use std::process::Command;
use std::time::Duration;

use opencode_client::{OpenCodeEvent, OpenCodeServer};
use serde_json::{json, Value};

/// Matches `runner-opencode`'s `BRIDGE_SERVER_NAME`.
const BRIDGE_SERVER_NAME: &str = "agentropy";
const SUFFIX: &str = " [via-config]";

fn opencode_available() -> bool {
    std::env::var("AGENTROPY_OPENCODE_E2E").as_deref() == Ok("1")
        && Command::new("opencode")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
}

/// The exact `mcp.agentropy` block `runner-opencode` writes: advertise the host
/// bridge as a `type: "local"` stdio MCP server whose `command` is the bridge
/// binary followed by its args. opencode surfaces its tools `agentropy_<tool>`.
fn config_content(bridge: &str) -> String {
    json!({
        "$schema": "https://opencode.ai/config.json",
        "permission": { "*": "allow" },
        "mcp": {
            BRIDGE_SERVER_NAME: {
                "type": "local",
                "command": [bridge, "--suffix", SUFFIX],
                "enabled": true,
            }
        },
    })
    .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn opencode_serve_calls_echo_upper_through_host_bridge_and_survives_failure() {
    if !opencode_available() {
        eprintln!(
            "skipping opencode e2e: set AGENTROPY_OPENCODE_E2E=1 with opencode installed + authed"
        );
        return;
    }

    let bridge = env!("CARGO_BIN_EXE_example-mcp-bridge");
    let workspace = tempfile::tempdir().unwrap();
    let session_dir = workspace.path().join("sess");
    let config_dir = session_dir.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();

    let content = config_content(bridge);
    let config_path = config_dir.join("opencode.json");
    std::fs::write(&config_path, &content).unwrap();

    let env: Vec<(OsString, OsString)> = vec![
        (OsString::from("OPENCODE_CONFIG"), config_path.clone().into_os_string()),
        (OsString::from("OPENCODE_CONFIG_DIR"), config_dir.clone().into_os_string()),
        (OsString::from("OPENCODE_CONFIG_CONTENT"), OsString::from(content)),
        (OsString::from("XDG_DATA_HOME"), session_dir.join("data").into_os_string()),
        (OsString::from("XDG_STATE_HOME"), session_dir.join("state").into_os_string()),
        (OsString::from("XDG_CACHE_HOME"), session_dir.join("cache").into_os_string()),
    ];

    let args = vec![
        OsString::from("serve"),
        OsString::from("--hostname"),
        OsString::from("127.0.0.1"),
        OsString::from("--port"),
        OsString::from("0"),
    ];

    let mut server = OpenCodeServer::spawn(OsString::from("opencode"), args, env, workspace.path())
        .await
        .expect("spawn opencode serve");
    let client = server.client();
    let mut events = client.events().await.expect("open opencode event stream");
    let session_id = client.create_session("ALG-258-e2e").await.expect("create session");

    // ---- Happy path: valid echo_upper call returns in-session over SSE -------
    let expected = format!("HELLO FROM SPIKE{SUFFIX}");
    client
        .send_prompt(
            &session_id,
            "Call the agentropy_echo_upper host tool with {\"text\":\"hello from spike\"}. \
             Then reply with exactly the tool's output text and nothing else.",
            None,
        )
        .await
        .expect("send happy-path prompt");

    let happy = wait_for(&mut events, &session_id, Duration::from_secs(240), |payload| {
        // The configured in-host result must appear somewhere in the session
        // stream (tool output part and/or the assistant echo).
        payload.to_string().contains(&expected)
    })
    .await;
    assert!(
        happy,
        "expected the configured echo_upper result {expected:?} to return in-session over SSE"
    );

    // ---- Failure path: malformed call -> structured isError, no stall --------
    client
        .send_prompt(
            &session_id,
            "Call the agentropy_echo_upper host tool with NO arguments at all (an empty \
             object {}). Report back whether the tool returned an error and the error text.",
            None,
        )
        .await
        .expect("send malformed-call prompt");

    // The bridge surfaces a structured failure (the executor's
    // \"requires a 'text' string argument\"), classified by opencode as a tool
    // part in the `error` state — NOT a transport fault. The session reaching
    // idle again proves it did not stall.
    let failed_in_session = wait_for_idle_or(
        &mut events,
        &session_id,
        Duration::from_secs(240),
        |payload| tool_part_errored(payload) || payload.to_string().contains("requires a 'text'"),
    )
    .await;
    assert!(
        failed_in_session,
        "expected the malformed echo_upper call to surface a structured failure in-session"
    );

    // ---- Survival: a follow-up turn still completes normally -----------------
    let survive_expected = "STILL ALIVE [via-config]";
    client
        .send_prompt(
            &session_id,
            "Now call agentropy_echo_upper with {\"text\":\"still alive\"} and reply with \
             exactly the tool's output text and nothing else.",
            None,
        )
        .await
        .expect("send survival prompt");
    let survived = wait_for(&mut events, &session_id, Duration::from_secs(240), |payload| {
        payload.to_string().contains(survive_expected)
    })
    .await;
    assert!(
        survived,
        "session stalled after the malformed call: follow-up turn did not produce \
         {survive_expected:?}"
    );

    server.kill_and_wait(Duration::from_secs(5)).await;
}

/// Drain SSE events until `pred(payload)` holds or the deadline passes.
async fn wait_for(
    events: &mut opencode_client::EventStream,
    _session_id: &str,
    timeout: Duration,
    mut pred: impl FnMut(&Value) -> bool,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match tokio::time::timeout(remaining, events.next_event()).await {
            Ok(Ok(Some(event))) => {
                if let Some(payload) = event_payload(&event) {
                    if pred(&payload) {
                        return true;
                    }
                }
            }
            Ok(Ok(None)) | Ok(Err(_)) | Err(_) => return false,
        }
    }
}

/// Like `wait_for`, but also returns true once the session reaches `idle`
/// AFTER `pred` has matched at least once — proving the failure surfaced AND
/// the turn completed without stalling.
async fn wait_for_idle_or(
    events: &mut opencode_client::EventStream,
    session_id: &str,
    timeout: Duration,
    mut pred: impl FnMut(&Value) -> bool,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut matched = false;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return matched;
        }
        match tokio::time::timeout(remaining, events.next_event()).await {
            Ok(Ok(Some(event))) => {
                if let Some(payload) = event_payload(&event) {
                    if pred(&payload) {
                        matched = true;
                    }
                    if matched && is_session_idle(&payload, session_id) {
                        return true;
                    }
                }
            }
            Ok(Ok(None)) | Ok(Err(_)) | Err(_) => return matched,
        }
    }
}

fn is_session_idle(payload: &Value, session_id: &str) -> bool {
    payload.get("type").and_then(Value::as_str) == Some("session.idle")
        && payload
            .get("properties")
            .and_then(|p| p.get("sessionID"))
            .and_then(Value::as_str)
            == Some(session_id)
}

/// A tool part finalized in the `error` state — the structured-failure signal.
fn tool_part_errored(payload: &Value) -> bool {
    let part = payload
        .get("properties")
        .and_then(|p| p.get("part"));
    let is_tool = part.and_then(|p| p.get("type")).and_then(Value::as_str) == Some("tool");
    let status = part
        .and_then(|p| p.get("state"))
        .and_then(|s| s.get("status"))
        .and_then(Value::as_str);
    is_tool && status == Some("error")
}

/// Mirror `runner-opencode`'s payload unwrapping (`payload` envelope or raw).
fn event_payload(event: &OpenCodeEvent) -> Option<Value> {
    let value = serde_json::from_str::<Value>(&event.data).ok()?;
    if let Some(payload) = value.get("payload") {
        return Some(payload.clone());
    }
    Some(value)
}
