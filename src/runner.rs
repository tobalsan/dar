//! Agent runner abstraction.
//!
//! Spawns an agent process in its OWN process group (so SIGTERM/SIGKILL reach
//! the whole subtree), cwd = the per-issue workspace (containment asserted
//! first), pipes the rendered prompt / turn-request to stdin, and streams
//! stdout+stderr line-by-line into the log and the recent-events ring.
//!
//! Five runner backends implement the `RunnerSpec` trait:
//!   - `PiRunner`    — JSON-RPC over stdio with a per-issue session dir
//!   - `ClaudeRunner`— Claude Code CLI (`-p --permission-mode bypassPermissions --add-dir`)
//!   - `CodexRunner` — `codex app-server` + JSON-RPC turn request
//!   - `CliRunner`   — arbitrary command with `AGENT_*` env
//!   - `FakeRunner`  — test shim (echo $AGENT_PROMPT)
//!
//! `spawn` returns a `RunnerHandle` the orchestrator awaits and can kill.
//! On timeout or operator/reconcile kill the child's process group is sent
//! SIGTERM, granted a 5 s grace, then SIGKILL.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
pub use cap_runner::{ExitKind, KillReason, RunnerHandle, SpawnParams};
use chrono::{DateTime, Utc};
use runner_core::{setup_process_group, supervise};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::oneshot;

use crate::dotenv;
use crate::logging;
use crate::paths::assert_contained;
use crate::state::EventRing;
use crate::store::{NewEvent, Store};

impl cap_runner::RunnerEventSink for EventRing {
    fn push(&self, line: String) {
        EventRing::push(self, line);
    }
}

impl cap_runner::RunnerEventStore for Store {
    fn insert_event(
        &self,
        run_id: Option<&str>,
        issue_identifier: &str,
        kind: &'static str,
        payload: &str,
        ts: DateTime<Utc>,
    ) {
        let _ = Store::insert_event(
            self,
            &NewEvent {
                run_id,
                issue_identifier,
                kind,
                payload,
                ts,
            },
        );
    }
}

// ---------------------------------------------------------------------------
// RunnerKind
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum RunnerKind {
    Pi,
    Claude,
    Codex,
    Cli,
    Fake,
}

impl RunnerKind {
    fn parse(raw: &str) -> Result<Self> {
        match raw {
            "" | "pi" => Ok(Self::Pi),
            "claude" | "claude-code" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "cli" => Ok(Self::Cli),
            "fake" => Ok(Self::Fake),
            other => {
                bail!("unsupported runner kind {other:?}; expected pi, claude, codex, cli, or fake")
            }
        }
    }

    fn default_command(&self) -> &'static str {
        match self {
            Self::Pi => "pi",
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Cli => "sh",
            Self::Fake => "sh",
        }
    }

    fn event_kind(&self) -> &'static str {
        match self {
            Self::Pi => "runner.pi",
            Self::Claude => "runner.claude",
            Self::Codex => "runner.codex",
            Self::Cli => "runner.cli",
            Self::Fake => "runner.fake",
        }
    }
}

// ---------------------------------------------------------------------------
// RunnerSpec trait
// ---------------------------------------------------------------------------

trait RunnerSpec {
    fn kind(&self) -> RunnerKind;
    fn command(&self) -> OsString;
    fn args(&self) -> Vec<OsString>;
    fn stdin_payload(&self) -> Option<Vec<u8>>;
    fn session_dir(&self) -> Option<PathBuf>;
    fn env(&self) -> Vec<(OsString, OsString)>;
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn effective_command(p: &SpawnParams<'_>, kind: &RunnerKind) -> OsString {
    if p.command.trim().is_empty() {
        OsString::from(kind.default_command())
    } else {
        OsString::from(p.command)
    }
}

/// `AGENT_*` env vars that every runner receives.
///
/// Full list:
///   - `AGENT_ISSUE_IDENTIFIER` — issue identifier (e.g. `PROJ-42`)
///   - `AGENT_ISSUE_ID`         — same value (alias for back-compat with scripts)
///   - `AGENT_RUN_ID`           — unique run attempt ID
///   - `AGENT_PROJECT_ID`       — project/agent ID
///   - `AGENT_WORKSPACE`        — absolute path to the per-issue workspace dir
///   - `AGENT_WORKSPACE_ROOT`   — absolute path to the shared workspaces root
///   - `AGENT_PROMPT`           — rendered prompt text
///   - `AGENT_WORKER_PROMPT`    — same as `AGENT_PROMPT` (alias)
///   - `AGENT_MODEL`            — model name (only when configured)
///   - `AGENT_WORKER_MODEL`     — same as `AGENT_MODEL` (alias, only when configured)
///   - `AGENT_LINEAR_GRAPHQL_TOOL` — set to `1` when the Linear GraphQL tool is enabled
///   - `AGENT_SESSION_DIR`      — path to the per-issue session directory (Pi runner only)
fn common_env(p: &SpawnParams<'_>) -> Vec<(OsString, OsString)> {
    let mut env = vec![
        (
            OsString::from("AGENT_ISSUE_IDENTIFIER"),
            OsString::from(&p.issue_id),
        ),
        (
            OsString::from("AGENT_ISSUE_ID"),
            OsString::from(&p.issue_id),
        ),
        (OsString::from("AGENT_RUN_ID"), OsString::from(&p.run_id)),
        (
            OsString::from("AGENT_PROJECT_ID"),
            OsString::from(&p.issue_id),
        ),
        (
            OsString::from("AGENT_WORKSPACE"),
            p.workspace.as_os_str().to_os_string(),
        ),
        (
            OsString::from("AGENT_WORKSPACE_ROOT"),
            p.workspace_root.as_os_str().to_os_string(),
        ),
        (OsString::from("AGENT_PROMPT"), OsString::from(&p.prompt)),
        (
            OsString::from("AGENT_WORKER_PROMPT"),
            OsString::from(&p.prompt),
        ),
    ];
    if let Some(model) = &p.model {
        env.push((OsString::from("AGENT_MODEL"), OsString::from(model)));
        env.push((OsString::from("AGENT_WORKER_MODEL"), OsString::from(model)));
    }
    if p.expose_linear_graphql_tool {
        env.push((
            OsString::from("AGENT_LINEAR_GRAPHQL_TOOL"),
            OsString::from("1"),
        ));
    }
    env
}

fn env_with_session_dir(
    mut env: Vec<(OsString, OsString)>,
    session_dir: &Path,
) -> Vec<(OsString, OsString)> {
    env.push((
        OsString::from("AGENT_SESSION_DIR"),
        session_dir.as_os_str().to_os_string(),
    ));
    env
}

// ---------------------------------------------------------------------------
// Per-runner implementations
// ---------------------------------------------------------------------------

struct PiRunner<'a> {
    p: &'a SpawnParams<'a>,
}

impl RunnerSpec for PiRunner<'_> {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Pi
    }
    fn command(&self) -> OsString {
        effective_command(self.p, &RunnerKind::Pi)
    }
    fn args(&self) -> Vec<OsString> {
        vec![]
    }
    fn stdin_payload(&self) -> Option<Vec<u8>> {
        Some(pi_turn_request(self.p).into_bytes())
    }
    fn session_dir(&self) -> Option<PathBuf> {
        Some(self.p.agent_root.join("pi-sessions").join(&self.p.issue_id))
    }
    fn env(&self) -> Vec<(OsString, OsString)> {
        env_with_session_dir(common_env(self.p), &self.session_dir().unwrap())
    }
}

struct ClaudeRunner<'a> {
    p: &'a SpawnParams<'a>,
}

impl RunnerSpec for ClaudeRunner<'_> {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Claude
    }
    fn command(&self) -> OsString {
        effective_command(self.p, &RunnerKind::Claude)
    }
    fn args(&self) -> Vec<OsString> {
        claude_args(self.p)
    }
    fn stdin_payload(&self) -> Option<Vec<u8>> {
        Some(self.p.prompt.clone().into_bytes())
    }
    fn session_dir(&self) -> Option<PathBuf> {
        Some(
            self.p
                .agent_root
                .join("claude-sessions")
                .join(&self.p.issue_id),
        )
    }
    fn env(&self) -> Vec<(OsString, OsString)> {
        env_with_session_dir(common_env(self.p), &self.session_dir().unwrap())
    }
}

struct CodexRunner<'a> {
    p: &'a SpawnParams<'a>,
}

impl RunnerSpec for CodexRunner<'_> {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Codex
    }
    fn command(&self) -> OsString {
        effective_command(self.p, &RunnerKind::Codex)
    }
    fn args(&self) -> Vec<OsString> {
        codex_args(self.p)
    }
    fn stdin_payload(&self) -> Option<Vec<u8>> {
        Some(codex_turn_request(self.p).into_bytes())
    }
    fn session_dir(&self) -> Option<PathBuf> {
        None
    }
    fn env(&self) -> Vec<(OsString, OsString)> {
        common_env(self.p)
    }
}

struct CliRunner<'a> {
    p: &'a SpawnParams<'a>,
}

impl RunnerSpec for CliRunner<'_> {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Cli
    }
    fn command(&self) -> OsString {
        effective_command(self.p, &RunnerKind::Cli)
    }
    fn args(&self) -> Vec<OsString> {
        vec![]
    }
    fn stdin_payload(&self) -> Option<Vec<u8>> {
        None
    }
    fn session_dir(&self) -> Option<PathBuf> {
        None
    }
    fn env(&self) -> Vec<(OsString, OsString)> {
        common_env(self.p)
    }
}

struct FakeRunner<'a> {
    p: &'a SpawnParams<'a>,
}

impl RunnerSpec for FakeRunner<'_> {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Fake
    }
    fn command(&self) -> OsString {
        effective_command(self.p, &RunnerKind::Fake)
    }
    fn args(&self) -> Vec<OsString> {
        vec![
            OsString::from("-c"),
            OsString::from("printf '%s\\n' \"$AGENT_PROMPT\""),
        ]
    }
    fn stdin_payload(&self) -> Option<Vec<u8>> {
        None
    }
    fn session_dir(&self) -> Option<PathBuf> {
        None
    }
    fn env(&self) -> Vec<(OsString, OsString)> {
        common_env(self.p)
    }
}

/// Collected outputs from a `RunnerSpec` — all owned so the spec (and its
/// borrow of `SpawnParams`) can be dropped immediately after construction.
struct RunnerParams {
    command: OsString,
    args: Vec<OsString>,
    stdin_payload: Option<Vec<u8>>,
    event_kind: &'static str,
    session_dir: Option<PathBuf>,
    env: Vec<(OsString, OsString)>,
}

fn collect_runner_params<R: RunnerSpec>(r: R) -> RunnerParams {
    RunnerParams {
        command: r.command(),
        args: r.args(),
        stdin_payload: r.stdin_payload(),
        event_kind: r.kind().event_kind(),
        session_dir: r.session_dir(),
        env: r.env(),
    }
}

// ---------------------------------------------------------------------------
// Protocol helpers
// ---------------------------------------------------------------------------

fn pi_turn_request(p: &SpawnParams<'_>) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": p.run_id,
        "method": "turn",
        "params": {
            "prompt": p.prompt,
            "session_dir": p.agent_root.join("pi-sessions").join(&p.issue_id),
            "issue_identifier": p.issue_id,
            "run_id": p.run_id,
            "model": p.model,
            "tools": worker_tools(p),
        }
    })
    .to_string()
        + "\n"
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

fn worker_tools(p: &SpawnParams<'_>) -> Vec<serde_json::Value> {
    if p.expose_linear_graphql_tool {
        vec![serde_json::json!({
            "name": "linear_graphql",
            "description": "Execute a Linear GraphQL operation against the configured Linear API endpoint.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "GraphQL query or mutation document."
                    },
                    "variables": {
                        "type": "object",
                        "description": "GraphQL variables object."
                    }
                },
                "required": ["query"]
            }
        })]
    } else {
        Vec::new()
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
    args
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

// ---------------------------------------------------------------------------
// spawn
// ---------------------------------------------------------------------------

/// Spawn an agent child for one issue. Asserts workspace containment, sets up
/// its own process group, pipes the turn request/prompt to stdin, and
/// supervises the child in a background task that streams output and enforces
/// the per-turn timeout.
pub async fn spawn(p: SpawnParams<'_>) -> Result<RunnerHandle> {
    assert_contained(p.workspace_root, p.workspace)
        .context("workspace containment check failed; refusing to spawn child")?;

    // Build runner and collect all owned data before borrowing p further.
    let rp = match RunnerKind::parse(p.runner_kind)? {
        RunnerKind::Pi => collect_runner_params(PiRunner { p: &p }),
        RunnerKind::Claude => collect_runner_params(ClaudeRunner { p: &p }),
        RunnerKind::Codex => collect_runner_params(CodexRunner { p: &p }),
        RunnerKind::Cli => collect_runner_params(CliRunner { p: &p }),
        RunnerKind::Fake => collect_runner_params(FakeRunner { p: &p }),
    };

    if let Some(dir) = &rp.session_dir {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating runner session dir {}", dir.display()))?;
    }

    let mut cmd = Command::new(&rp.command);
    for arg in &rp.args {
        cmd.arg(arg);
    }
    dotenv::scrub_loaded_env(&mut cmd);
    cmd.envs(rp.env);
    cmd.current_dir(p.workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    setup_process_group(&mut cmd);

    let mut child = cmd.spawn().with_context(|| {
        format!(
            "spawning `{}` in {}",
            rp.command.to_string_lossy(),
            p.workspace.display()
        )
    })?;

    let pid = child
        .id()
        .context("child has no pid immediately after spawn")?;
    logging::ev(
        &p.issue_id,
        "spawn",
        &format!(
            "runner={} pid={pid} cwd={}",
            p.runner_kind,
            p.workspace.display()
        ),
    );
    persist_runner_event(
        p.store.as_ref(),
        Some(&p.run_id),
        &p.issue_id,
        rp.event_kind,
        serde_json::json!({
            "type": "spawn",
            "runner": p.runner_kind,
            "pid": pid,
            "command": rp.command.to_string_lossy(),
            "args": rp.args.iter().map(|a| a.to_string_lossy().to_string()).collect::<Vec<_>>(),
            "session_dir": rp.session_dir.as_ref().map(|d| d.display().to_string()),
        }),
    );

    // Write the turn request / prompt to stdin, then close it (EOF).
    if let (Some(mut stdin), Some(payload)) = (child.stdin.take(), rp.stdin_payload) {
        tokio::spawn(async move {
            let _ = stdin.write_all(&payload).await;
            let _ = stdin.flush().await;
            // drop(stdin) closes the pipe → child sees EOF.
        });
    }

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    if let Some(out) = stdout {
        runner_core::spawn_line_pump(
            out,
            p.issue_id.clone(),
            p.run_id.clone(),
            "stdout",
            rp.event_kind,
            Arc::clone(&p.events),
            Arc::clone(&p.store),
            Arc::clone(&p.last_event_at),
            logging::ev,
        );
    }
    if let Some(err) = stderr {
        runner_core::spawn_line_pump(
            err,
            p.issue_id.clone(),
            p.run_id.clone(),
            "stderr",
            rp.event_kind,
            Arc::clone(&p.events),
            Arc::clone(&p.store),
            Arc::clone(&p.last_event_at),
            logging::ev,
        );
    }

    let (kill_tx, kill_rx) = oneshot::channel::<KillReason>();
    let timeout = Duration::from_millis(p.max_run_timeout_ms);
    let issue_id = p.issue_id.clone();

    let done = tokio::spawn(async move {
        supervise(child, pid, timeout, kill_rx, move |kind, message| {
            logging::ev(&issue_id, kind, &message);
        })
        .await
    });

    Ok(RunnerHandle::new(pid, kill_tx, done))
}

// ---------------------------------------------------------------------------
// Internal
// ---------------------------------------------------------------------------

fn persist_runner_event(
    store: &dyn cap_runner::RunnerEventStore,
    run_id: Option<&str>,
    issue_id: &str,
    kind: &'static str,
    value: serde_json::Value,
) {
    let payload = value.to_string();
    store.insert_event(run_id, issue_id, kind, &payload, Utc::now());
}

pub use runner_core::{term_then_kill, wait_for_pids_dead};

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::EventRing;
    use crate::store::Store;
    use chrono::Utc;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    fn params<'a>(
        runner_kind: &'a str,
        model: Option<String>,
        workspace: &'a Path,
        workspace_root: &'a Path,
    ) -> SpawnParams<'a> {
        SpawnParams::builder(
            "runner",
            runner_kind,
            workspace,
            workspace_root,
            workspace_root.parent().unwrap_or(workspace_root),
            String::new(),
            "ISSUE-1".to_string(),
            "ISSUE-1-test".to_string(),
            1000,
            Arc::new(EventRing::new()),
            Arc::new(Store::open(&PathBuf::from(":memory:")).unwrap()),
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

    /// Build command args via the appropriate per-runner struct.
    fn build_command_args(p: &SpawnParams<'_>) -> Vec<OsString> {
        match RunnerKind::parse(p.runner_kind).unwrap_or(RunnerKind::Pi) {
            RunnerKind::Pi => PiRunner { p }.args(),
            RunnerKind::Claude => ClaudeRunner { p }.args(),
            RunnerKind::Codex => CodexRunner { p }.args(),
            RunnerKind::Cli => CliRunner { p }.args(),
            RunnerKind::Fake => FakeRunner { p }.args(),
        }
    }

    #[test]
    fn claude_code_model_is_passed_to_spawn_args() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let p = params(
            "claude-code",
            Some("claude-opus-4-6".to_string()),
            workspace,
            workspace_root,
        );

        let args = arg_strings(build_command_args(&p));

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
    fn non_claude_runner_does_not_get_claude_spawn_args() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        // Unknown kind falls back to Pi which has no special args.
        let p = params("gemini-code", None, workspace, workspace_root);

        let args = arg_strings(build_command_args(&p));

        assert!(!args.iter().any(|arg| {
            matches!(
                arg.as_str(),
                "-p" | "--permission-mode" | "bypassPermissions" | "--add-dir" | "--model"
            )
        }));
        assert!(args.is_empty());
    }

    #[test]
    fn runner_specs_create_session_dirs_for_pi_and_claude() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let pi = params("pi", None, workspace, workspace_root);
        let claude = params("claude", None, workspace, workspace_root);

        let pi_spec = PiRunner { p: &pi };
        let claude_spec = ClaudeRunner { p: &claude };

        assert_eq!(
            pi_spec.session_dir().unwrap(),
            PathBuf::from("/tmp/agent/pi-sessions/ISSUE-1")
        );
        assert_eq!(
            claude_spec.session_dir().unwrap(),
            PathBuf::from("/tmp/agent/claude-sessions/ISSUE-1")
        );
    }

    #[test]
    fn linear_graphql_tool_is_gated_in_turn_request() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let mut p = params("pi", None, workspace, workspace_root);

        let without_tool: serde_json::Value = serde_json::from_str(&pi_turn_request(&p)).unwrap();
        assert_eq!(without_tool["params"]["tools"].as_array().unwrap().len(), 0);

        p.expose_linear_graphql_tool = true;
        let with_tool: serde_json::Value = serde_json::from_str(&pi_turn_request(&p)).unwrap();
        assert_eq!(with_tool["params"]["tools"][0]["name"], "linear_graphql");
    }

    #[test]
    fn codex_sends_turn_request_and_cli_gets_aihub_env() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let codex = params(
            "codex",
            Some("codex-1".to_string()),
            workspace,
            workspace_root,
        );
        let cli = params(
            "cli",
            Some("model-a".to_string()),
            workspace,
            workspace_root,
        );

        let codex_spec = CodexRunner { p: &codex };
        let cli_spec = CliRunner { p: &cli };

        // Codex args must include app-server plus the headless approval/sandbox defaults.
        let args = arg_strings(codex_spec.args());
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
        let payload = codex_spec.stdin_payload().unwrap();
        let json: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["method"], "turn");
        assert_eq!(json["params"]["issue_identifier"], "ISSUE-1");
        assert_eq!(json["params"]["model"], "codex-1");
        // Turn request must carry headless defaults.
        assert_eq!(json["params"]["approvalPolicy"], "never");
        assert_eq!(json["params"]["sandboxPolicy"], "danger-full-access");

        let env: Vec<(String, String)> = cli_spec
            .env()
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
    fn codex_effort_is_passed_as_config_flag_and_turn_request() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let mut p = params("codex", Some("o3".to_string()), workspace, workspace_root);
        p.effort = Some("high".to_string());

        let codex_spec = CodexRunner { p: &p };
        let args = arg_strings(codex_spec.args());

        // -c model_reasoning_effort="high" must appear
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-c" && w[1].contains("model_reasoning_effort")),
            "model_reasoning_effort flag missing: {args:?}"
        );

        // effort must be in the turn request params
        let json: serde_json::Value =
            serde_json::from_slice(&codex_spec.stdin_payload().unwrap()).unwrap();
        assert_eq!(json["params"]["effort"], "high");
    }

    #[test]
    fn codex_approval_and_sandbox_always_set_even_without_model() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        // No model, no effort.
        let p = params("codex", None, workspace, workspace_root);
        let codex_spec = CodexRunner { p: &p };
        let args = arg_strings(codex_spec.args());

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
        let mut p = params("codex", Some("o3".to_string()), workspace, workspace_root);
        p.provider = Some("openai".to_string());

        let codex_spec = CodexRunner { p: &p };
        let args = arg_strings(codex_spec.args());

        // -c model_provider="openai" must appear
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-c" && w[1].contains("model_provider")),
            "model_provider flag missing: {args:?}"
        );

        // provider must be in the turn request params
        let json: serde_json::Value =
            serde_json::from_slice(&codex_spec.stdin_payload().unwrap()).unwrap();
        assert_eq!(json["params"]["provider"], "openai");
    }

    #[test]
    fn codex_thinking_is_passed_in_turn_request() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let mut p = params("codex", Some("o3".to_string()), workspace, workspace_root);
        p.thinking = Some("8000".to_string());

        let codex_spec = CodexRunner { p: &p };

        // thinking must be in the turn request params
        let json: serde_json::Value =
            serde_json::from_slice(&codex_spec.stdin_payload().unwrap()).unwrap();
        assert_eq!(json["params"]["thinking"], "8000");
    }

    #[test]
    fn codex_provider_and_thinking_absent_when_unset() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let p = params("codex", None, workspace, workspace_root);
        let codex_spec = CodexRunner { p: &p };

        let args = arg_strings(codex_spec.args());
        assert!(
            !args
                .windows(2)
                .any(|w| w[0] == "-c" && w[1].contains("model_provider")),
            "provider flag should be absent when unset: {args:?}"
        );

        let json: serde_json::Value =
            serde_json::from_slice(&codex_spec.stdin_payload().unwrap()).unwrap();
        assert!(json["params"]["provider"].is_null());
        assert!(json["params"]["thinking"].is_null());
    }

    #[test]
    fn pi_spec_writes_json_rpc_turn_request() {
        let workspace_root = Path::new("/tmp/agent/workspaces");
        let workspace = Path::new("/tmp/agent/workspaces/ISSUE-1");
        let pi = params(
            "pi",
            Some("pi-model".to_string()),
            workspace,
            workspace_root,
        );
        let pi_spec = PiRunner { p: &pi };

        let payload = String::from_utf8(pi_spec.stdin_payload().unwrap()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["method"], "turn");
        assert_eq!(value["params"]["issue_identifier"], "ISSUE-1");
        assert_eq!(value["params"]["model"], "pi-model");
    }
}
