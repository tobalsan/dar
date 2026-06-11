//! Integration tests link `cap-runner` as an external crate: passing here is
//! the proof that the `#[non_exhaustive]` `SpawnParams` (no struct-literal
//! allowed outside the crate) is constructible through the builder alone.

use std::path::Path;
use std::sync::{Arc, Mutex};

use cap_runner::{ExitKind, KillReason, RunnerEventSink, RunnerEventStore, SpawnParams};
use chrono::{DateTime, Utc};

struct NullSink;

impl RunnerEventSink for NullSink {
    fn push(&self, _line: String) {}
}

struct NullStore;

impl RunnerEventStore for NullStore {
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

fn build_params<'a>(
    events: &Arc<dyn RunnerEventSink>,
    store: &Arc<dyn RunnerEventStore>,
    last_event_at: &Arc<Mutex<DateTime<Utc>>>,
) -> cap_runner::SpawnParamsBuilder<'a> {
    SpawnParams::builder(
        "claude",
        "claude",
        Path::new("/agent/workspaces/ISSUE-1"),
        Path::new("/agent/workspaces"),
        Path::new("/agent"),
        "do the thing".to_string(),
        "ISSUE-1".to_string(),
        "run-1".to_string(),
        60_000,
        Arc::clone(events),
        Arc::clone(store),
        Arc::clone(last_event_at),
    )
}

#[test]
fn spawn_params_builder_round_trips_all_fields() {
    let events: Arc<dyn RunnerEventSink> = Arc::new(NullSink);
    let store: Arc<dyn RunnerEventStore> = Arc::new(NullStore);
    let last_event_at = Arc::new(Mutex::new(Utc::now()));

    let params = build_params(&events, &store, &last_event_at)
        .model(Some("model-a".into()))
        .provider(Some("anthropic".into()))
        .thinking(Some("high".into()))
        .effort(Some("medium".into()))
        .expose_linear_graphql_tool(true)
        .build();

    assert_eq!(params.command, "claude");
    assert_eq!(params.runner_kind, "claude");
    assert_eq!(params.model.as_deref(), Some("model-a"));
    assert_eq!(params.provider.as_deref(), Some("anthropic"));
    assert_eq!(params.thinking.as_deref(), Some("high"));
    assert_eq!(params.effort.as_deref(), Some("medium"));
    assert_eq!(params.workspace, Path::new("/agent/workspaces/ISSUE-1"));
    assert_eq!(params.workspace_root, Path::new("/agent/workspaces"));
    assert_eq!(params.agent_root, Path::new("/agent"));
    assert_eq!(params.prompt, "do the thing");
    assert_eq!(params.issue_id, "ISSUE-1");
    assert_eq!(params.run_id, "run-1");
    assert_eq!(params.max_run_timeout_ms, 60_000);
    assert!(params.expose_linear_graphql_tool);
    assert!(Arc::ptr_eq(&params.events, &events));
    assert!(Arc::ptr_eq(&params.store, &store));
    assert!(Arc::ptr_eq(&params.last_event_at, &last_event_at));
}

#[test]
fn spawn_params_builder_defaults_optional_fields() {
    let events: Arc<dyn RunnerEventSink> = Arc::new(NullSink);
    let store: Arc<dyn RunnerEventStore> = Arc::new(NullStore);
    let last_event_at = Arc::new(Mutex::new(Utc::now()));

    let params = build_params(&events, &store, &last_event_at).build();

    assert_eq!(params.model, None);
    assert_eq!(params.provider, None);
    assert_eq!(params.thinking, None);
    assert_eq!(params.effort, None);
    assert!(!params.expose_linear_graphql_tool);
}

#[test]
fn exit_kind_variants_classify_as_expected() {
    assert_eq!(ExitKind::Normal, ExitKind::Normal);
    assert_eq!(ExitKind::Abnormal(Some(1)), ExitKind::Abnormal(Some(1)));
    assert_ne!(ExitKind::Abnormal(None), ExitKind::Abnormal(Some(1)));
    assert_ne!(ExitKind::Normal, ExitKind::Abnormal(None));

    let interrupted = ExitKind::Interrupted { reason: "timeout" };
    match interrupted {
        ExitKind::Interrupted { reason } => assert_eq!(reason, "timeout"),
        other => panic!("expected Interrupted, got {other:?}"),
    }
}

#[test]
fn kill_reason_variants_exist() {
    for reason in [
        KillReason::Timeout,
        KillReason::OperatorStop,
        KillReason::Reconcile,
    ] {
        // Exhaustive match: a new variant is a compile error here.
        match reason {
            KillReason::Timeout | KillReason::OperatorStop | KillReason::Reconcile => {}
        }
    }
}

#[test]
fn agent_env_constants_match_contract() {
    assert_eq!(cap_runner::AGENT_ISSUE_IDENTIFIER, "AGENT_ISSUE_IDENTIFIER");
    assert_eq!(cap_runner::AGENT_ISSUE_ID, "AGENT_ISSUE_ID");
    assert_eq!(cap_runner::AGENT_RUN_ID, "AGENT_RUN_ID");
    assert_eq!(cap_runner::AGENT_PROJECT_ID, "AGENT_PROJECT_ID");
    assert_eq!(cap_runner::AGENT_WORKSPACE, "AGENT_WORKSPACE");
    assert_eq!(cap_runner::AGENT_WORKSPACE_ROOT, "AGENT_WORKSPACE_ROOT");
    assert_eq!(cap_runner::AGENT_PROMPT, "AGENT_PROMPT");
    assert_eq!(cap_runner::AGENT_WORKER_PROMPT, "AGENT_WORKER_PROMPT");
    assert_eq!(cap_runner::AGENT_MODEL, "AGENT_MODEL");
    assert_eq!(cap_runner::AGENT_WORKER_MODEL, "AGENT_WORKER_MODEL");
    assert_eq!(
        cap_runner::AGENT_LINEAR_GRAPHQL_TOOL,
        "AGENT_LINEAR_GRAPHQL_TOOL"
    );
    assert_eq!(cap_runner::AGENT_SESSION_DIR, "AGENT_SESSION_DIR");
}
