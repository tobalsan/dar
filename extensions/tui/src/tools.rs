//! Model-facing recall tools over the TUI session archive (ALG-304, ALG-305).
//!
//! `session_list` lets the agent see what prior TUI conversations exist —
//! their ids, start times, and a short label — without reading any message
//! body. It delegates to [`crate::archive::list`], which reads only each
//! session file's header line and returns entries newest-first, bounded.
//!
//! `session_search` (ALG-305) does a lexical substring match over user+assistant
//! message text across all sessions and returns *locations* —
//! `{session_id, message_index, snippet}` with the snippet length-capped, never
//! full message bodies. `session_read` (ALG-305) returns a ranged/paginated
//! slice of one session's messages with a hard maximum cap so a single recall
//! call can never flood the agent's context window. Both delegate to
//! [`crate::archive`] and tolerate malformed lines / empty corpora.
//!
//! Registration mirrors the scheduler's pattern (`tools::register_into`):
//! [`TuiExtension::register`] resolves the shared [`ToolRegistry`] service and,
//! only when it is present, registers this tool. With no registry service (the
//! `tool-registry-host` extension absent) the tool simply isn't registered, the
//! same way the foreground's `host_tool_bridge` wiring is conditional. The tool
//! reaches the agent through the existing host MCP bridge — no new transport.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};

use tool_registry::{ToolExecutor, ToolOutcome, ToolRegistryHandle, ToolSpec};

use crate::archive::{self, Message, SearchHit, SessionInfo, READ_MAX};

/// Register the TUI recall tools against the shared registry. Called from the
/// extension's `register()` pass only when the registry service is present.
pub fn register_into(registry: &dyn ToolRegistryHandle, sessions_dir: PathBuf) -> Result<()> {
    registry.register_tool(
        session_list_spec(),
        Arc::new(SessionListTool {
            sessions_dir: sessions_dir.clone(),
        }),
    )?;
    registry.register_tool(
        session_search_spec(),
        Arc::new(SessionSearchTool {
            sessions_dir: sessions_dir.clone(),
        }),
    )?;
    registry.register_tool(
        session_read_spec(),
        Arc::new(SessionReadTool { sessions_dir }),
    )?;
    Ok(())
}

fn session_list_spec() -> ToolSpec {
    ToolSpec::new(
        "session_list",
        "List prior TUI chat sessions, newest first, with each session's id, \
         start time, and a short label. Reads only session metadata (headers), \
         never message contents. Use it to see what earlier conversations \
         exist before recalling one. Read-only.",
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        }),
    )
    .reads()
}

struct SessionListTool {
    sessions_dir: PathBuf,
}

#[async_trait::async_trait]
impl ToolExecutor for SessionListTool {
    async fn execute(&self, _args: Value) -> Result<ToolOutcome> {
        let sessions = archive::list(&self.sessions_dir);
        Ok(ToolOutcome::ok(render(&sessions)))
    }
}

/// Shape the listing as a compact JSON object for the tool's `text` payload.
fn render(sessions: &[SessionInfo]) -> String {
    let entries: Vec<Value> = sessions
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "startTime": s.start_time,
                "label": s.label,
            })
        })
        .collect();
    serde_json::to_string(&json!({
        "count": entries.len(),
        "sessions": entries,
    }))
    .unwrap_or_else(|_| "{\"count\":0,\"sessions\":[]}".to_string())
}

fn session_search_spec() -> ToolSpec {
    ToolSpec::new(
        "session_search",
        "Search prior TUI chat sessions for a keyword or substring over the \
         text of user and assistant messages. Returns matches as locations — \
         each hit is {session_id, message_index, snippet} where the snippet is \
         a short, length-capped window around the match, never the full \
         message. Use it to find where something was discussed, then call \
         session_read to read the located range. Read-only.",
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keyword or substring to match (case-insensitive).",
                },
            },
            "required": ["query"],
            "additionalProperties": false,
        }),
    )
    .reads()
}

struct SessionSearchTool {
    sessions_dir: PathBuf,
}

#[async_trait::async_trait]
impl ToolExecutor for SessionSearchTool {
    async fn execute(&self, args: Value) -> Result<ToolOutcome> {
        let query = args.get("query").and_then(Value::as_str).unwrap_or("");
        let hits = archive::search(&self.sessions_dir, query);
        Ok(ToolOutcome::ok(render_hits(&hits)))
    }
}

/// Shape search hits as a compact JSON object — locations only, each with a
/// bounded snippet.
fn render_hits(hits: &[SearchHit]) -> String {
    let entries: Vec<Value> = hits
        .iter()
        .map(|h| {
            json!({
                "sessionId": h.session_id,
                "messageIndex": h.message_index,
                "snippet": h.snippet,
            })
        })
        .collect();
    serde_json::to_string(&json!({
        "count": entries.len(),
        "hits": entries,
    }))
    .unwrap_or_else(|_| "{\"count\":0,\"hits\":[]}".to_string())
}

fn session_read_spec() -> ToolSpec {
    ToolSpec::new(
        "session_read",
        format!(
            "Read a ranged slice of one prior TUI session's messages. Args: \
             session_id, start (zero-based message index, default 0), and count \
             (number of messages, default {READ_MAX}). At most {READ_MAX} \
             messages are returned per call — a larger count is clamped — so a \
             single call can never flood the context window. Call repeatedly \
             with an advancing start to page through a long session. Read-only.",
        ),
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Id of the session to read (from session_list or session_search).",
                },
                "start": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Zero-based message index to start at. Default 0.",
                },
                "count": {
                    "type": "integer",
                    "minimum": 1,
                    "description": format!("How many messages to read. Clamped to a hard max of {READ_MAX}."),
                },
            },
            "required": ["session_id"],
            "additionalProperties": false,
        }),
    )
    .reads()
}

struct SessionReadTool {
    sessions_dir: PathBuf,
}

#[async_trait::async_trait]
impl ToolExecutor for SessionReadTool {
    async fn execute(&self, args: Value) -> Result<ToolOutcome> {
        let session_id = args.get("session_id").and_then(Value::as_str).unwrap_or("");
        if session_id.is_empty() {
            return Ok(ToolOutcome::error("session_read requires a session_id"));
        }
        let start = args
            .get("start")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(usize::MAX as u64) as usize;
        // Default to the hard cap when count is omitted; oversized values are
        // clamped inside `archive::read`.
        let count = args
            .get("count")
            .and_then(Value::as_u64)
            .map(|c| c.min(usize::MAX as u64) as usize)
            .unwrap_or(READ_MAX);
        let messages = archive::read(&self.sessions_dir, session_id, start, count);
        Ok(ToolOutcome::ok(render_messages(
            session_id, start, &messages,
        )))
    }
}

/// Shape a read slice as a compact JSON object. `nextStart` advances paging
/// past the returned range; it's the same as `start` for an empty slice.
fn render_messages(session_id: &str, start: usize, messages: &[Message]) -> String {
    let next_start = messages.last().map(|m| m.index + 1).unwrap_or(start);
    let entries: Vec<Value> = messages
        .iter()
        .map(|m| {
            json!({
                "index": m.index,
                "role": m.role,
                "text": m.text,
            })
        })
        .collect();
    serde_json::to_string(&json!({
        "sessionId": session_id,
        "count": entries.len(),
        "nextStart": next_start,
        "messages": entries,
    }))
    .unwrap_or_else(|_| "{\"count\":0,\"messages\":[]}".to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tool_registry::ToolRegistry;

    use super::*;

    fn write_session(dir: &Path, name: &str, contents: &str) {
        fs::write(dir.join(name), contents).unwrap();
    }

    /// A minimal user-message line: `archive::list` excludes sessions with no
    /// renderable messages (opencode resume markers have none), so fixtures
    /// meant to be listed need at least one.
    const MSG_LINE: &str =
        "{\"type\":\"message_end\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n";

    #[test]
    fn spec_is_read_only_with_an_empty_args_schema() {
        let spec = session_list_spec();
        assert_eq!(spec.name, "session_list");
        assert!(spec.access.read && !spec.access.write);
        assert_eq!(spec.input_schema["type"], "object");
        assert_eq!(spec.input_schema["additionalProperties"], false);
    }

    #[tokio::test]
    async fn register_then_dispatch_lists_sessions_newest_first() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        write_session(
            &sessions,
            "2024-01-01T00:00:00Z_a.jsonl",
            &format!("{{\"type\":\"session\",\"id\":\"old\"}}\n{MSG_LINE}"),
        );
        write_session(
            &sessions,
            "2024-06-15T12:30:00Z_b.jsonl",
            &format!("{{\"type\":\"session\",\"id\":\"newest\",\"label\":\"Recent\"}}\n{MSG_LINE}"),
        );

        let reg = ToolRegistry::new();
        register_into(&reg, sessions).unwrap();

        let out = reg.dispatch("session_list", json!({})).await;
        assert!(!out.is_error);
        let value: Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(value["count"], 2);
        assert_eq!(value["sessions"][0]["id"], "newest");
        assert_eq!(value["sessions"][0]["startTime"], "2024-06-15T12:30:00Z");
        assert_eq!(value["sessions"][0]["label"], "hi");
        assert_eq!(value["sessions"][1]["id"], "old");
    }

    #[tokio::test]
    async fn dispatch_on_empty_dir_returns_an_empty_list() {
        let temp = tempfile::tempdir().unwrap();
        let reg = ToolRegistry::new();
        register_into(&reg, temp.path().to_path_buf()).unwrap();
        let out = reg.dispatch("session_list", json!({})).await;
        assert!(!out.is_error);
        let value: Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(value["count"], 0);
        assert_eq!(value["sessions"].as_array().unwrap().len(), 0);
    }

    fn session_with_messages(id: &str, msgs: &[(&str, &str)]) -> String {
        let mut out = format!("{{\"type\":\"session\",\"id\":\"{id}\"}}\n");
        for (role, text) in msgs {
            out.push_str(&format!(
                "{{\"type\":\"message_end\",\"message\":{{\"role\":\"{role}\",\"content\":\"{text}\"}}}}\n"
            ));
        }
        out
    }

    #[test]
    fn search_and_read_specs_are_read_only() {
        let s = session_search_spec();
        assert_eq!(s.name, "session_search");
        assert!(s.access.read && !s.access.write);
        assert_eq!(s.input_schema["required"][0], "query");

        let r = session_read_spec();
        assert_eq!(r.name, "session_read");
        assert!(r.access.read && !r.access.write);
        assert_eq!(r.input_schema["required"][0], "session_id");
    }

    #[tokio::test]
    async fn register_then_dispatch_search_returns_located_hits() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        write_session(
            &sessions,
            "2024-06-15T12:30:00Z_a.jsonl",
            &session_with_messages(
                "sess-a",
                &[
                    ("user", "ask about widgets"),
                    ("assistant", "a gadget reply"),
                ],
            ),
        );

        let reg = ToolRegistry::new();
        register_into(&reg, sessions).unwrap();

        let out = reg
            .dispatch("session_search", json!({ "query": "gadget" }))
            .await;
        assert!(!out.is_error);
        let value: Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(value["count"], 1);
        assert_eq!(value["hits"][0]["sessionId"], "sess-a");
        assert_eq!(value["hits"][0]["messageIndex"], 1);
        assert!(value["hits"][0]["snippet"]
            .as_str()
            .unwrap()
            .contains("gadget"));
    }

    #[tokio::test]
    async fn search_on_empty_corpus_returns_no_hits() {
        let temp = tempfile::tempdir().unwrap();
        let reg = ToolRegistry::new();
        register_into(&reg, temp.path().to_path_buf()).unwrap();
        let out = reg
            .dispatch("session_search", json!({ "query": "anything" }))
            .await;
        assert!(!out.is_error);
        let value: Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(value["count"], 0);
    }

    #[tokio::test]
    async fn register_then_dispatch_read_clamps_to_hard_cap() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let msgs: Vec<(&str, String)> = (0..(READ_MAX + 30))
            .map(|i| ("user", format!("m{i}")))
            .collect();
        let refs: Vec<(&str, &str)> = msgs.iter().map(|(r, t)| (*r, t.as_str())).collect();
        write_session(
            &sessions,
            "2024-06-15T12:30:00Z_a.jsonl",
            &session_with_messages("sess-a", &refs),
        );

        let reg = ToolRegistry::new();
        register_into(&reg, sessions).unwrap();

        let out = reg
            .dispatch(
                "session_read",
                json!({ "session_id": "sess-a", "start": 0, "count": 9999 }),
            )
            .await;
        assert!(!out.is_error);
        let value: Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(value["count"], READ_MAX);
        assert_eq!(value["messages"].as_array().unwrap().len(), READ_MAX);
        // nextStart advances paging past the returned slice.
        assert_eq!(value["nextStart"], READ_MAX);
    }

    #[tokio::test]
    async fn read_pages_deliberately_through_a_session() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let total = READ_MAX + 5;
        let msgs: Vec<(&str, String)> = (0..total).map(|i| ("user", format!("m{i}"))).collect();
        let refs: Vec<(&str, &str)> = msgs.iter().map(|(r, t)| (*r, t.as_str())).collect();
        write_session(
            &sessions,
            "2024-06-15T12:30:00Z_a.jsonl",
            &session_with_messages("sess-a", &refs),
        );

        let reg = ToolRegistry::new();
        register_into(&reg, sessions).unwrap();

        let first = reg
            .dispatch(
                "session_read",
                json!({ "session_id": "sess-a", "start": 0 }),
            )
            .await;
        let first_v: Value = serde_json::from_str(&first.text).unwrap();
        assert_eq!(first_v["count"], READ_MAX);
        let next = first_v["nextStart"].as_u64().unwrap();

        let second = reg
            .dispatch(
                "session_read",
                json!({ "session_id": "sess-a", "start": next }),
            )
            .await;
        let second_v: Value = serde_json::from_str(&second.text).unwrap();
        assert_eq!(second_v["count"], 5);
        assert_eq!(second_v["messages"][0]["index"], READ_MAX);
    }

    #[tokio::test]
    async fn read_missing_session_id_is_an_error_outcome() {
        let temp = tempfile::tempdir().unwrap();
        let reg = ToolRegistry::new();
        register_into(&reg, temp.path().to_path_buf()).unwrap();
        let out = reg.dispatch("session_read", json!({})).await;
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn dispatch_skips_malformed_session_files() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        write_session(
            &sessions,
            "2024-01-01T00:00:00Z_good.jsonl",
            &format!("{{\"type\":\"session\",\"id\":\"good\"}}\n{MSG_LINE}"),
        );
        write_session(&sessions, "2024-02-02T00:00:00Z_bad.jsonl", "not json\n");

        let reg = ToolRegistry::new();
        register_into(&reg, sessions).unwrap();
        let out = reg.dispatch("session_list", json!({})).await;
        let value: Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(value["count"], 1);
        assert_eq!(value["sessions"][0]["id"], "good");
    }
}
