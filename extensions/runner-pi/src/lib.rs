//! Pi runner extension — JSON-RPC over stdio with a per-issue session dir.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use cap_runner::{Runner, RunnerHandle, SpawnParams};
use host_api::{Extension, RegisterCtx};
use runner_core::{
    common_env, effective_command, env_with_session_dir, spawn_backend, worker_tools, BackendSpec,
};

pub struct RunnerPiExtension;

impl Extension for RunnerPiExtension {
    fn id(&self) -> &'static str {
        "runner-pi"
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            ctx.services
                .service::<dyn Runner>("pi", Arc::new(PiRunner))?;
            Ok(())
        })
    }
}

pub struct PiRunner;

impl Runner for PiRunner {
    fn spawn<'a>(
        &self,
        params: SpawnParams<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<RunnerHandle>> + Send + 'a>,
    > {
        Box::pin(async move { spawn_backend(spec(&params), params).await })
    }
}

fn session_dir(p: &SpawnParams<'_>) -> PathBuf {
    p.agent_root.join("pi-sessions").join(&p.issue_id)
}

fn spec(p: &SpawnParams<'_>) -> BackendSpec {
    let session_dir = session_dir(p);
    BackendSpec {
        command: effective_command(p.command, "pi"),
        args: pi_args(),
        stdin_payload: Some(pi_turn_request(p).into_bytes()),
        event_kind: "runner.pi",
        env: env_with_session_dir(common_env(p), &session_dir),
        session_dir: Some(session_dir),
    }
}

fn pi_args() -> Vec<OsString> {
    vec![]
}

fn pi_turn_request(p: &SpawnParams<'_>) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": p.run_id,
        "method": "turn",
        "params": {
            "prompt": p.prompt,
            "session_dir": session_dir(p),
            "issue_identifier": p.issue_id,
            "run_id": p.run_id,
            "model": p.model,
            "tools": worker_tools(p),
        }
    })
    .to_string()
        + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    struct NullSink;
    impl cap_runner::RunnerEventSink for NullSink {
        fn push(&self, _line: String) {}
    }

    struct NullStore;
    impl cap_runner::RunnerEventStore for NullStore {
        fn insert_event(
            &self,
            _run_id: Option<&str>,
            _issue_identifier: &str,
            _kind: &'static str,
            _payload: &str,
            _ts: DateTime<Utc>,
        ) {
        }
    }

    fn params<'a>(
        model: Option<String>,
        workspace: &'a Path,
        workspace_root: &'a Path,
    ) -> SpawnParams<'a> {
        SpawnParams::builder(
            "runner",
            "pi",
            workspace,
            workspace_root,
            workspace_root.parent().unwrap_or(workspace_root),
            String::new(),
            "ISSUE-1".to_string(),
            "ISSUE-1-test".to_string(),
            1000,
            Arc::new(NullSink),
            Arc::new(NullStore),
            Arc::new(Mutex::new(Utc::now())),
        )
        .model(model)
        .build()
    }

    #[test]
    fn pi_spec_has_no_claude_style_args() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let p = params(None, workspace, workspace_root);

        let args = spec(&p).args;

        assert!(args.is_empty());
    }

    #[test]
    fn pi_spec_uses_per_issue_session_dir() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let p = params(None, workspace, workspace_root);

        assert_eq!(
            spec(&p).session_dir.unwrap(),
            PathBuf::from("/tmp/agent/pi-sessions/ISSUE-1")
        );
    }

    #[test]
    fn linear_graphql_tool_is_gated_in_turn_request() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let mut p = params(None, workspace, workspace_root);

        let without_tool: serde_json::Value = serde_json::from_str(&pi_turn_request(&p)).unwrap();
        assert_eq!(without_tool["params"]["tools"].as_array().unwrap().len(), 0);

        p.expose_linear_graphql_tool = true;
        let with_tool: serde_json::Value = serde_json::from_str(&pi_turn_request(&p)).unwrap();
        assert_eq!(with_tool["params"]["tools"][0]["name"], "linear_graphql");
    }

    #[test]
    fn pi_spec_writes_json_rpc_turn_request() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let p = params(Some("pi-model".to_string()), workspace, workspace_root);

        let payload = String::from_utf8(spec(&p).stdin_payload.unwrap()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["method"], "turn");
        assert_eq!(value["params"]["issue_identifier"], "ISSUE-1");
        assert_eq!(value["params"]["model"], "pi-model");
    }
}
