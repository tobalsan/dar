use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use cap_runner::{RunnerHandle, SpawnParams};
use host_api::{Extension, RegisterCtx, StartCtx};
use runner_core::{common_env, effective_command, env_with_session_dir, RunnerSpec};

pub struct ClaudeRunnerExtension;

impl Extension for ClaudeRunnerExtension {
    fn id(&self) -> &'static str {
        "runner-claude"
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let runner: Arc<dyn cap_runner::Runner> = Arc::new(ClaudeRunner);
            ctx.services
                .service::<dyn cap_runner::Runner>("claude", Arc::clone(&runner))?;
            ctx.services
                .service::<dyn cap_runner::Runner>("claude-code", runner)?;
            Ok(())
        })
    }

    fn start<'a>(&'a self, _ctx: StartCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Clone)]
pub struct ClaudeRunner;

impl cap_runner::Runner for ClaudeRunner {
    fn spawn<'a>(
        &self,
        params: SpawnParams<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<RunnerHandle>> + Send + 'a>,
    > {
        Box::pin(async move { runner_core::spawn_spec(ClaudeSpec, params).await })
    }
}

struct ClaudeSpec;

impl RunnerSpec for ClaudeSpec {
    fn command(&self, params: &SpawnParams<'_>) -> OsString {
        effective_command(params, "claude")
    }

    fn args(&self, params: &SpawnParams<'_>) -> Vec<OsString> {
        claude_args(params)
    }

    fn stdin_payload(&self, params: &SpawnParams<'_>) -> Option<Vec<u8>> {
        Some(params.prompt.clone().into_bytes())
    }

    fn session_dir(&self, params: &SpawnParams<'_>) -> Option<PathBuf> {
        Some(
            params
                .agent_root
                .join("claude-sessions")
                .join(&params.issue_id),
        )
    }

    fn env(&self, params: &SpawnParams<'_>) -> Vec<(OsString, OsString)> {
        env_with_session_dir(common_env(params), &self.session_dir(params).unwrap())
    }

    fn event_kind(&self) -> &'static str {
        "runner.claude"
    }
}

fn claude_args(params: &SpawnParams<'_>) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("-p"),
        OsString::from("--permission-mode"),
        OsString::from("bypassPermissions"),
        OsString::from("--add-dir"),
        params.agent_root.as_os_str().to_os_string(),
    ];
    if let Some(model) = &params.model {
        args.push(OsString::from("--model"));
        args.push(OsString::from(model));
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn params<'a>(workspace: &'a Path, workspace_root: &'a Path) -> SpawnParams<'a> {
        cap_runner::SpawnParams::builder(
            "",
            "claude-code",
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
        .model(Some("claude-opus-4-6".to_string()))
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
    fn claude_args_include_permission_bypass_add_dir_and_model() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let params = params(workspace, workspace_root);
        let args: Vec<_> = claude_args(&params)
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        assert_eq!(
            args,
            vec![
                "-p",
                "--permission-mode",
                "bypassPermissions",
                "--add-dir",
                "/tmp/agent",
                "--model",
                "claude-opus-4-6"
            ]
        );
    }
}
