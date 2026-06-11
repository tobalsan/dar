//! Fake runner extension — test shim that echoes `$AGENT_PROMPT`.

use std::ffi::OsString;
use std::sync::Arc;

use anyhow::Result;
use cap_runner::{Runner, RunnerHandle, SpawnParams};
use host_api::{Extension, RegisterCtx};
use runner_core::{common_env, effective_command, spawn_backend, BackendSpec};

pub struct RunnerFakeExtension;

impl Extension for RunnerFakeExtension {
    fn id(&self) -> &'static str {
        "runner-fake"
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            ctx.services
                .service::<dyn Runner>("fake", Arc::new(FakeRunner))?;
            Ok(())
        })
    }
}

pub struct FakeRunner;

impl Runner for FakeRunner {
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
        args: vec![
            OsString::from("-c"),
            OsString::from("printf '%s\\n' \"$AGENT_PROMPT\""),
        ],
        stdin_payload: None,
        event_kind: "runner.fake",
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

    #[test]
    fn fake_echoes_agent_prompt_via_sh() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let p = SpawnParams::builder(
            "",
            "fake",
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
        .build();

        let fake_spec = spec(&p);
        assert_eq!(fake_spec.command, OsString::from("sh"));
        assert_eq!(
            fake_spec.args,
            vec![
                OsString::from("-c"),
                OsString::from("printf '%s\\n' \"$AGENT_PROMPT\""),
            ]
        );
        assert!(fake_spec.stdin_payload.is_none());
    }
}
