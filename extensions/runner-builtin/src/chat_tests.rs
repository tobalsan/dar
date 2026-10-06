//! End-to-end tests of the builtin chat backend against a scripted
//! OpenAI-compatible SSE stub server.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cap_chat::{ChatBackend, ChatEvent, ChatRole, ChatSession, ChatSessionParams};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use crate::{BuiltinChatBackend, COMPACT_PROMPT};

enum Reply {
    Sse(String),
}

fn sse(chunks: &[Value]) -> String {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str(&format!("data: {chunk}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    body
}

fn text_reply(text: &str, usage: Option<(u64, u64)>) -> Reply {
    let mut chunks = vec![
        json!({"choices":[{"delta":{"content":text}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
    ];
    if let Some((prompt, completion)) = usage {
        chunks.push(json!({"choices":[],"usage":{"prompt_tokens":prompt,"completion_tokens":completion,"total_tokens":prompt+completion}}));
    }
    Reply::Sse(sse(&chunks))
}

fn tool_reply(calls: &[(&str, &str)]) -> Reply {
    let calls: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(index, (id, name))| {
            json!({"index":index,"id":id,"type":"function","function":{"name":name,"arguments":"{}"}})
        })
        .collect();
    Reply::Sse(sse(&[
        json!({"choices":[{"delta":{"tool_calls":calls}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
    ]))
}

/// Starts the stub; returns its base URL and the recorded request bodies.
async fn spawn_stub(replies: Vec<Reply>) -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&bodies);
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let replies = Arc::clone(&replies);
            let bodies = Arc::clone(&bodies);
            tokio::spawn(async move {
                let mut data = Vec::new();
                let (header_end, length) = loop {
                    let mut buf = [0u8; 4096];
                    let read = socket.read(&mut buf).await.unwrap();
                    data.extend_from_slice(&buf[..read]);
                    let text = String::from_utf8_lossy(&data).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = text[..end]
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        break (end + 4, length);
                    }
                };
                while data.len() < header_end + length {
                    let mut buf = [0u8; 4096];
                    let read = socket.read(&mut buf).await.unwrap();
                    data.extend_from_slice(&buf[..read]);
                }
                let body: Value = serde_json::from_slice(&data[header_end..]).unwrap();
                bodies.lock().unwrap().push(body);
                let reply = replies.lock().unwrap().pop_front();
                let response = match reply {
                    Some(Reply::Sse(body)) => format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}"
                    ),
                    None => "HTTP/1.1 500 Internal Server Error\r\nconnection: close\r\n\r\nno scripted reply".to_string(),
                };
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (url, recorded)
}

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    sessions: PathBuf,
}

fn fixture(url: &str) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    std::fs::write(
        root.join("agent.yaml"),
        format!("providers:\n  stub:\n    api_url: {url}\n    api_key: k\n"),
    )
    .unwrap();
    let sessions = root.join("sessions");
    Fixture {
        _temp: temp,
        root,
        sessions,
    }
}

fn params(fx: &Fixture) -> cap_chat::ChatSessionParamsBuilder {
    ChatSessionParams::builder("", &fx.root, &fx.sessions)
        .provider(Some("stub".to_string()))
        .model(Some("m".to_string()))
}

async fn open(params: ChatSessionParams) -> (Box<dyn ChatSession>, mpsc::Receiver<ChatEvent>) {
    let (tx, rx) = mpsc::channel(256);
    let session = BuiltinChatBackend.open(params, tx).await.unwrap();
    (session, rx)
}

async fn next_event(rx: &mut mpsc::Receiver<ChatEvent>) -> ChatEvent {
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("timed out waiting for chat event")
        .expect("chat event channel closed")
}

/// Events up to and including the next `TurnFinished`.
async fn run_turn(
    session: &mut Box<dyn ChatSession>,
    rx: &mut mpsc::Receiver<ChatEvent>,
    prompt: &str,
) -> Vec<ChatEvent> {
    session.send_turn(prompt.to_string()).await.unwrap();
    until_finished(rx).await
}

async fn until_finished(rx: &mut mpsc::Receiver<ChatEvent>) -> Vec<ChatEvent> {
    let mut events = Vec::new();
    loop {
        let event = next_event(rx).await;
        let done = matches!(event, ChatEvent::TurnFinished { .. });
        events.push(event);
        if done {
            return events;
        }
    }
}

fn finished(events: &[ChatEvent]) -> (bool, Option<String>) {
    match events.last() {
        Some(ChatEvent::TurnFinished { ok, error }) => (*ok, error.clone()),
        other => panic!("expected TurnFinished, got {other:?}"),
    }
}

fn transcripts(dir: &Path) -> Vec<(PathBuf, Vec<Value>)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .map(|path| {
            let lines = std::fs::read_to_string(&path)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            (path, lines)
        })
        .collect()
}

fn request_messages(bodies: &Arc<Mutex<Vec<Value>>>, index: usize) -> Vec<Value> {
    bodies.lock().unwrap()[index]["messages"]
        .as_array()
        .unwrap()
        .clone()
}

fn roles_and_text(messages: &[Value]) -> Vec<(String, String)> {
    messages
        .iter()
        .map(|m| {
            (
                m["role"].as_str().unwrap().to_string(),
                m["content"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// Fake host tool bridge: answers MCP-style JSON-RPC lines with one tool,
/// `probe`; with `hang_on_call` a `tools/call` never answers.
fn write_bridge_script(dir: &Path, hang_on_call: bool) -> cap_runner::HostToolBridge {
    let on_call = if hang_on_call {
        "sleep 30".to_string()
    } else {
        r#"printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"tool-ok"}]}}\n' "$id""#.to_string()
    };
    let script = format!(
        r#"while read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*) printf '{{"jsonrpc":"2.0","id":%s,"result":{{}}}}\n' "$id" ;;
    *'"tools/list"'*) printf '{{"jsonrpc":"2.0","id":%s,"result":{{"tools":[{{"name":"probe","description":"d","inputSchema":{{"type":"object"}}}}]}}}}\n' "$id" ;;
    *'"tools/call"'*) {on_call} ;;
  esac
done
"#
    );
    let path = dir.join(if hang_on_call { "slow.sh" } else { "fast.sh" });
    std::fs::write(&path, script).unwrap();
    cap_runner::HostToolBridge {
        command: "sh".to_string(),
        args: vec![path.display().to_string()],
    }
}

#[tokio::test]
async fn writes_transcript_with_builtin_header_and_every_message() {
    let (url, bodies) = spawn_stub(vec![text_reply("hello back", None)]).await;
    let fx = fixture(&url);
    let (mut session, mut rx) = open(params(&fx).build()).await;
    assert!(
        transcripts(&fx.sessions).is_empty(),
        "no file before first message"
    );

    let events = run_turn(&mut session, &mut rx, "hello").await;
    assert_eq!(finished(&events), (true, None));

    let files = transcripts(&fx.sessions);
    assert_eq!(files.len(), 1);
    let (path, lines) = &files[0];
    let name = path.file_name().unwrap().to_str().unwrap();
    assert!(name.ends_with(".jsonl") && name.contains('_'), "{name}");
    assert_eq!(lines[0]["type"], "session");
    assert_eq!(lines[0]["backend"], "builtin");
    let id = lines[0]["id"].as_str().unwrap();
    assert!(name.ends_with(&format!("_{id}.jsonl")), "{name}");
    assert_eq!(
        lines[1]["message"],
        json!({"role":"user","content":"hello"})
    );
    assert_eq!(
        lines[2]["message"],
        json!({"role":"assistant","content":"hello back"})
    );
    assert_eq!(lines.len(), 3);
    assert_eq!(bodies.lock().unwrap().len(), 1);
    assert_eq!(
        bodies.lock().unwrap()[0]["stream_options"]["include_usage"],
        true
    );
}

#[tokio::test]
async fn unused_session_leaves_no_file() {
    let (url, _) = spawn_stub(vec![]).await;
    let fx = fixture(&url);
    let (session, _rx) = open(params(&fx).build()).await;
    session.close().await.unwrap();
    assert!(transcripts(&fx.sessions).is_empty());
}

#[tokio::test]
async fn resume_restores_history_and_appends_to_same_file() {
    let (url, bodies) = spawn_stub(vec![
        text_reply("first answer", None),
        text_reply("second", None),
    ])
    .await;
    let fx = fixture(&url);
    let (mut session, mut rx) = open(params(&fx).system_prompt(Some("SYS".into())).build()).await;
    run_turn(&mut session, &mut rx, "first question").await;
    let (_, lines) = transcripts(&fx.sessions).remove(0);
    let id = lines[0]["id"].as_str().unwrap().to_string();
    session.close().await.unwrap();

    let resumed = params(&fx)
        .system_prompt(Some("SYS".into()))
        .resume_session_id(Some(id.clone()))
        .build();
    let (mut session, mut rx) = open(resumed).await;
    let events = run_turn(&mut session, &mut rx, "follow up").await;
    assert_eq!(finished(&events), (true, None));

    assert_eq!(
        roles_and_text(&request_messages(&bodies, 1)),
        [
            ("system".to_string(), "SYS".to_string()),
            ("user".to_string(), "first question".to_string()),
            ("assistant".to_string(), "first answer".to_string()),
            ("user".to_string(), "follow up".to_string()),
        ]
    );
    let files = transcripts(&fx.sessions);
    assert_eq!(files.len(), 1, "resume appends to the same transcript");
    assert_eq!(files[0].1.len(), 5);
    assert_eq!(files[0].1[0]["id"], id.as_str());
}

#[tokio::test]
async fn resume_of_unknown_or_foreign_session_opens_fresh() {
    let (url, bodies) = spawn_stub(vec![text_reply("ok", None)]).await;
    let fx = fixture(&url);
    std::fs::create_dir_all(&fx.sessions).unwrap();
    std::fs::write(
        fx.sessions.join("2024-01-01T00-00-00-000Z_p1.jsonl"),
        "{\"type\":\"session\",\"id\":\"p1\",\"backend\":\"pi\"}\n{\"role\":\"user\",\"content\":\"pi text\"}\n",
    )
    .unwrap();
    for id in ["missing", "p1"] {
        let (mut session, mut rx) =
            open(params(&fx).resume_session_id(Some(id.to_string())).build()).await;
        if id == "p1" {
            run_turn(&mut session, &mut rx, "hi").await;
        }
    }
    assert_eq!(
        roles_and_text(&request_messages(&bodies, 0)),
        [("user".to_string(), "hi".to_string())]
    );
}

#[tokio::test]
async fn abort_mid_tool_call_keeps_history_valid_for_next_turn() {
    let (url, bodies) = spawn_stub(vec![
        tool_reply(&[("c1", "probe"), ("c2", "probe")]),
        text_reply("recovered", None),
    ])
    .await;
    let fx = fixture(&url);
    std::fs::create_dir_all(&fx.root).unwrap();
    let bridge = write_bridge_script(&fx.root, true);
    let (mut session, mut rx) = open(params(&fx).host_tool_bridge(Some(bridge)).build()).await;

    session.send_turn("go".to_string()).await.unwrap();
    loop {
        if matches!(next_event(&mut rx).await, ChatEvent::ToolCall { .. }) {
            break;
        }
    }
    session.abort().await.unwrap();
    let events = until_finished(&mut rx).await;
    assert_eq!(finished(&events), (false, Some("aborted".to_string())));

    let events = run_turn(&mut session, &mut rx, "again").await;
    assert_eq!(finished(&events), (true, None));
    let messages = request_messages(&bodies, 1);
    let shape: Vec<(String, String)> = messages
        .iter()
        .map(|m| {
            (
                m["role"].as_str().unwrap().to_string(),
                m["tool_call_id"]
                    .as_str()
                    .or(m["content"].as_str())
                    .unwrap_or("<calls>")
                    .to_string(),
            )
        })
        .collect();
    assert_eq!(
        shape,
        [
            ("user".to_string(), "go".to_string()),
            ("assistant".to_string(), "<calls>".to_string()),
            ("tool".to_string(), "c1".to_string()),
            ("tool".to_string(), "c2".to_string()),
            ("user".to_string(), "again".to_string()),
        ]
    );
    assert_eq!(messages[2]["content"], "not executed: aborted");
    assert_eq!(messages[3]["content"], "not executed: aborted");
    // The repair was persisted too.
    let lines = transcripts(&fx.sessions).remove(0).1;
    assert!(lines.iter().any(|l| l["message"]["tool_call_id"] == "c2"
        && l["message"]["content"] == "not executed: aborted"));
}

#[tokio::test]
async fn abort_without_turn_is_a_noop_and_session_stays_usable() {
    let (url, _) = spawn_stub(vec![text_reply("fine", None)]).await;
    let fx = fixture(&url);
    let (mut session, mut rx) = open(params(&fx).build()).await;
    session.abort().await.unwrap();
    let events = run_turn(&mut session, &mut rx, "hi").await;
    assert_eq!(finished(&events), (true, None));
}

#[tokio::test]
async fn tool_round_trip_persists_call_and_result() {
    let (url, bodies) = spawn_stub(vec![
        tool_reply(&[("c1", "probe")]),
        text_reply("done", Some((20, 4))),
    ])
    .await;
    let fx = fixture(&url);
    let bridge = write_bridge_script(&fx.root, false);
    let (mut session, mut rx) = open(params(&fx).host_tool_bridge(Some(bridge)).build()).await;
    let events = run_turn(&mut session, &mut rx, "use the tool").await;
    assert_eq!(finished(&events), (true, None));
    assert!(events.iter().any(
        |e| matches!(e, ChatEvent::ToolCall { id, name, .. } if id == "c1" && name == "probe")
    ));
    assert!(events
        .iter()
        .any(|e| matches!(e, ChatEvent::ToolOutput { id, done: true, .. } if id == "c1")));
    let second = request_messages(&bodies, 1);
    assert_eq!(second[2]["role"], "tool");
    assert_eq!(second[2]["tool_call_id"], "c1");
    let lines = transcripts(&fx.sessions).remove(0).1;
    let roles: Vec<&str> = lines[1..]
        .iter()
        .map(|l| l["message"]["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "assistant", "tool", "assistant"]);
}

#[tokio::test]
async fn emits_context_usage_with_configured_window() {
    let (url, _) = spawn_stub(vec![
        text_reply("a", Some((100, 25))),
        text_reply("b", None),
    ])
    .await;
    let fx = fixture(&url);
    let (mut session, mut rx) = open(params(&fx).context_window(Some(8000)).build()).await;
    let events = run_turn(&mut session, &mut rx, "x").await;
    assert!(events.iter().any(|e| matches!(
        e,
        ChatEvent::ContextUsage {
            tokens_used: 125,
            context_window: Some(8000)
        }
    )));
    let events = run_turn(&mut session, &mut rx, "y").await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ChatEvent::ContextUsage { .. })),
        "no usage from provider means no ContextUsage"
    );
}

#[tokio::test]
async fn usage_falls_back_to_total_tokens() {
    let reply = Reply::Sse(sse(&[
        json!({"choices":[{"delta":{"content":"a"}}]}),
        json!({"choices":[],"usage":{"total_tokens":77}}),
    ]));
    let (url, _) = spawn_stub(vec![reply]).await;
    let fx = fixture(&url);
    let (mut session, mut rx) = open(params(&fx).build()).await;
    let events = run_turn(&mut session, &mut rx, "x").await;
    assert!(events.iter().any(|e| matches!(
        e,
        ChatEvent::ContextUsage {
            tokens_used: 77,
            context_window: None
        }
    )));
}

#[tokio::test]
async fn compact_replaces_history_and_resume_replays_from_compaction() {
    let (url, bodies) = spawn_stub(vec![
        text_reply("answer one", None),
        text_reply("THE SUMMARY", Some((500, 12))),
        text_reply("answer two", None),
        text_reply("answer three", None),
    ])
    .await;
    let fx = fixture(&url);
    let (mut session, mut rx) = open(params(&fx).context_window(Some(1000)).build()).await;
    run_turn(&mut session, &mut rx, "question one").await;

    let events = run_turn(&mut session, &mut rx, " /compact ").await;
    assert_eq!(finished(&events), (true, None));
    assert!(matches!(
        &events[0],
        ChatEvent::Delta { role: ChatRole::Assistant, text } if text == "Context compacted."
    ));
    assert!(events.iter().any(|e| matches!(
        e,
        ChatEvent::ContextUsage {
            tokens_used: 12,
            context_window: Some(1000)
        }
    )));
    let compact_request = roles_and_text(&request_messages(&bodies, 1));
    assert_eq!(compact_request.last().unwrap().1, COMPACT_PROMPT);
    assert!(compact_request.iter().all(|(_, text)| text != "/compact"));

    run_turn(&mut session, &mut rx, "question two").await;
    let summary = "Summary of the conversation so far:\n\nTHE SUMMARY".to_string();
    assert_eq!(
        roles_and_text(&request_messages(&bodies, 2)),
        [
            ("user".to_string(), summary.clone()),
            ("user".to_string(), "question two".to_string()),
        ]
    );

    let (_, lines) = transcripts(&fx.sessions).remove(0);
    assert!(lines
        .iter()
        .any(|l| l["type"] == "compaction" && l["summary"] == "THE SUMMARY"));
    let id = lines[0]["id"].as_str().unwrap().to_string();
    session.close().await.unwrap();

    let (mut session, mut rx) = open(params(&fx).resume_session_id(Some(id)).build()).await;
    run_turn(&mut session, &mut rx, "question three").await;
    assert_eq!(
        roles_and_text(&request_messages(&bodies, 3)),
        [
            ("user".to_string(), summary),
            ("user".to_string(), "question two".to_string()),
            ("assistant".to_string(), "answer two".to_string()),
            ("user".to_string(), "question three".to_string()),
        ]
    );
}

#[tokio::test]
async fn compact_with_empty_history_is_a_noop() {
    let (url, bodies) = spawn_stub(vec![]).await;
    let fx = fixture(&url);
    let (mut session, mut rx) = open(params(&fx).build()).await;
    let events = run_turn(&mut session, &mut rx, "/compact").await;
    assert_eq!(finished(&events), (true, None));
    assert!(bodies.lock().unwrap().is_empty());
    assert!(transcripts(&fx.sessions).is_empty());
}

#[tokio::test]
async fn failed_compaction_leaves_history_unchanged() {
    // The second request (the summary call) hits an exhausted stub -> HTTP 500.
    let (url, bodies) = spawn_stub(vec![text_reply("answer one", None)]).await;
    let fx = fixture(&url);
    let (mut session, mut rx) = open(params(&fx).build()).await;
    run_turn(&mut session, &mut rx, "question one").await;
    let events = run_turn(&mut session, &mut rx, "/compact").await;
    assert!(!finished(&events).0);
    let lines = transcripts(&fx.sessions).remove(0).1;
    assert!(lines.iter().all(|l| l["type"] != "compaction"));
    assert_eq!(bodies.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn tool_call_budget_exhaustion_fails_turn_without_dangling_calls() {
    let (url, _) = spawn_stub(vec![tool_reply(&[("c1", "probe"), ("c2", "probe")])]).await;
    let fx = fixture(&url);
    let (mut session, mut rx) = open(params(&fx).max_tool_calls(Some(1)).build()).await;
    let events = run_turn(&mut session, &mut rx, "go").await;
    let (ok, error) = finished(&events);
    assert!(!ok);
    assert!(error.unwrap().contains("runner.max_tool_calls (1)"));
    let lines = transcripts(&fx.sessions).remove(0).1;
    assert!(
        lines
            .iter()
            .all(|l| l["message"].get("tool_calls").is_none()),
        "a rejected batch never enters history"
    );
}

#[tokio::test]
async fn overlapping_turns_queue_in_order() {
    let (url, bodies) = spawn_stub(vec![text_reply("one", None), text_reply("two", None)]).await;
    let fx = fixture(&url);
    let (mut session, mut rx) = open(params(&fx).build()).await;
    session.send_turn("A".to_string()).await.unwrap();
    session.send_turn("B".to_string()).await.unwrap();
    assert_eq!(finished(&until_finished(&mut rx).await), (true, None));
    assert_eq!(finished(&until_finished(&mut rx).await), (true, None));
    assert_eq!(
        roles_and_text(&request_messages(&bodies, 1)),
        [
            ("user".to_string(), "A".to_string()),
            ("assistant".to_string(), "one".to_string()),
            ("user".to_string(), "B".to_string()),
        ]
    );
}

#[tokio::test]
async fn abort_cancels_running_and_queued_turns() {
    let (url, bodies) = spawn_stub(vec![
        tool_reply(&[("c1", "probe")]),
        text_reply("recovered", None),
    ])
    .await;
    let fx = fixture(&url);
    let bridge = write_bridge_script(&fx.root, true);
    let (mut session, mut rx) = open(params(&fx).host_tool_bridge(Some(bridge)).build()).await;
    session.send_turn("A".to_string()).await.unwrap();
    loop {
        if matches!(next_event(&mut rx).await, ChatEvent::ToolCall { .. }) {
            break;
        }
    }
    session.send_turn("B".to_string()).await.unwrap();
    session.abort().await.unwrap();
    for _ in 0..2 {
        let events = until_finished(&mut rx).await;
        assert_eq!(finished(&events), (false, Some("aborted".to_string())));
    }
    let events = run_turn(&mut session, &mut rx, "C").await;
    assert_eq!(finished(&events), (true, None));
    let shape = roles_and_text(&request_messages(&bodies, 1));
    assert_eq!(shape.len(), 4, "B never ran: {shape:?}");
    assert_eq!(shape[2].0, "tool");
    assert_eq!(shape[3], ("user".to_string(), "C".to_string()));
}

#[tokio::test]
async fn abort_does_not_wait_for_a_full_event_channel() {
    let (url, _) = spawn_stub(vec![tool_reply(&[("c1", "probe")])]).await;
    let fx = fixture(&url);
    let bridge = write_bridge_script(&fx.root, true);
    let (tx, mut rx) = mpsc::channel(1);
    let mut session = BuiltinChatBackend
        .open(params(&fx).host_tool_bridge(Some(bridge)).build(), tx)
        .await
        .unwrap();
    // Four cancelled turns need more slots than the loop-guard forwarder
    // buffers, so reporting them blocks unless it runs in the background.
    for prompt in ["A", "B", "C", "D"] {
        session.send_turn(prompt.to_string()).await.unwrap();
    }
    // The unread ToolCall fills the channel while the tool call hangs.
    for _ in 0..500 {
        if rx.len() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(rx.len(), 1, "channel should be full");
    tokio::time::timeout(Duration::from_secs(5), session.abort())
        .await
        .expect("abort must not block on a full channel")
        .unwrap();
    assert!(matches!(
        next_event(&mut rx).await,
        ChatEvent::ToolCall { .. }
    ));
    for _ in 0..4 {
        let events = until_finished(&mut rx).await;
        assert_eq!(finished(&events), (false, Some("aborted".to_string())));
    }
}

#[tokio::test]
async fn compaction_rejects_stream_error_and_keeps_history() {
    let reply = Reply::Sse(sse(&[
        json!({"choices":[{"delta":{"content":"partial"}}]}),
        json!({"error":{"message":"upstream failed"}}),
    ]));
    let (url, _) = spawn_stub(vec![text_reply("answer", None), reply]).await;
    let fx = fixture(&url);
    let (mut session, mut rx) = open(params(&fx).build()).await;
    run_turn(&mut session, &mut rx, "question").await;
    let events = run_turn(&mut session, &mut rx, "/compact").await;
    let (ok, error) = finished(&events);
    assert!(!ok);
    assert!(error.unwrap().contains("upstream failed"));
    let lines = transcripts(&fx.sessions).remove(0).1;
    assert!(lines.iter().all(|l| l["type"] != "compaction"));
}

#[tokio::test]
async fn compaction_usage_falls_back_to_total_tokens() {
    let reply = Reply::Sse(sse(&[
        json!({"choices":[{"delta":{"content":"SUM"}}]}),
        json!({"choices":[],"usage":{"total_tokens":50}}),
    ]));
    let (url, _) = spawn_stub(vec![text_reply("answer", None), reply]).await;
    let fx = fixture(&url);
    let (mut session, mut rx) = open(params(&fx).build()).await;
    run_turn(&mut session, &mut rx, "question").await;
    let events = run_turn(&mut session, &mut rx, "/compact").await;
    assert!(events.iter().any(|e| matches!(
        e,
        ChatEvent::ContextUsage {
            tokens_used: 50,
            ..
        }
    )));
}

#[tokio::test]
async fn failed_compaction_does_not_persist_tool_call_repair() {
    let (url, bodies) = spawn_stub(vec![]).await;
    let fx = fixture(&url);
    std::fs::create_dir_all(&fx.sessions).unwrap();
    let call = json!({"role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":"probe","arguments":"{}"}}]});
    let transcript = format!(
        "{}\n{}\n{}\n",
        json!({"type":"session","id":"d1","backend":"builtin"}),
        json!({"type":"message","message":{"role":"user","content":"go"}}),
        json!({"type":"message","message":call}),
    );
    let path = fx.sessions.join("2024-01-01T00-00-00-000Z_d1.jsonl");
    std::fs::write(&path, &transcript).unwrap();
    let (mut session, mut rx) = open(
        params(&fx)
            .resume_session_id(Some("d1".to_string()))
            .build(),
    )
    .await;
    let events = run_turn(&mut session, &mut rx, "/compact").await;
    assert!(!finished(&events).0);
    // The summary request was still valid: the dangling call was answered virtually.
    let messages = request_messages(&bodies, 0);
    assert_eq!(messages[2]["role"], "tool");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), transcript);
}

#[tokio::test]
async fn resume_after_unterminated_last_line_keeps_entries_separate() {
    let (url, bodies) = spawn_stub(vec![text_reply("ok", None), text_reply("ok2", None)]).await;
    let fx = fixture(&url);
    std::fs::create_dir_all(&fx.sessions).unwrap();
    let transcript = format!(
        "{}\n{}",
        json!({"type":"session","id":"t1","backend":"builtin"}),
        json!({"type":"message","message":{"role":"user","content":"before"}}),
    );
    std::fs::write(
        fx.sessions.join("2024-01-01T00-00-00-000Z_t1.jsonl"),
        transcript,
    )
    .unwrap();
    let params_for = || {
        params(&fx)
            .resume_session_id(Some("t1".to_string()))
            .build()
    };
    let (mut session, mut rx) = open(params_for()).await;
    run_turn(&mut session, &mut rx, "after").await;
    session.close().await.unwrap();
    // `transcripts` parses every line, so glued objects would panic there.
    assert_eq!(transcripts(&fx.sessions)[0].1.len(), 4);
    let (mut session, mut rx) = open(params_for()).await;
    run_turn(&mut session, &mut rx, "again").await;
    let texts = roles_and_text(&request_messages(&bodies, 1));
    assert_eq!(texts[0].1, "before");
    assert_eq!(texts[1].1, "after");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn back_to_back_turns_reach_provider_in_submission_order() {
    let replies = (0..8).map(|i| text_reply(&format!("r{i}"), None)).collect();
    let (url, bodies) = spawn_stub(replies).await;
    let fx = fixture(&url);
    let (mut session, mut rx) = open(params(&fx).build()).await;
    for i in 0..8 {
        session.send_turn(format!("m{i}")).await.unwrap();
    }
    for _ in 0..8 {
        assert_eq!(finished(&until_finished(&mut rx).await), (true, None));
    }
    for i in 0..8 {
        let messages = request_messages(&bodies, i);
        assert_eq!(messages.last().unwrap()["content"], format!("m{i}"));
    }
}
