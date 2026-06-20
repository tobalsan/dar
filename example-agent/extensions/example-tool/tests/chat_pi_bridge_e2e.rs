//! End-to-end integration test for the **TUI chat** spawn path + host MCP
//! bridge (ALG-259).
//!
//! Unlike `pi_rpc_e2e` (which drives a raw `pi --mode rpc` child), this test
//! exercises the real `chat-pi` `PiChatBackend` — the exact code the TUI
//! foreground uses — with `ChatSessionParams.host_tool_bridge` set, proving the
//! chat spawn path advertises the same host registry tools an issue worker sees.
//! It then asserts the toy `echo_upper` result surfaces as a
//! `ChatEvent::ToolOutput` in the same session and that the turn completes with
//! `ChatEvent::TurnFinished { ok: true }` (the `send_turn` loop continues from
//! the tool result).
//!
//! The non-empty `--suffix` passed to the bridge stands in for resolved
//! `extensions.example-tool.suffix` config: it only appears in the result if the
//! bridge threaded extension config into the tool's register() pass, so a green
//! run guards config parity on the chat path too.
//!
//! Gated: skips (passes) unless `pi` is installed (with the MCP adapter that
//! registers `--mcp-config`) and `AGENTROPY_PI_E2E=1` is set, so CI without a pi
//! login is unaffected.
//!
//! Run it explicitly with:
//!   AGENTROPY_PI_E2E=1 cargo test -p example-tool --test chat_pi_bridge_e2e -- --nocapture

use std::process::{Command, Stdio};
use std::time::Duration;

use cap_chat::{ChatBackend, ChatEvent, ChatSessionParams, HostToolBridge};
use chat_pi::PiChatBackend;

fn pi_available() -> bool {
    std::env::var("AGENTROPY_PI_E2E").as_deref() == Ok("1")
        && Command::new("pi")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_pi_calls_echo_upper_through_host_bridge() {
    if !pi_available() {
        eprintln!("skipping chat-pi e2e: set AGENTROPY_PI_E2E=1 with pi installed + authed");
        return;
    }

    let bridge_bin = env!("CARGO_BIN_EXE_example-mcp-bridge");
    let workspace = tempfile::tempdir().unwrap();
    let session_dir = workspace.path().join("sess");
    std::fs::create_dir_all(&session_dir).unwrap();

    const SUFFIX: &str = " [via-config]";
    let expected = format!("HELLO FROM SPIKE{SUFFIX}");

    // The same `HostToolBridge` descriptor the TUI foreground builds for chat:
    // `<bridge> --suffix <suffix>`. chat-pi's spawn writes the `--mcp-config`
    // document and `MCP_DIRECT_TOOLS` env via the shared runner-core writer.
    let bridge = HostToolBridge {
        command: bridge_bin.to_string(),
        args: vec!["--suffix".to_string(), SUFFIX.to_string()],
    };
    let params = ChatSessionParams::builder("pi", workspace.path(), &session_dir)
        .host_tool_bridge(Some(bridge))
        .build();
    if let Ok(provider) = std::env::var("AGENTROPY_PI_E2E_PROVIDER") {
        // Forward an optional provider override for environments with a specific
        // pi login.
        std::env::set_var("PI_PROVIDER", provider);
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel::<ChatEvent>(256);
    let backend = PiChatBackend;
    let mut session = backend
        .open(params, tx)
        .await
        .expect("open chat-pi session");

    session
        .send_turn(
            "Use the echo_upper host tool (call it directly, or via the mcp gateway \
             tool) with text \"hello from spike\". Then reply with exactly the tool's \
             output text and nothing else."
                .to_string(),
        )
        .await
        .expect("send chat turn");

    // Collect events until the turn finishes (or timeout). We want to see the
    // toy tool result arrive as ChatEvent::ToolOutput AND the turn complete
    // cleanly, proving the loop continues from the tool result.
    let mut got_tool_output = false;
    let mut turn_ok: Option<bool> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(ChatEvent::ToolOutput {
                text, is_error, ..
            })) => {
                if text.contains(&expected) && !is_error {
                    got_tool_output = true;
                }
            }
            Ok(Some(ChatEvent::TurnFinished { ok, error })) => {
                turn_ok = Some(ok);
                if let Some(err) = error {
                    eprintln!("turn finished with error: {err}");
                }
                break;
            }
            Ok(Some(ChatEvent::SessionClosed { error })) => {
                panic!("chat session closed unexpectedly: {error:?}");
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => break,
        }
    }

    // Close the session cleanly (drops stdin -> EOF, term/kill on overrun).
    let _ = session.close().await;

    assert!(
        got_tool_output,
        "expected the configured echo_upper result {expected:?} to surface as a \
         ChatEvent::ToolOutput in the chat session"
    );
    assert_eq!(
        turn_ok,
        Some(true),
        "expected the chat turn to finish ok=true after acting on the tool result"
    );
}
