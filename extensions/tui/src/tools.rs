//! Model-facing recall tools over the TUI session archive (ALG-304).
//!
//! `session_list` lets the agent see what prior TUI conversations exist —
//! their ids, start times, and a short label — without reading any message
//! body. It delegates to [`crate::archive::list`], which reads only each
//! session file's header line and returns entries newest-first, bounded.
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

use crate::archive::{self, SessionInfo};

/// Register the TUI recall tools against the shared registry. Called from the
/// extension's `register()` pass only when the registry service is present.
pub fn register_into(registry: &dyn ToolRegistryHandle, sessions_dir: PathBuf) -> Result<()> {
    registry.register_tool(
        session_list_spec(),
        Arc::new(SessionListTool { sessions_dir }),
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tool_registry::ToolRegistry;

    use super::*;

    fn write_session(dir: &Path, name: &str, contents: &str) {
        fs::write(dir.join(name), contents).unwrap();
    }

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
            "{\"type\":\"session\",\"id\":\"old\"}\n",
        );
        write_session(
            &sessions,
            "2024-06-15T12:30:00Z_b.jsonl",
            "{\"type\":\"session\",\"id\":\"newest\",\"label\":\"Recent\"}\n",
        );

        let reg = ToolRegistry::new();
        register_into(&reg, sessions).unwrap();

        let out = reg.dispatch("session_list", json!({})).await;
        assert!(!out.is_error);
        let value: Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(value["count"], 2);
        assert_eq!(value["sessions"][0]["id"], "newest");
        assert_eq!(value["sessions"][0]["startTime"], "2024-06-15T12:30:00Z");
        assert_eq!(value["sessions"][0]["label"], "Recent");
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

    #[tokio::test]
    async fn dispatch_skips_malformed_session_files() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        write_session(
            &sessions,
            "2024-01-01T00:00:00Z_good.jsonl",
            "{\"type\":\"session\",\"id\":\"good\"}\n",
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
