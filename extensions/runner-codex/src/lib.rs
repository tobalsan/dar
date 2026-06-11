//! Codex runner extension — `codex app-server` + JSON-RPC turn request.

use std::ffi::OsString;
use std::sync::Arc;

use anyhow::Result;
use cap_runner::{Runner, RunnerHandle, SpawnParams};
use host_api::{Extension, RegisterCtx};
use runner_core::{common_env, effective_command, spawn_backend, worker_tools, BackendSpec};

pub struct RunnerCodexExtension;

impl Extension for RunnerCodexExtension {
    fn id(&self) -> &'static str {
        "runner-codex"
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            ctx.services
                .service::<dyn Runner>("codex", Arc::new(CodexRunner))?;
            Ok(())
        })
    }
}

pub struct CodexRunner;

impl Runner for CodexRunner {
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
        command: effective_command(p.command, "codex"),
        args: codex_args(p),
        stdin_payload: Some(codex_turn_request(p).into_bytes()),
        event_kind: "runner.codex",
        env: common_env(p),
        session_dir: None,
    }
}

fn codex_args(p: &SpawnParams<'_>) -> Vec<OsString> {
    // Headless operation: never ask for human approval and grant full disk
    // access (the agent folder lives outside the workspace cwd, so a
    // restricted sandbox would block it).  These defaults mirror AIHub's
    // codex runner and are always set for unattended dispatch.
    let mut args = vec![
        OsString::from("app-server"),
        OsString::from("-c"),
        OsString::from("approval_policy=\"never\""),
        OsString::from("-c"),
        OsString::from("sandbox_permissions=[\"disk-full-read-access\", \"disk-write-access\"]"),
    ];
    if let Some(ref model) = p.model {
        args.push(OsString::from("-c"));
        args.push(OsString::from(format!("model={model:?}")));
    }
    if let Some(ref provider) = p.provider {
        args.push(OsString::from("-c"));
        args.push(OsString::from(format!("model_provider={provider:?}")));
    }
    if let Some(ref effort) = p.effort {
        args.push(OsString::from("-c"));
        args.push(OsString::from(format!("model_reasoning_effort={effort:?}")));
    }
    args
}

fn codex_turn_request(p: &SpawnParams<'_>) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": p.run_id,
        "method": "turn",
        "params": {
            "prompt": p.prompt,
            "issue_identifier": p.issue_id,
            "run_id": p.run_id,
            "model": p.model,
            "provider": p.provider,
            "thinking": p.thinking,
            // Headless defaults: never require human approval; grant full disk
            // access so the child can reach the issue file outside the workspace.
            "approvalPolicy": "never",
            "sandboxPolicy": "danger-full-access",
            "effort": p.effort,
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
            "codex",
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
    fn codex_sends_app_server_flags_and_turn_request() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let p = params(Some("codex-1".to_string()), workspace, workspace_root);

        let codex_spec = spec(&p);

        // Codex args must include app-server plus the headless approval/sandbox defaults.
        let args = arg_strings(codex_spec.args);
        assert_eq!(args[0], "app-server");
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-c" && w[1].contains("approval_policy")),
            "approval_policy flag missing: {args:?}"
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-c" && w[1].contains("sandbox_permissions")),
            "sandbox_permissions flag missing: {args:?}"
        );
        // model is passed as a -c flag when set
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-c" && w[1].contains("model=")),
            "model -c flag missing: {args:?}"
        );

        // Codex must send a JSON-RPC turn request (not None).
        let payload = codex_spec.stdin_payload.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["method"], "turn");
        assert_eq!(json["params"]["issue_identifier"], "ISSUE-1");
        assert_eq!(json["params"]["model"], "codex-1");
        // Turn request must carry headless defaults.
        assert_eq!(json["params"]["approvalPolicy"], "never");
        assert_eq!(json["params"]["sandboxPolicy"], "danger-full-access");
    }

    #[test]
    fn codex_effort_is_passed_as_config_flag_and_turn_request() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let mut p = params(Some("o3".to_string()), workspace, workspace_root);
        p.effort = Some("high".to_string());

        let args = arg_strings(codex_args(&p));

        // -c model_reasoning_effort="high" must appear
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-c" && w[1].contains("model_reasoning_effort")),
            "model_reasoning_effort flag missing: {args:?}"
        );

        // effort must be in the turn request params
        let json: serde_json::Value =
            serde_json::from_slice(&spec(&p).stdin_payload.unwrap()).unwrap();
        assert_eq!(json["params"]["effort"], "high");
    }

    #[test]
    fn codex_approval_and_sandbox_always_set_even_without_model() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        // No model, no effort.
        let p = params(None, workspace, workspace_root);
        let args = arg_strings(codex_args(&p));

        assert_eq!(args[0], "app-server");
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-c" && w[1].contains("approval_policy")),
            "approval_policy missing without model: {args:?}"
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-c" && w[1].contains("sandbox_permissions")),
            "sandbox_permissions missing without model: {args:?}"
        );
        // model -c flag must NOT be present when model is None
        assert!(
            !args
                .windows(2)
                .any(|w| w[0] == "-c" && w[1].starts_with("model=")),
            "model flag should be absent when unset: {args:?}"
        );
        // effort -c flag must NOT be present
        assert!(
            !args
                .windows(2)
                .any(|w| w[0] == "-c" && w[1].contains("model_reasoning_effort")),
            "effort flag should be absent when unset: {args:?}"
        );
        // provider -c flag must NOT be present
        assert!(
            !args
                .windows(2)
                .any(|w| w[0] == "-c" && w[1].contains("model_provider")),
            "provider flag should be absent when unset: {args:?}"
        );
    }

    #[test]
    fn codex_provider_is_passed_as_config_flag_and_turn_request() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let mut p = params(Some("o3".to_string()), workspace, workspace_root);
        p.provider = Some("openai".to_string());

        let args = arg_strings(codex_args(&p));

        // -c model_provider="openai" must appear
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-c" && w[1].contains("model_provider")),
            "model_provider flag missing: {args:?}"
        );

        // provider must be in the turn request params
        let json: serde_json::Value =
            serde_json::from_slice(&spec(&p).stdin_payload.unwrap()).unwrap();
        assert_eq!(json["params"]["provider"], "openai");
    }

    #[test]
    fn codex_thinking_is_passed_in_turn_request() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let mut p = params(Some("o3".to_string()), workspace, workspace_root);
        p.thinking = Some("8000".to_string());

        // thinking must be in the turn request params
        let json: serde_json::Value =
            serde_json::from_slice(&spec(&p).stdin_payload.unwrap()).unwrap();
        assert_eq!(json["params"]["thinking"], "8000");
    }

    #[test]
    fn codex_provider_and_thinking_absent_when_unset() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let p = params(None, workspace, workspace_root);

        let args = arg_strings(codex_args(&p));
        assert!(
            !args
                .windows(2)
                .any(|w| w[0] == "-c" && w[1].contains("model_provider")),
            "provider flag should be absent when unset: {args:?}"
        );

        let json: serde_json::Value =
            serde_json::from_slice(&spec(&p).stdin_payload.unwrap()).unwrap();
        assert!(json["params"]["provider"].is_null());
        assert!(json["params"]["thinking"].is_null());
    }
}
