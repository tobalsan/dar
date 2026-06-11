use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use cap_runner::{RunnerHandle, SpawnParams};
use host_api::{Extension, RegisterCtx, StartCtx};
use runner_core::{common_env, effective_command, RunnerSpec};

pub struct CliRunnerExtension;

impl Extension for CliRunnerExtension {
    fn id(&self) -> &'static str {
        "runner-cli"
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            ctx.services
                .service::<dyn cap_runner::Runner>("cli", Arc::new(CliRunner))?;
            Ok(())
        })
    }

    fn start<'a>(&'a self, _ctx: StartCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Clone)]
pub struct CliRunner;

impl cap_runner::Runner for CliRunner {
    fn spawn<'a>(
        &self,
        params: SpawnParams<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<RunnerHandle>> + Send + 'a>,
    > {
        Box::pin(async move { runner_core::spawn_spec(CliSpec, params).await })
    }
}

struct CliSpec;

impl RunnerSpec for CliSpec {
    fn command(&self, params: &SpawnParams<'_>) -> OsString {
        effective_command(params, "sh")
    }

    fn args(&self, _params: &SpawnParams<'_>) -> Vec<OsString> {
        vec![]
    }

    fn stdin_payload(&self, _params: &SpawnParams<'_>) -> Option<Vec<u8>> {
        None
    }

    fn session_dir(&self, _params: &SpawnParams<'_>) -> Option<PathBuf> {
        None
    }

    fn env(&self, params: &SpawnParams<'_>) -> Vec<(OsString, OsString)> {
        common_env(params)
    }

    fn event_kind(&self) -> &'static str {
        "runner.cli"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn params<'a>(workspace: &'a Path, workspace_root: &'a Path) -> SpawnParams<'a> {
        cap_runner::SpawnParams::builder(
            "",
            "cli",
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
        .model(Some("model-a".to_string()))
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
    fn cli_gets_standard_agent_env_contract() {
        let p = params(
            Path::new("/tmp/agent/workspaces/ISSUE-1"),
            Path::new("/tmp/agent/workspaces"),
        );
        let env: Vec<(String, String)> = CliSpec
            .env(&p)
            .into_iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect();

        assert!(env.contains(&("AGENT_ISSUE_IDENTIFIER".into(), "ISSUE-1".into())));
        assert!(env.contains(&("AGENT_ISSUE_ID".into(), "ISSUE-1".into())));
        assert!(env.contains(&("AGENT_RUN_ID".into(), "run-1".into())));
        assert!(env.contains(&("AGENT_PROJECT_ID".into(), "ISSUE-1".into())));
        assert!(env.contains(&("AGENT_PROMPT".into(), "prompt".into())));
        assert!(env.contains(&("AGENT_WORKER_PROMPT".into(), "prompt".into())));
        assert!(env.contains(&("AGENT_MODEL".into(), "model-a".into())));
        assert!(env.contains(&("AGENT_WORKER_MODEL".into(), "model-a".into())));
        assert!(env.contains(&("AGENT_LINEAR_GRAPHQL_TOOL".into(), "1".into())));
    }
}
