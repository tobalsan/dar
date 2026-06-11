use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use cap_runner::{RunnerHandle, SpawnParams};
use host_api::{Extension, RegisterCtx, StartCtx};
use runner_core::{common_env, effective_command, env_with_session_dir, worker_tools, RunnerSpec};

pub struct PiRunnerExtension;

impl Extension for PiRunnerExtension {
    fn id(&self) -> &'static str {
        "runner-pi"
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            ctx.services
                .service::<dyn cap_runner::Runner>("pi", Arc::new(PiRunner))?;
            Ok(())
        })
    }

    fn start<'a>(&'a self, _ctx: StartCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Clone)]
pub struct PiRunner;

impl cap_runner::Runner for PiRunner {
    fn spawn<'a>(
        &self,
        params: SpawnParams<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<RunnerHandle>> + Send + 'a>,
    > {
        Box::pin(async move { runner_core::spawn_spec(PiSpec, params).await })
    }
}

struct PiSpec;

impl RunnerSpec for PiSpec {
    fn command(&self, params: &SpawnParams<'_>) -> OsString {
        effective_command(params, "pi")
    }

    fn args(&self, _params: &SpawnParams<'_>) -> Vec<OsString> {
        vec![]
    }

    fn stdin_payload(&self, params: &SpawnParams<'_>) -> Option<Vec<u8>> {
        Some(pi_turn_request(params).into_bytes())
    }

    fn session_dir(&self, params: &SpawnParams<'_>) -> Option<PathBuf> {
        Some(params.agent_root.join("pi-sessions").join(&params.issue_id))
    }

    fn env(&self, params: &SpawnParams<'_>) -> Vec<(OsString, OsString)> {
        env_with_session_dir(common_env(params), &self.session_dir(params).unwrap())
    }

    fn event_kind(&self) -> &'static str {
        "runner.pi"
    }
}

fn pi_turn_request(params: &SpawnParams<'_>) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": params.run_id,
        "method": "turn",
        "params": {
            "prompt": params.prompt,
            "session_dir": params.agent_root.join("pi-sessions").join(&params.issue_id),
            "issue_identifier": params.issue_id,
            "run_id": params.run_id,
            "model": params.model,
            "tools": worker_tools(params),
        }
    })
    .to_string()
        + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn params<'a>(workspace: &'a Path, workspace_root: &'a Path) -> SpawnParams<'a> {
        cap_runner::SpawnParams::builder(
            "",
            "pi",
            workspace,
            workspace_root,
            workspace_root.parent().unwrap(),
            "prompt".to_string(),
            "ISSUE-1".to_string(),
            "run-1".to_string(),
            1000,
            Arc::new(TestSink),
            Arc::new(TestStore),
            Arc::new(std::sync::Mutex::new(chrono::Utc::now())),
        )
        .model(Some("pi-model".to_string()))
        .expose_linear_graphql_tool(true)
        .build()
    }

    struct TestSink;
    impl cap_runner::RunnerEventSink for TestSink {
        fn push(&self, _line: String) {}
    }

    struct TestStore;
    impl cap_runner::RunnerEventStore for TestStore {
        fn insert_event(
            &self,
            _run_id: Option<&str>,
            _issue_identifier: &str,
            _kind: &'static str,
            _payload: &str,
            _ts: chrono::DateTime<chrono::Utc>,
        ) {
        }
    }

    #[test]
    fn pi_sends_json_rpc_turn_request() {
        let p = params(
            Path::new("/tmp/agent/workspaces/ISSUE-1"),
            Path::new("/tmp/agent/workspaces"),
        );
        let json: serde_json::Value = serde_json::from_str(&pi_turn_request(&p)).unwrap();

        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["method"], "turn");
        assert_eq!(json["params"]["issue_identifier"], "ISSUE-1");
        assert_eq!(json["params"]["model"], "pi-model");
        assert_eq!(
            json["params"]["session_dir"],
            "/tmp/agent/pi-sessions/ISSUE-1"
        );
        assert_eq!(json["params"]["tools"][0]["name"], "linear_graphql");
    }
}
