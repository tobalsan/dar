//! Real pi provider-payload and chat-event test of mixed host/upstream exposure.
//!
//! Run with DAR_PI_E2E=1 and an authenticated pi installation. Optional
//! DAR_PI_E2E_PROVIDER / DAR_PI_E2E_MODEL override the inexpensive default.
//! The fixture covers declaration, discovery and execution; production bridge
//! initialization and Iris token measurement are outside this seam.

use std::process::{Command, Stdio};
use std::time::Duration;

use cap_chat::{ChatBackend, ChatEvent, ChatSessionParams, HostToolBridge};
use chat_pi::PiChatBackend;
use serde_json::Value;

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_pi_discovers_upstream_without_declaring_it_initially() {
    if !pi_available() {
        eprintln!("skipping chat-pi e2e: set DAR_PI_E2E=1 with pi installed + authed");
        return;
    }

    let workspace = tempfile::tempdir().unwrap();
    let session_dir = workspace.path().join("sess");
    let extensions = workspace.path().join(".pi/extensions");
    std::fs::create_dir_all(&extensions).unwrap();
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(
        workspace.path().join(".pi/settings.json"),
        r#"{"defaultTools":["+codemode"]}"#,
    )
    .unwrap();
    // Observe the first serialized provider payload unchanged. Capture only the
    // fixture request body, never provider headers or authentication material.
    std::fs::write(
        extensions.join("capture.ts"),
        r#"import { writeFileSync } from "node:fs";
import { join } from "node:path";
export default function (pi) {
  let captured = false;
  pi.on("before_provider_request", (event, ctx) => {
    if (!captured) {
      writeFileSync(join(ctx.cwd, "first-provider-request.json"), JSON.stringify(event.payload));
      captured = true;
    }
  });
}
"#,
    )
    .unwrap();

    const EXPECTED: &str = "HELLO FROM SPIKE [via-config]";
    const DIRECT: &str = "mcp__dar__echo_upper";
    const UPSTREAM: &str = "mcp__dar__fixture__echo_upper";
    let bridge = HostToolBridge {
        command: env!("CARGO_BIN_EXE_example-mcp-bridge").to_string(),
        args: vec![
            "--suffix".to_string(),
            " [via-config]".to_string(),
            "--include-upstream".to_string(),
        ],
    };
    let params = ChatSessionParams::builder("pi", workspace.path(), &session_dir)
        .host_tool_bridge(Some(bridge))
        .provider(Some(
            std::env::var("DAR_PI_E2E_PROVIDER").unwrap_or_else(|_| "openai-codex".into()),
        ))
        .model(Some(
            std::env::var("DAR_PI_E2E_MODEL").unwrap_or_else(|_| "gpt-6-luna".into()),
        ))
        .build();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ChatEvent>(256);
    // One deadline includes process startup, provider calls and event delivery.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    let mut session = tokio::time::timeout_at(deadline, PiChatBackend.open(params, tx))
        .await
        .expect("chat-pi open timed out")
        .expect("open chat-pi session");
    tokio::time::timeout_at(
        deadline,
        session.send_turn(
            "First call tool_search to discover fixture__echo_upper. Then directly call \
             the discovered upstream fixture__echo_upper with text \"hello from spike\". \
             Do not use echo_upper or codemode. Reply with exactly the upstream tool's output."
                .to_string(),
        ),
    )
    .await
    .expect("send chat turn timed out")
    .expect("send chat turn");

    let captured = workspace.path().join("first-provider-request.json");
    tokio::time::timeout_at(deadline, async {
        while !captured.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("first provider request was not captured");
    let payload: Value = serde_json::from_slice(&std::fs::read(captured).unwrap()).unwrap();
    let mut declarations: Vec<&Value> = payload["tools"].as_array().into_iter().flatten().collect();
    // Responses providers can declare tools in input items instead of the
    // top-level tools field, including the initial declaration batch.
    for item in payload["input"].as_array().into_iter().flatten() {
        if item["type"].as_str() == Some("additional_tools") {
            declarations.extend(item["tools"].as_array().into_iter().flatten());
        }
    }
    let names: Vec<&str> = declarations
        .into_iter()
        .filter_map(|tool| {
            tool["name"]
                .as_str()
                .or_else(|| tool["function"]["name"].as_str())
        })
        .collect();
    eprintln!("first provider request tool names: {names:?}");
    if !names.contains(&DIRECT) || !names.contains(&"tool_search") || names.contains(&UPSTREAM) {
        let _ = tokio::time::timeout(Duration::from_secs(10), session.close()).await;
        panic!(
            "first request must declare host echo_upper and tool_search, and defer \
             upstream fixture__echo_upper; observed {names:?}"
        );
    }

    let mut search_id = None;
    let mut upstream_id = None;
    let mut search_ok = false;
    let mut upstream_ok = false;
    let mut turn_ok = None;
    while let Ok(Some(event)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        match event {
            ChatEvent::ToolCall { id, name, .. } if name == "tool_search" => {
                eprintln!("chat tool call: tool_search");
                search_id = Some(id);
            }
            ChatEvent::ToolCall { id, name, .. } if name == UPSTREAM => {
                eprintln!("chat tool call: {UPSTREAM}");
                assert!(search_ok, "upstream call must follow successful discovery");
                upstream_id = Some(id);
            }
            ChatEvent::ToolOutput {
                id,
                text,
                is_error: false,
                done: true,
            } => {
                if search_id.as_deref() == Some(id.as_str()) && text.contains("fixture__echo_upper")
                {
                    search_ok = true;
                }
                if upstream_id.as_deref() == Some(id.as_str()) && text.contains(EXPECTED) {
                    upstream_ok = true;
                }
            }
            ChatEvent::TurnFinished { ok, .. } => {
                turn_ok = Some(ok);
                break;
            }
            ChatEvent::SessionClosed { error } => panic!("chat session closed: {error:?}"),
            _ => {}
        }
    }
    tokio::time::timeout(Duration::from_secs(10), session.close())
        .await
        .expect("closing chat-pi session timed out")
        .expect("close chat-pi session");
    eprintln!("chat result: search_ok={search_ok}, upstream_ok={upstream_ok}, turn_ok={turn_ok:?}");
    assert!(search_ok, "tool_search must discover fixture__echo_upper");
    assert!(upstream_ok, "upstream ToolOutput must contain {EXPECTED:?}");
    assert_eq!(
        turn_ok,
        Some(true),
        "turn must finish cleanly after upstream output"
    );
}
