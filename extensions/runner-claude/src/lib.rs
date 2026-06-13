//! Claude Code runner extension
//! (`-p --permission-mode bypassPermissions --add-dir <agent-root>`).

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use cap_runner::{Runner, RunnerHandle, SpawnParams};
use host_api::{Extension, RegisterCtx};
use runner_core::{
    common_env, effective_command, env_with_session_dir, spawn_backend, BackendSpec,
};

pub struct RunnerClaudeExtension;

impl Extension for RunnerClaudeExtension {
    fn id(&self) -> &'static str {
        "runner-claude"
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            ctx.services
                .service::<dyn Runner>("claude", Arc::new(ClaudeRunner))?;
            // Back-compat alias for configs that say `runner.use = claude-code`.
            ctx.services
                .service::<dyn Runner>("claude-code", Arc::new(ClaudeRunner))?;
            Ok(())
        })
    }
}

pub struct ClaudeRunner;

impl Runner for ClaudeRunner {
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
    p.agent_root.join("claude-sessions").join(&p.issue_id)
}

fn spec(p: &SpawnParams<'_>) -> BackendSpec {
    let session_dir = session_dir(p);
    BackendSpec {
        command: effective_command(p.command, "claude"),
        args: claude_args(p),
        stdin_payload: Some(p.prompt.clone().into_bytes()),
        event_kind: "runner.claude",
        env: env_with_session_dir(common_env(p), &session_dir),
        session_dir: Some(session_dir),
    }
}

fn claude_args(p: &SpawnParams<'_>) -> Vec<OsString> {
    let mut args = Vec::new();
    // Autonomous runner: no human is present to answer Claude's permission
    // prompts, and the workflow needs the child to edit its issue file, which
    // lives outside the workspace cwd (under the agent folder). Bypass the
    // permission sandbox and widen the allowed dirs to the agent folder.
    args.extend([
        OsString::from("-p"),
        OsString::from("--permission-mode"),
        OsString::from("bypassPermissions"),
        OsString::from("--add-dir"),
        p.agent_root.as_os_str().to_os_string(),
    ]);
    if let Some(ref model) = p.model {
        args.push(OsString::from("--model"));
        args.push(OsString::from(model));
    }
    if let Some(level) = p.thinking.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        args.push(OsString::from("--effort"));
        args.push(OsString::from(level));
    }
    args
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
            "claude-code",
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

    fn arg_strings(args: Vec<OsString>) -> Vec<String> {
        args.into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn claude_code_model_is_passed_to_spawn_args() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let p = params(
            Some("claude-opus-4-6".to_string()),
            workspace,
            workspace_root,
        );

        let args = arg_strings(spec(&p).args);

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

    #[test]
    fn claude_effort_is_passed_as_flag_when_set() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let p = SpawnParams::builder(
            "runner",
            "claude-code",
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
        .thinking(Some("high".to_string()))
        .build();
        let args = arg_strings(claude_args(&p));
        assert!(
            args.windows(2).any(|w| w[0] == "--effort" && w[1] == "high"),
            "--effort flag missing: {args:?}"
        );
    }

    #[test]
    fn claude_omits_effort_when_absent() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let p = params(None, workspace, workspace_root);
        let args = arg_strings(claude_args(&p));
        assert!(!args.iter().any(|a| a == "--effort"), "{args:?}");
    }

    #[test]
    fn claude_spec_uses_per_issue_session_dir() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let p = params(None, workspace, workspace_root);

        assert_eq!(
            spec(&p).session_dir.unwrap(),
            PathBuf::from("/tmp/agent/claude-sessions/ISSUE-1")
        );
    }

    #[test]
    fn claude_stdin_payload_is_the_prompt() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let mut p = params(None, workspace, workspace_root);
        p.prompt = "do the work".to_string();

        assert_eq!(spec(&p).stdin_payload.unwrap(), b"do the work".to_vec());
    }
}
