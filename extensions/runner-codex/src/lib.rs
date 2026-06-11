use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use cap_runner::{RunnerHandle, SpawnParams};
use host_api::{Extension, RegisterCtx, StartCtx};
use runner_core::{common_env, effective_command, worker_tools, RunnerSpec};

pub struct CodexRunnerExtension;

impl Extension for CodexRunnerExtension {
    fn id(&self) -> &'static str {
        "runner-codex"
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            ctx.services
                .service::<dyn cap_runner::Runner>("codex", Arc::new(CodexRunner))?;
            Ok(())
        })
    }

    fn start<'a>(&'a self, _ctx: StartCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Clone)]
pub struct CodexRunner;

impl cap_runner::Runner for CodexRunner {
    fn spawn<'a>(
        &self,
        params: SpawnParams<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<RunnerHandle>> + Send + 'a>,
    > {
        Box::pin(async move { runner_core::spawn_spec(CodexSpec, params).await })
    }
}

struct CodexSpec;

impl RunnerSpec for CodexSpec {
    fn command(&self, params: &SpawnParams<'_>) -> OsString {
        effective_command(params, "codex")
    }

    fn args(&self, params: &SpawnParams<'_>) -> Vec<OsString> {
        codex_args(params)
    }

    fn stdin_payload(&self, params: &SpawnParams<'_>) -> Option<Vec<u8>> {
        Some(codex_turn_request(params).into_bytes())
    }

    fn session_dir(&self, _params: &SpawnParams<'_>) -> Option<PathBuf> {
        None
    }

    fn env(&self, params: &SpawnParams<'_>) -> Vec<(OsString, OsString)> {
        common_env(params)
    }

    fn event_kind(&self) -> &'static str {
        "runner.codex"
    }
}

fn codex_args(params: &SpawnParams<'_>) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("app-server"),
        OsString::from("-c"),
        OsString::from("approval_policy=\"never\""),
        OsString::from("-c"),
        OsString::from("sandbox_permissions=[\"disk-full-read-access\", \"disk-write-access\"]"),
    ];
    if let Some(model) = &params.model {
        args.push(OsString::from("-c"));
        args.push(OsString::from(format!("model={model:?}")));
    }
    if let Some(provider) = &params.provider {
        args.push(OsString::from("-c"));
        args.push(OsString::from(format!("model_provider={provider:?}")));
    }
    if let Some(effort) = &params.effort {
        args.push(OsString::from("-c"));
        args.push(OsString::from(format!("model_reasoning_effort={effort:?}")));
    }
    args
}

fn codex_turn_request(params: &SpawnParams<'_>) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": params.run_id,
        "method": "turn",
        "params": {
            "prompt": params.prompt,
            "issue_identifier": params.issue_id,
            "run_id": params.run_id,
            "model": params.model,
            "provider": params.provider,
            "thinking": params.thinking,
            "approvalPolicy": "never",
            "sandboxPolicy": "danger-full-access",
            "effort": params.effort,
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
            "codex",
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
        .model(Some("o3".to_string()))
        .provider(Some("openai".to_string()))
        .thinking(Some("8000".to_string()))
        .effort(Some("high".to_string()))
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
    fn codex_args_include_app_server_and_headless_flags() {
        let p = params(
            Path::new("/tmp/agent/workspaces/ISSUE-1"),
            Path::new("/tmp/agent/workspaces"),
        );
        let args: Vec<_> = codex_args(&p)
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        assert_eq!(args[0], "app-server");
        assert!(args
            .windows(2)
            .any(|w| w[0] == "-c" && w[1].contains("approval_policy")));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "-c" && w[1].contains("sandbox_permissions")));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "-c" && w[1].contains("model=")));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "-c" && w[1].contains("model_provider")));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "-c" && w[1].contains("model_reasoning_effort")));
    }

    #[test]
    fn codex_sends_json_rpc_turn_request() {
        let p = params(
            Path::new("/tmp/agent/workspaces/ISSUE-1"),
            Path::new("/tmp/agent/workspaces"),
        );
        let json: serde_json::Value = serde_json::from_str(&codex_turn_request(&p)).unwrap();

        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["method"], "turn");
        assert_eq!(json["params"]["issue_identifier"], "ISSUE-1");
        assert_eq!(json["params"]["model"], "o3");
        assert_eq!(json["params"]["provider"], "openai");
        assert_eq!(json["params"]["thinking"], "8000");
        assert_eq!(json["params"]["effort"], "high");
        assert_eq!(json["params"]["approvalPolicy"], "never");
        assert_eq!(json["params"]["sandboxPolicy"], "danger-full-access");
        assert_eq!(json["params"]["tools"][0]["name"], "linear_graphql");
    }
}
