use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use cap_runner::{RunnerHandle, SpawnParams};
use host_api::{Extension, RegisterCtx, StartCtx};
use runner_core::{common_env, effective_command, RunnerSpec};

pub struct FakeRunnerExtension;

impl Extension for FakeRunnerExtension {
    fn id(&self) -> &'static str {
        "runner-fake"
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            ctx.services
                .service::<dyn cap_runner::Runner>("fake", Arc::new(FakeRunner))?;
            Ok(())
        })
    }

    fn start<'a>(&'a self, _ctx: StartCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Clone)]
pub struct FakeRunner;

impl cap_runner::Runner for FakeRunner {
    fn spawn<'a>(
        &self,
        params: SpawnParams<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<RunnerHandle>> + Send + 'a>,
    > {
        Box::pin(async move { runner_core::spawn_spec(FakeSpec, params).await })
    }
}

struct FakeSpec;

impl RunnerSpec for FakeSpec {
    fn command(&self, params: &SpawnParams<'_>) -> OsString {
        effective_command(params, "sh")
    }

    fn args(&self, _params: &SpawnParams<'_>) -> Vec<OsString> {
        vec![
            OsString::from("-c"),
            OsString::from("printf '%s\\n' \"$AGENT_PROMPT\""),
        ]
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
        "runner.fake"
    }
}
