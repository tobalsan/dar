//! CLI runner extension — arbitrary command with the `AGENT_*` env contract.

use std::sync::Arc;

use anyhow::Result;
use cap_runner::{Runner, RunnerHandle, SpawnParams};
use host_api::{Extension, RegisterCtx};
use runner_core::{common_env, effective_command, spawn_backend, BackendSpec};

pub struct RunnerCliExtension;

impl Extension for RunnerCliExtension {
    fn id(&self) -> &'static str {
        "runner-cli"
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            ctx.services
                .service::<dyn Runner>("cli", Arc::new(CliRunner))?;
            Ok(())
        })
    }
}

pub struct CliRunner;

impl Runner for CliRunner {
    fn spawn<'a>(
        &self,
        params: SpawnParams<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<RunnerHandle>> + Send + 'a>,
    > {
        Box::pin(async move { spawn_backend(spec(&params), params).await })
    }
}

fn spec(p: &SpawnParams<'_>) -> BackendSpec {
    BackendSpec {
        command: effective_command(p.command, "sh"),
        args: vec![],
        stdin_payload: None,
        event_kind: "runner.cli",
        env: common_env(p),
        session_dir: None,
    }
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
            "cli",
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
    fn cli_gets_agent_env() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let p = params(Some("model-a".to_string()), workspace, workspace_root);

        let env: Vec<(String, String)> = spec(&p)
            .env
            .into_iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect();
        assert!(env.contains(&("AGENT_ISSUE_IDENTIFIER".into(), "ISSUE-1".into())));
        assert!(env.contains(&("AGENT_MODEL".into(), "model-a".into())));
        assert!(env.contains(&("AGENT_WORKER_MODEL".into(), "model-a".into())));
        assert!(env.iter().any(|(k, _)| k == "AGENT_WORKER_PROMPT"));
    }

    #[test]
    fn cli_has_no_args_session_dir_or_stdin_payload() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let p = params(None, workspace, workspace_root);

        let cli_spec = spec(&p);
        assert!(cli_spec.args.is_empty());
        assert!(cli_spec.session_dir.is_none());
        assert!(cli_spec.stdin_payload.is_none());
    }
}
