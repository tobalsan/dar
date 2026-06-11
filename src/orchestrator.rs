//! The orchestration loop (PRD steps 1-9).
//!
//! Owns an in-memory run registry of `max_concurrent` slots, a retry queue with
//! exponential backoff, and the short continuation retry. Ticks every
//! `poll_interval_ms`, draining `ControlMsg`s between/within ticks. It observes
//! issue state and controls child-process lifetime. The only tracker writes it
//! performs are safety/parking writes to needs-human.
//!
//! Run-state machine per slot:
//!   dispatch -> Running
//!   reconcile finds missing/terminal/non-active -> kill -> Cancelled, release
//!   normal exit (0) + still active        -> continuation retry (1s)
//!   normal exit (0) + terminal/neither    -> Succeeded, release
//!   abnormal exit                         -> backoff retry, or Failed at cap
//!   operator Stop                         -> kill -> Cancelled (no retry)
//!
//! ## WORKFLOW.md hot-reload
//!
//! At the start of each tick the orchestrator calls `prompt.maybe_reload()`. On
//! a successful reload it re-derives `effective_cfg` from the new frontmatter +
//! the base `agent_cfg`. If the tracker's active/terminal states changed the
//! tracker is rebuilt so `poll_candidates` uses the new filter immediately.
//! Loop-timing changes (poll_interval_ms) take effect on the next sleep.

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::watch;

use crate::config::AgentConfig;
use crate::domain::Issue;
use crate::dotenv;
#[cfg(test)]
use crate::hitl::NoopHitlNotifier;
use crate::hitl::{HitlNotification, HitlNotify};
use crate::logging;
use crate::paths::{issue_workspace, issue_workspace_path, resolve_workspace_root, AgentPaths};
use crate::prompt::PromptRenderer;
use crate::runner::{ExitKind, KillReason, RunnerHandle, SpawnParams};
use crate::state::{
    ActiveRun, AppState, ControlMsg, ControlReply, HistoryEntry, QueueItem, RetryItem, RunStatus,
};
use crate::store::{
    new_run_id, NewClaim, NewEvent, NewHeartbeat, NewRun, RunFinish, ACTIVE_CONTINUATION_MARKER,
};
use crate::workflow_config::EffectiveLoopConfig;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

/// Max backoff cap for dispatch/abnormal-exit retries (30 minutes).
const BACKOFF_CAP: Duration = Duration::from_secs(30 * 60);
/// Short continuation retry delay for normal-exit-but-still-active (1s).
const CONTINUATION_DELAY: Duration = Duration::from_secs(1);

/// One occupied run slot.
struct RunSlot {
    identifier: String,
    issue: Issue,
    workspace: String,
    handle: Option<RunnerHandle>,
    attempt: u32,
    /// SQLite run_id for this dispatch attempt. Used for finish_run / event writes.
    run_id: String,
    started_at: DateTime<Utc>,
    /// SQLite claim_id from `insert_claim` at dispatch; released at finish.
    claim_id: Option<i64>,
    last_event_at: Arc<Mutex<DateTime<Utc>>>,
}

/// One pending retry. `continuation` retries do NOT count against `max_retries`.
struct Retry {
    identifier: String,
    attempt: u32,
    due_at: DateTime<Utc>,
    last_error: String,
    continuation: bool,
}

pub struct Orchestrator {
    /// Agent definition (id, name, tracker path, etc.). Never modified; used as
    /// the fallback when WORKFLOW.md frontmatter omits a field.
    agent_cfg: AgentConfig,
    paths: AgentPaths,
    tracker: Arc<dyn crate::tracker::Tracker>,
    runner: Arc<dyn cap_runner::Runner>,
    runner_services: host_api::ServiceRegistry,
    prompt: PromptRenderer,
    /// Effective loop config derived from agent_cfg + WORKFLOW.md frontmatter.
    /// Re-derived on every successful WORKFLOW.md reload.
    effective_cfg: EffectiveLoopConfig,
    state: AppState,
    control_rx: UnboundedReceiver<ControlMsg>,
    hitl: Arc<dyn HitlNotify>,

    // In-memory run registry.
    slots: Vec<RunSlot>,
    claims: HashSet<String>,
    retries: Vec<Retry>,
}

impl Orchestrator {
    #[cfg(test)]
    pub fn new(
        agent_cfg: AgentConfig,
        paths: AgentPaths,
        tracker: Arc<dyn crate::tracker::Tracker>,
        prompt: PromptRenderer,
        effective_cfg: EffectiveLoopConfig,
        state: AppState,
        control_rx: UnboundedReceiver<ControlMsg>,
    ) -> Self {
        Self::with_hitl_notifier(
            agent_cfg,
            paths,
            tracker,
            default_runner(),
            default_runner_services(),
            prompt,
            effective_cfg,
            state,
            control_rx,
            Arc::new(NoopHitlNotifier),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_hitl_notifier(
        agent_cfg: AgentConfig,
        paths: AgentPaths,
        tracker: Arc<dyn crate::tracker::Tracker>,
        runner: Arc<dyn cap_runner::Runner>,
        runner_services: host_api::ServiceRegistry,
        prompt: PromptRenderer,
        effective_cfg: EffectiveLoopConfig,
        state: AppState,
        control_rx: UnboundedReceiver<ControlMsg>,
        hitl: Arc<dyn HitlNotify>,
    ) -> Self {
        Self {
            agent_cfg,
            paths,
            tracker,
            runner,
            runner_services,
            prompt,
            effective_cfg,
            state,
            control_rx,
            hitl,
            slots: Vec::new(),
            claims: HashSet::new(),
            retries: Vec::new(),
        }
    }

    /// Main loop. Returns when `shutdown` flips true; kills any active children
    /// first.
    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        loop {
            // One full tick of the loop.
            self.tick().await;
            let poll = self.next_poll_delay();

            // Inter-tick sleep, but stay responsive to control + shutdown.
            let mut pending: Option<ControlMsg> = None;
            tokio::select! {
                _ = tokio::time::sleep(poll) => {}
                _ = shutdown.changed() => {}
                msg = self.control_rx.recv() => {
                    pending = msg;
                }
            }

            if let Some(msg) = pending {
                self.handle_control(msg).await;
            }

            if *shutdown.borrow() {
                break;
            }
        }

        logging::ev(
            "-",
            "shutdown",
            "orchestrator stopping; killing active runs",
        );
        self.kill_all(KillReason::OperatorStop).await;
        self.hitl.stop();
        Ok(())
    }

    /// PRD steps 1-9 for one tick, prefixed with a WORKFLOW.md reload check.
    async fn tick(&mut self) {
        // Step 1: heartbeat / lastTickAt for currently-live runs.
        self.heartbeat_active_runs();

        // Step 2: detect stalled/released runs before observing child exits.
        self.reconcile().await;

        // Step 3: observe child completions.
        self.collect_finished().await;

        // Step 4: load WORKFLOW.md.
        self.maybe_reload_workflow();

        // Poll candidates once per tick (after maybe_reload so config changes
        // apply to the filter immediately).
        let candidates = match self.tracker.poll_candidates() {
            Ok(v) => v,
            Err(e) => {
                logging::ev("-", "poll_error", &format!("{e:#}"));
                Vec::new()
            }
        };

        // Steps 5-9: release/skip/backoff, respect concurrency, dispatch unless paused.
        if !self.state.paused.load(Ordering::SeqCst) {
            self.dispatch(&candidates).await;
        }

        // Refresh dashboard snapshots last so they reflect post-tick reality.
        self.publish_snapshots(&candidates).await;
    }

    fn next_poll_delay(&self) -> Duration {
        let base = self.effective_cfg.poll_interval_ms;
        let jitter = self.effective_cfg.poll_jitter_ms;
        if jitter == 0 {
            return Duration::from_millis(base);
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as u64;
        let span = jitter.saturating_mul(2).saturating_add(1);
        let offset = (now % span) as i128 - jitter as i128;
        Duration::from_millis(base.saturating_add_signed(offset as i64))
    }

    /// Re-read WORKFLOW.md if its mtime changed. On a successful reload:
    /// - Re-derive `effective_cfg` from the new frontmatter.
    /// - Rebuild the tracker if active/terminal states changed.
    ///
    /// When tracker state lists change but the tracker rebuild fails, the
    /// effective_cfg state fields are kept at their current values so the
    /// tracker's internal filter and effective_cfg.active/terminal_states
    /// stay in sync.
    ///
    /// On parse error: `maybe_reload` handles allow_stale internally; log only.
    fn maybe_reload_workflow(&mut self) {
        match self.prompt.maybe_reload() {
            Ok(false) => {}
            Ok(true) => {
                let mut new_eff = EffectiveLoopConfig::merge(
                    &self.agent_cfg,
                    &self.prompt.snapshot().frontmatter,
                );

                // Rebuild the tracker when state lists change so poll_candidates
                // uses the new active/terminal filters immediately. If the
                // rebuild fails, revert the state fields in new_eff so the
                // tracker filter and effective_cfg remain in sync.
                let tracker_changed = new_eff.active_states != self.effective_cfg.active_states
                    || new_eff.terminal_states != self.effective_cfg.terminal_states
                    || new_eff.needs_human != self.effective_cfg.needs_human
                    || new_eff.tracker_kind != self.effective_cfg.tracker_kind
                    || new_eff.tracker_project_slug != self.effective_cfg.tracker_project_slug
                    || new_eff.tracker_endpoint != self.effective_cfg.tracker_endpoint;

                if tracker_changed {
                    let mut tracker_cfg = self.agent_cfg.tracker.clone();
                    tracker_cfg.use_ = new_eff.tracker_kind.clone();
                    tracker_cfg.active_states = new_eff.active_states.clone();
                    tracker_cfg.terminal_states = new_eff.terminal_states.clone();
                    tracker_cfg.project_slug = new_eff.tracker_project_slug.clone();
                    tracker_cfg.endpoint = Some(new_eff.tracker_endpoint.clone());
                    tracker_cfg.needs_human = new_eff.needs_human.clone();
                    let mut services = host_api::ServiceRegistry::default();
                    match crate::tracker::register_configured(
                        &mut services,
                        &tracker_cfg,
                        &self.paths,
                    )
                    .and_then(|_| {
                        services.get_named::<dyn crate::tracker::Tracker>(&tracker_cfg.use_)
                    }) {
                        Ok(tracker) => {
                            self.tracker = tracker;
                            logging::ev(
                                "-",
                                "workflow_reload",
                                &format!(
                                    "tracker rebuilt: kind={} active={:?} terminal={:?}",
                                    new_eff.tracker_kind,
                                    new_eff.active_states,
                                    new_eff.terminal_states
                                ),
                            );
                        }
                        Err(e) => {
                            // Revert all tracker-related fields so the running
                            // tracker instance and effective_cfg stay in sync.
                            new_eff.active_states = self.effective_cfg.active_states.clone();
                            new_eff.terminal_states = self.effective_cfg.terminal_states.clone();
                            new_eff.needs_human = self.effective_cfg.needs_human.clone();
                            new_eff.tracker_kind = self.effective_cfg.tracker_kind.clone();
                            new_eff.tracker_project_slug =
                                self.effective_cfg.tracker_project_slug.clone();
                            new_eff.tracker_endpoint = self.effective_cfg.tracker_endpoint.clone();
                            logging::ev(
                                "-",
                                "workflow_reload",
                                &format!(
                                    "tracker rebuild failed (tracker config unchanged): {e:#}"
                                ),
                            );
                        }
                    }
                }

                let runner_changed = new_eff.runner_kind != self.effective_cfg.runner_kind;
                if runner_changed {
                    let runner_id = runner_service_id(&new_eff.runner_kind);
                    match self
                        .runner_services
                        .get_named::<dyn cap_runner::Runner>(runner_id)
                    {
                        Ok(runner) => {
                            self.runner = runner;
                            logging::ev(
                                "-",
                                "workflow_reload",
                                &format!("runner resolved: kind={runner_id}"),
                            );
                        }
                        Err(e) => {
                            new_eff.runner_kind = self.effective_cfg.runner_kind.clone();
                            new_eff.runner_command = self.effective_cfg.runner_command.clone();
                            logging::ev(
                                "-",
                                "workflow_reload",
                                &format!(
                                    "runner resolution failed (runner config unchanged): {e:#}"
                                ),
                            );
                        }
                    }
                }

                self.effective_cfg = new_eff;
                logging::ev(
                    "-",
                    "workflow_reload",
                    &format!(
                        "effective config updated: poll={}ms concurrent={} retries={}",
                        self.effective_cfg.poll_interval_ms,
                        self.effective_cfg.max_concurrent,
                        self.effective_cfg.max_retries,
                    ),
                );
            }
            Err(e) => {
                logging::ev(
                    "-",
                    "workflow_reload",
                    &format!("WORKFLOW.md reload error: {e:#}"),
                );
            }
        }
    }

    /// Step 2. Re-read each running issue's file; kill+cancel if it is missing,
    /// terminal, or in neither active nor terminal state.
    async fn reconcile(&mut self) {
        let active = &self.effective_cfg.active_states;
        let terminal = &self.effective_cfg.terminal_states;
        let needs_human = self.effective_cfg.needs_human.as_deref();

        let mut to_cancel: Vec<(usize, RunStatus, &'static str, KillReason)> = Vec::new();

        for (idx, slot) in self.slots.iter().enumerate() {
            match &slot.handle {
                None => continue,
                Some(h) if h.is_finished() => continue,
                Some(_) => {}
            }
            let last_event_at = slot
                .last_event_at
                .lock()
                .map(|t| *t)
                .unwrap_or(slot.started_at);
            let stale_for = Utc::now()
                .signed_duration_since(last_event_at)
                .to_std()
                .unwrap_or_default();
            if stale_for > Duration::from_millis(self.effective_cfg.stall_timeout_ms) {
                logging::ev(
                    &slot.identifier,
                    "stalled",
                    "no runner events before timeout; killing",
                );
                to_cancel.push((
                    idx,
                    RunStatus::Stalled,
                    "stalled no runner events",
                    KillReason::Timeout,
                ));
                continue;
            }
            match self.tracker.fetch_one(&slot.identifier) {
                Ok(Some(issue)) => {
                    let st = &issue.state;
                    if terminal.contains(st) {
                        logging::ev(&slot.identifier, "reconcile", "issue terminal; finishing");
                        to_cancel.push((
                            idx,
                            RunStatus::Terminal,
                            "terminal at reconcile",
                            KillReason::Reconcile,
                        ));
                    } else if needs_human == Some(st.as_str()) {
                        logging::ev(
                            &slot.identifier,
                            "reconcile",
                            &format!("issue state {st:?} is needs-human; releasing without retry"),
                        );
                        to_cancel.push((
                            idx,
                            RunStatus::NeedsHuman,
                            "needs-human at reconcile",
                            KillReason::Reconcile,
                        ));
                    } else if !active.contains(st) {
                        logging::ev(
                            &slot.identifier,
                            "reconcile",
                            &format!("issue state {st:?} neither active nor terminal; cancelling"),
                        );
                        to_cancel.push((
                            idx,
                            RunStatus::Released,
                            "non-active at reconcile",
                            KillReason::Reconcile,
                        ));
                    }
                }
                Ok(None) => {
                    logging::ev(
                        &slot.identifier,
                        "reconcile",
                        "issue file missing; cancelling",
                    );
                    to_cancel.push((
                        idx,
                        RunStatus::Orphaned,
                        "issue file missing",
                        KillReason::Reconcile,
                    ));
                }
                Err(e) => {
                    logging::ev(
                        &slot.identifier,
                        "reconcile",
                        &format!("fetch_one error: {e:#}; keeping run"),
                    );
                }
            }
        }

        to_cancel.sort_unstable_by_key(|(idx, _, _, _)| *idx);
        for (idx, status, note, kill_reason) in to_cancel.into_iter().rev() {
            let mut slot = self.slots.remove(idx);
            let pid = slot.handle.as_ref().map(|h| h.pid()).unwrap_or(0);
            if let Some(handle) = slot.handle.take() {
                handle.request_kill(kill_reason);
            }
            if matches!(status, RunStatus::Stalled) {
                self.hitl.notify(HitlNotification::new(
                    "stall",
                    slot.identifier.clone(),
                    note,
                ));
                self.park_issue_for_safety(&slot.issue, note);
            }
            self.record_history(
                &slot.run_id,
                &slot.identifier,
                &slot.issue.id,
                &slot.issue,
                Path::new(&slot.workspace),
                status,
                pid,
                note,
                None,
                slot.claim_id,
            );
            // Not retried: terminal = done, missing/non-active = cancelled.
        }
    }

    /// Steps 7/8. For each slot whose child has finished, classify the exit and
    /// either succeed, schedule a continuation/backoff retry, or fail.
    async fn collect_finished(&mut self) {
        let active = &self.effective_cfg.active_states.clone();
        let terminal = &self.effective_cfg.terminal_states.clone();
        let needs_human = self.effective_cfg.needs_human.clone();
        let max_retries = self.effective_cfg.max_retries;

        let mut finished: Vec<usize> = Vec::new();
        for (idx, slot) in self.slots.iter().enumerate() {
            if let Some(h) = &slot.handle {
                if h.is_finished() {
                    finished.push(idx);
                }
            }
        }

        finished.sort_unstable();
        for idx in finished.into_iter().rev() {
            let mut slot = self.slots.remove(idx);
            let pid = slot.handle.as_ref().map(|h| h.pid()).unwrap_or(0);
            let claim_id = slot.claim_id;
            let handle = match slot.handle.take() {
                Some(h) => h,
                None => continue,
            };
            let exit = handle.wait().await;
            let id = slot.identifier.clone();
            let run_id = slot.run_id.clone();

            match exit {
                ExitKind::Normal => {
                    let state_now = self.tracker.fetch_one(&id).ok().flatten().map(|i| i.state);
                    match state_now {
                        Some(ref st) if terminal.contains(st) => {
                            logging::ev(&id, "succeeded", "terminal after normal exit");
                            self.record_history(
                                &run_id,
                                &id,
                                &slot.issue.id,
                                &slot.issue,
                                Path::new(&slot.workspace),
                                RunStatus::Terminal,
                                pid,
                                "terminal after normal exit",
                                Some(0),
                                claim_id,
                            );
                        }
                        Some(ref st) if needs_human.as_deref() == Some(st.as_str()) => {
                            logging::ev(
                                &id,
                                "needs_human",
                                "needs-human state after normal exit; releasing without retry",
                            );
                            self.record_history(
                                &run_id,
                                &id,
                                &slot.issue.id,
                                &slot.issue,
                                Path::new(&slot.workspace),
                                RunStatus::NeedsHuman,
                                pid,
                                "needs-human after normal exit",
                                Some(0),
                                claim_id,
                            );
                        }
                        Some(ref st) if active.contains(st) => {
                            logging::ev(&id, "continuation", "still active after exit 0; retry 1s");
                            self.record_history(
                                &run_id,
                                &id,
                                &slot.issue.id,
                                &slot.issue,
                                Path::new(&slot.workspace),
                                RunStatus::Succeeded,
                                pid,
                                // record_history stores "{:?} {note}"; the resulting payload
                                // must equal ACTIVE_CONTINUATION_EVENT for the park barrier.
                                ACTIVE_CONTINUATION_MARKER,
                                Some(0),
                                claim_id,
                            );
                            self.retries.push(Retry {
                                identifier: id.clone(),
                                attempt: slot.attempt,
                                due_at: Utc::now()
                                    + chrono::Duration::from_std(CONTINUATION_DELAY).unwrap(),
                                last_error: String::new(),
                                continuation: true,
                            });
                        }
                        _ => {
                            logging::ev(
                                &id,
                                "succeeded",
                                "non-active after normal exit; releasing",
                            );
                            self.record_history(
                                &run_id,
                                &id,
                                &slot.issue.id,
                                &slot.issue,
                                Path::new(&slot.workspace),
                                RunStatus::Succeeded,
                                pid,
                                "non-active after normal exit",
                                Some(0),
                                claim_id,
                            );
                        }
                    }
                }
                ExitKind::Interrupted { reason } => {
                    logging::ev(&id, "interrupted", &format!("reason={reason}"));
                    self.record_history(
                        &run_id,
                        &id,
                        &slot.issue.id,
                        &slot.issue,
                        Path::new(&slot.workspace),
                        RunStatus::Interrupted,
                        pid,
                        reason,
                        None,
                        claim_id,
                    );
                }
                ExitKind::Abnormal(exit_code) => {
                    let state_now = self.tracker.fetch_one(&id).ok().flatten().map(|i| i.state);
                    if matches!(
                        (state_now.as_deref(), needs_human.as_deref()),
                        (Some(st), Some(needs_human)) if st == needs_human
                    ) {
                        logging::ev(
                            &id,
                            "needs_human",
                            "needs-human after abnormal exit; releasing without retry",
                        );
                        self.record_history(
                            &run_id,
                            &id,
                            &slot.issue.id,
                            &slot.issue,
                            Path::new(&slot.workspace),
                            RunStatus::NeedsHuman,
                            pid,
                            "needs-human after abnormal exit",
                            exit_code,
                            claim_id,
                        );
                    } else if slot.attempt >= max_retries {
                        logging::ev(
                            &id,
                            "failed",
                            &format!(
                                "abnormal exit; retries exhausted ({}/{})",
                                slot.attempt, max_retries
                            ),
                        );
                        self.park_issue_for_safety(&slot.issue, "worker error; retries exhausted");
                        self.record_history(
                            &run_id,
                            &id,
                            &slot.issue.id,
                            &slot.issue,
                            Path::new(&slot.workspace),
                            RunStatus::Failed,
                            pid,
                            "abnormal exit; retries exhausted",
                            exit_code,
                            claim_id,
                        );
                    } else {
                        let next = slot.attempt + 1;
                        let delay = backoff(self.effective_cfg.retry_backoff_ms, next);
                        let due = Utc::now() + chrono::Duration::from_std(delay).unwrap();
                        logging::ev(
                            &id,
                            "retry_queued",
                            &format!(
                                "abnormal exit; attempt {next}/{max_retries} in {}ms",
                                delay.as_millis()
                            ),
                        );
                        // Release claim for this attempt; a new claim is opened
                        // when the retry is dispatched.
                        if let Some(cid) = claim_id {
                            let _ = self.state.store.release_claim(cid, Utc::now());
                        }
                        self.claims.remove(&slot.issue.id);
                        let _ = self.state.store.insert_event(&NewEvent {
                            run_id: Some(&run_id),
                            issue_identifier: &id,
                            kind: "lifecycle",
                            payload: &format!(
                                "retry_queued attempt {next}/{max_retries} in {}ms",
                                delay.as_millis()
                            ),
                            ts: Utc::now(),
                        });
                        self.retries.push(Retry {
                            identifier: id.clone(),
                            attempt: next,
                            due_at: due,
                            last_error: "abnormal exit".to_string(),
                            continuation: false,
                        });
                    }
                }
            }
        }
    }

    /// Steps 4-6. Sort and dispatch the pre-fetched candidate set into free slots.
    async fn dispatch(&mut self, candidates: &[Issue]) {
        let max = self.effective_cfg.max_concurrent;
        if self.slots.len() >= max {
            return;
        }

        let busy: HashSet<String> = self.slots.iter().map(|s| s.issue.id.clone()).collect();
        let busy_identifiers: HashSet<String> =
            self.slots.iter().map(|s| s.identifier.clone()).collect();

        let now = Utc::now();

        // First, dispatch due retries (continuation + backoff), oldest first.
        let mut due_idx: Vec<usize> = self
            .retries
            .iter()
            .enumerate()
            .filter(|(_, r)| r.due_at <= now && !busy_identifiers.contains(&r.identifier))
            .map(|(i, _)| i)
            .collect();
        due_idx.sort_unstable();

        let mut due_retries: Vec<Retry> = Vec::new();
        for idx in due_idx.into_iter().rev() {
            due_retries.push(self.retries.remove(idx));
        }
        due_retries.reverse();

        for retry in due_retries {
            if self.slots.len() >= max {
                self.retries.push(retry);
                continue;
            }
            if self.slots.iter().any(|s| s.identifier == retry.identifier) {
                continue;
            }
            match self.tracker.fetch_one(&retry.identifier) {
                Ok(Some(issue)) if self.effective_cfg.active_states.contains(&issue.state) => {
                    if self.park_if_active_run_barrier_reached(&issue) {
                        continue;
                    }
                    if self.claims.contains(&issue.id) {
                        self.retries.push(retry);
                        continue;
                    }
                    let label = if retry.continuation {
                        "continuation"
                    } else {
                        "retry"
                    };
                    logging::ev(
                        &retry.identifier,
                        "dispatch",
                        &format!("from {label} attempt={}", retry.attempt),
                    );
                    self.try_dispatch(issue, retry.attempt).await;
                }
                _ => {
                    logging::ev(
                        &retry.identifier,
                        "retry_drop",
                        "no longer active; dropping retry",
                    );
                }
            }
        }

        if self.slots.len() >= max {
            return;
        }

        let retry_ids: HashSet<String> =
            self.retries.iter().map(|r| r.identifier.clone()).collect();
        let mut fresh: Vec<Issue> = candidates
            .iter()
            .filter(|i| {
                !busy.contains(&i.id)
                    && !retry_ids.contains(&i.identifier)
                    && !retry_ids.contains(&i.id)
                    && !self.claims.contains(&i.id)
            })
            .cloned()
            .collect();

        if self.tracker.sort_candidates_locally() {
            sort_candidates(&mut fresh);
        }

        for issue in fresh {
            if self.slots.len() >= max {
                break;
            }
            if self.claims.contains(&issue.id) {
                continue;
            }
            if self.park_if_active_run_barrier_reached(&issue) {
                continue;
            }
            logging::ev(&issue.identifier, "dispatch", "fresh candidate");
            self.try_dispatch(issue, 0).await;
        }
    }

    /// Render the prompt and spawn a child for one issue, creating a run slot.
    /// On render failure (strict-undefined) the child is NOT spawned; the
    /// attempt is treated as abnormal and scheduled for backoff retry.
    async fn try_dispatch(&mut self, issue: Issue, attempt: u32) {
        if !self.claims.insert(issue.id.clone()) {
            logging::ev(&issue.identifier, "claim_skip", "issue already claimed");
            return;
        }
        // Generate run_id early so pre-spawn failures can persist a runs row.
        let started_at = Utc::now();
        let run_id = new_run_id(&issue.identifier, &started_at);
        let max_retries = self.effective_cfg.max_retries;
        let prompt = match self.prompt.render(&issue, attempt, max_retries) {
            Ok(p) => p,
            Err(e) => {
                let msg = format!("WORKFLOW.md render failed: {e:#}");
                logging::ev(&issue.identifier, "render_error", &msg);
                let _ = self.state.store.insert_event(&NewEvent {
                    run_id: Some(&run_id),
                    issue_identifier: &issue.identifier,
                    kind: "lifecycle",
                    payload: &format!("render_error {msg}"),
                    ts: started_at,
                });
                self.schedule_backoff_after_render_failure(
                    &issue,
                    attempt,
                    &run_id,
                    started_at,
                    &format!("render error: {e}"),
                );
                self.claims.remove(&issue.id);
                return;
            }
        };

        let ws_root = resolve_workspace_root(&self.paths.root, &self.effective_cfg.workspace_root);
        if let Err(e) = std::fs::create_dir_all(&ws_root) {
            let msg = format!("creating workspace root {}: {e}", ws_root.display());
            logging::ev(&issue.identifier, "workspace_error", &msg);
            let _ = self.state.store.insert_event(&NewEvent {
                run_id: Some(&run_id),
                issue_identifier: &issue.identifier,
                kind: "lifecycle",
                payload: &format!("workspace_error {msg}"),
                ts: started_at,
            });
            self.schedule_backoff_after_render_failure(
                &issue,
                attempt,
                &run_id,
                started_at,
                "workspace root error",
            );
            self.claims.remove(&issue.id);
            return;
        }
        let workspace_path = match issue_workspace_path(&ws_root, &issue.identifier) {
            Ok(w) => w,
            Err(e) => {
                let msg = format!("{e:#}");
                logging::ev(&issue.identifier, "workspace_error", &msg);
                let _ = self.state.store.insert_event(&NewEvent {
                    run_id: Some(&run_id),
                    issue_identifier: &issue.identifier,
                    kind: "lifecycle",
                    payload: &format!("workspace_error {msg}"),
                    ts: started_at,
                });
                self.schedule_backoff_after_render_failure(
                    &issue,
                    attempt,
                    &run_id,
                    started_at,
                    "workspace error",
                );
                self.claims.remove(&issue.id);
                return;
            }
        };
        if !self.effective_cfg.workspace_reuse && workspace_path.exists() {
            if let Err(e) = std::fs::remove_dir_all(&workspace_path) {
                let msg = format!(
                    "removing existing workspace {}: {e}",
                    workspace_path.display()
                );
                logging::ev(&issue.identifier, "workspace_error", &msg);
                let _ = self.state.store.insert_event(&NewEvent {
                    run_id: Some(&run_id),
                    issue_identifier: &issue.identifier,
                    kind: "lifecycle",
                    payload: &format!("workspace_error {msg}"),
                    ts: started_at,
                });
                self.schedule_backoff_after_render_failure(
                    &issue,
                    attempt,
                    &run_id,
                    started_at,
                    "workspace reuse error",
                );
                self.claims.remove(&issue.id);
                return;
            }
        }
        let existed = workspace_path.exists();
        let workspace = match issue_workspace(&ws_root, &issue.identifier) {
            Ok(w) => w,
            Err(e) => {
                let msg = format!("{e:#}");
                logging::ev(&issue.identifier, "workspace_error", &msg);
                let _ = self.state.store.insert_event(&NewEvent {
                    run_id: Some(&run_id),
                    issue_identifier: &issue.identifier,
                    kind: "lifecycle",
                    payload: &format!("workspace_error {msg}"),
                    ts: started_at,
                });
                self.schedule_backoff_after_render_failure(
                    &issue,
                    attempt,
                    &run_id,
                    started_at,
                    "workspace error",
                );
                self.claims.remove(&issue.id);
                return;
            }
        };
        if !existed {
            if let Err(e) = self.run_lifecycle_hook("after_create", &issue, &workspace) {
                logging::ev(
                    &issue.identifier,
                    "hook_error",
                    &format!("after_create failed: {e:#}"),
                );
            }
        }

        if let Err(e) = self.run_lifecycle_hook("before_run", &issue, &workspace) {
            let msg = format!("before_run failed: {e:#}");
            let ended_at = Utc::now();
            logging::ev(&issue.identifier, "hook_failed", &msg);
            // Persist a runs row so this failure is visible in dashboard history
            // and counted by the park barrier.
            if let Err(e) = self.state.store.insert_run(&NewRun {
                run_id: &run_id,
                issue_id: &issue.id,
                issue_identifier: &issue.identifier,
                workspace: &workspace.display().to_string(),
                profile_json: None,
                workflow_path: Some(&self.paths.workflow_md().display().to_string()),
                workflow_sha: None,
                pid: 0,
                worker_id: Some("orchestrator"),
                started_at,
            }) {
                tracing::warn!(issue = %issue.identifier, "insert_run (hook_failed) SQLite write failed: {e:#}");
            }
            let _ = self.state.store.finish_run(
                &run_id,
                &RunFinish {
                    outcome: RunStatus::HookFailed,
                    exit_code: None,
                    finished_at: ended_at,
                },
            );
            self.state.history.push(HistoryEntry {
                identifier: issue.identifier.clone(),
                status: RunStatus::HookFailed,
                pid: 0,
                ended_at,
                note: msg.clone(),
            });
            let _ = self.state.store.insert_event(&NewEvent {
                run_id: Some(&run_id),
                issue_identifier: &issue.identifier,
                kind: "lifecycle",
                payload: &format!("HookFailed {msg}"),
                ts: ended_at,
            });
            self.cleanup_workspace_if_needed(&issue, &workspace, RunStatus::HookFailed);
            self.claims.remove(&issue.id);
            return;
        }

        let last_event_at = Arc::new(Mutex::new(started_at));
        let runner_events: Arc<dyn cap_runner::RunnerEventSink> = self.state.events.clone();
        let runner_store: Arc<dyn cap_runner::RunnerEventStore> = self.state.store.clone();
        let params = SpawnParams::builder(
            &self.effective_cfg.runner_command,
            &self.effective_cfg.runner_kind,
            &workspace,
            &ws_root,
            &self.paths.root,
            prompt,
            issue.identifier.clone(),
            run_id.clone(),
            self.effective_cfg.max_run_timeout_ms,
            runner_events,
            runner_store,
            Arc::clone(&last_event_at),
        )
        .model(self.effective_cfg.model.clone())
        .provider(self.effective_cfg.provider.clone())
        .thinking(self.effective_cfg.thinking.clone())
        .effort(self.effective_cfg.effort.clone())
        .expose_linear_graphql_tool(self.effective_cfg.linear.worker_tool.unwrap_or(false))
        .build();

        match self.runner.spawn(params).await {
            Ok(handle) => {
                let pid = handle.pid();
                // Persist the new run to SQLite.
                if let Err(e) = self.state.store.insert_run(&NewRun {
                    run_id: &run_id,
                    issue_id: &issue.id,
                    issue_identifier: &issue.identifier,
                    workspace: &workspace.display().to_string(),
                    profile_json: None,
                    workflow_path: Some(&self.paths.workflow_md().display().to_string()),
                    workflow_sha: None,
                    pid,
                    worker_id: None,
                    started_at,
                }) {
                    tracing::warn!(issue = %issue.identifier, "insert_run SQLite write failed: {e:#}");
                }
                // Mirror claim into the claims table for tracking.
                let claim_id = match self.state.store.insert_claim(&NewClaim {
                    run_id: &run_id,
                    issue_id: &issue.id,
                    issue_identifier: &issue.identifier,
                    worker_id: "orchestrator",
                    claimed_at: started_at,
                }) {
                    Ok(claim_id) => Some(claim_id),
                    Err(e) => {
                        tracing::warn!(issue = %issue.identifier, "insert_claim SQLite write failed: {e:#}");
                        handle.request_kill(KillReason::Reconcile);
                        self.state.history.push(HistoryEntry {
                            identifier: issue.identifier.clone(),
                            status: RunStatus::DispatchFailed,
                            pid,
                            ended_at: Utc::now(),
                            note: "persisted claim rejected".to_string(),
                        });
                        let _ = self.state.store.finish_run(
                            &run_id,
                            &RunFinish {
                                outcome: RunStatus::DispatchFailed,
                                exit_code: None,
                                finished_at: Utc::now(),
                            },
                        );
                        self.cleanup_workspace_if_needed(
                            &issue,
                            &workspace,
                            RunStatus::DispatchFailed,
                        );
                        self.claims.remove(&issue.id);
                        return;
                    }
                };
                // Lifecycle event: dispatch.
                let _ = self.state.store.insert_event(&NewEvent {
                    run_id: Some(&run_id),
                    issue_identifier: &issue.identifier,
                    kind: "lifecycle",
                    payload: &format!("dispatch attempt={attempt} pid={pid}"),
                    ts: started_at,
                });
                self.slots.push(RunSlot {
                    identifier: issue.identifier.clone(),
                    workspace: workspace.display().to_string(),
                    issue,
                    handle: Some(handle),
                    attempt,
                    run_id,
                    started_at,
                    claim_id,
                    last_event_at,
                });
            }
            Err(e) => {
                let msg = format!("{e:#}");
                logging::ev(&issue.identifier, "spawn_error", &msg);
                let _ = self.state.store.insert_event(&NewEvent {
                    run_id: Some(&run_id),
                    issue_identifier: &issue.identifier,
                    kind: "lifecycle",
                    payload: &format!("spawn_error {msg}"),
                    ts: started_at,
                });
                self.schedule_backoff_after_render_failure(
                    &issue,
                    attempt,
                    &run_id,
                    started_at,
                    "spawn error",
                );
                self.claims.remove(&issue.id);
            }
        }
    }

    /// A pre-spawn failure (render/workspace/spawn) is an abnormal attempt.
    /// Persists a runs row so the failure is visible in dashboard history and
    /// counted by the park barrier, then schedules a backoff retry up to
    /// max_retries (or logs Failed + parks the issue when retries are exhausted).
    fn schedule_backoff_after_render_failure(
        &mut self,
        issue: &Issue,
        attempt: u32,
        run_id: &str,
        started_at: DateTime<Utc>,
        err: &str,
    ) {
        let ended_at = Utc::now();
        let max = self.effective_cfg.max_retries;
        let outcome = if attempt >= max {
            RunStatus::Failed
        } else {
            RunStatus::DispatchFailed
        };
        // Insert the run row so this attempt is visible in history and
        // counted by consecutive_completed_runs (park barrier).
        if let Err(e) = self.state.store.insert_run(&NewRun {
            run_id,
            issue_id: &issue.id,
            issue_identifier: &issue.identifier,
            workspace: "",
            profile_json: None,
            workflow_path: Some(&self.paths.workflow_md().display().to_string()),
            workflow_sha: None,
            pid: 0,
            worker_id: Some("orchestrator"),
            started_at,
        }) {
            tracing::warn!(issue = %issue.identifier, "insert_run (dispatch_failed) SQLite write failed: {e:#}");
        }
        let _ = self.state.store.finish_run(
            run_id,
            &RunFinish {
                outcome,
                exit_code: None,
                finished_at: ended_at,
            },
        );
        self.state.history.push(HistoryEntry {
            identifier: issue.identifier.clone(),
            status: outcome,
            pid: 0,
            ended_at,
            note: err.to_string(),
        });
        if attempt >= max {
            logging::ev(
                &issue.identifier,
                "failed",
                &format!("{err}; retries exhausted"),
            );
            self.park_issue_for_safety(issue, &format!("{err}; retries exhausted"));
            return;
        }
        let next = attempt + 1;
        let delay = backoff(self.effective_cfg.retry_backoff_ms, next);
        let due = Utc::now() + chrono::Duration::from_std(delay).unwrap();
        self.retries.push(Retry {
            identifier: issue.identifier.clone(),
            attempt: next,
            due_at: due,
            last_error: err.to_string(),
            continuation: false,
        });
    }

    /// Handle a dashboard control message. Mutates RUN state only.
    async fn handle_control(&mut self, msg: ControlMsg) {
        match msg {
            ControlMsg::Stop => {
                logging::ev("-", "control", "stop: cancelling active runs");
                self.persist_system_event("control stop: cancelling active runs");
                self.kill_all(KillReason::OperatorStop).await;
                self.slots.clear();
            }
            ControlMsg::Pause => {
                self.state.paused.store(true, Ordering::SeqCst);
                logging::ev("-", "control", "paused");
                self.persist_system_event("control paused");
            }
            ControlMsg::Resume => {
                self.state.paused.store(false, Ordering::SeqCst);
                logging::ev("-", "control", "resumed");
                self.persist_system_event("control resumed");
            }
            ControlMsg::Tick { reply } => {
                logging::ev("-", "control", "manual tick");
                self.persist_system_event("control manual tick");
                self.tick().await;
                let _ = reply.send(ControlReply::ok("tick complete"));
            }
            ControlMsg::Claim { identifier, reply } => {
                let result = self.control_claim(&identifier).await;
                let _ = reply.send(result);
            }
            ControlMsg::Release { run_id, reply } => {
                let result = self
                    .control_finish(&run_id, RunStatus::Released, "released")
                    .await;
                let _ = reply.send(result);
            }
            ControlMsg::Interrupt { run_id, reply } => {
                let result = self
                    .control_finish(&run_id, RunStatus::Interrupted, "interrupted")
                    .await;
                let _ = reply.send(result);
            }
            ControlMsg::Kill { run_id, reply } => {
                let result = self.control_kill(&run_id).await;
                let _ = reply.send(result);
            }
        }
    }

    async fn control_claim(&mut self, identifier: &str) -> ControlReply {
        if self.slots.len() >= self.effective_cfg.max_concurrent {
            return ControlReply::err("concurrency limit reached");
        }
        match self.tracker.fetch_one(identifier) {
            Ok(Some(issue)) => {
                if self.claims.contains(&issue.id) {
                    return ControlReply::err("issue already claimed");
                }
                self.try_dispatch(issue, 0).await;
                self.publish_snapshots(&[]).await;
                ControlReply::ok("claim dispatched")
            }
            Ok(None) => ControlReply::err("issue not found"),
            Err(e) => ControlReply::err(format!("fetch issue failed: {e:#}")),
        }
    }

    async fn control_finish(
        &mut self,
        run_id: &str,
        status: RunStatus,
        note: &str,
    ) -> ControlReply {
        let Some(idx) = self.slots.iter().position(|slot| slot.run_id == run_id) else {
            return ControlReply::err("run is not active");
        };
        let mut slot = self.slots.remove(idx);
        let pid = slot.handle.as_ref().map(|h| h.pid()).unwrap_or(0);
        if let Some(handle) = slot.handle.take() {
            let _ = handle.request_kill_and_wait(KillReason::OperatorStop).await;
        }
        self.record_history(
            &slot.run_id,
            &slot.identifier,
            &slot.issue.id,
            &slot.issue,
            Path::new(&slot.workspace),
            status,
            pid,
            note,
            None,
            slot.claim_id,
        );
        self.publish_snapshots(&[]).await;
        ControlReply::ok(note)
    }

    async fn control_kill(&mut self, run_id: &str) -> ControlReply {
        let Some(idx) = self.slots.iter().position(|slot| slot.run_id == run_id) else {
            return ControlReply::err("run is not active");
        };
        let mut slot = self.slots.remove(idx);
        let pid = slot.handle.as_ref().map(|h| h.pid()).unwrap_or(0);
        if let Some(handle) = slot.handle.take() {
            let _ = handle.request_kill_and_wait(KillReason::OperatorStop).await;
        }
        let mut notes = Vec::new();
        if let Some(cmd) = self.effective_cfg.hooks.before_remove.as_deref() {
            if let Err(e) = run_before_remove(cmd, &slot.workspace, &slot.identifier, &slot.run_id)
            {
                let message = format!("before_remove failed: {e:#}");
                self.persist_system_event(&message);
                notes.push(message);
            }
        }
        if let Err(e) = std::fs::remove_dir_all(&slot.workspace) {
            if std::path::Path::new(&slot.workspace).exists() {
                let message = format!("workspace remove failed: {e}");
                self.persist_system_event(&message);
                notes.push(message);
            }
        }
        let note = if notes.is_empty() {
            "killed".to_string()
        } else {
            format!("killed; {}", notes.join("; "))
        };
        self.record_history(
            &slot.run_id,
            &slot.identifier,
            &slot.issue.id,
            &slot.issue,
            Path::new(&slot.workspace),
            RunStatus::Killed,
            pid,
            &note,
            None,
            slot.claim_id,
        );
        self.publish_snapshots(&[]).await;
        if notes.is_empty() {
            ControlReply::ok("killed")
        } else {
            ControlReply::err(note)
        }
    }

    fn persist_system_event(&self, payload: &str) {
        let _ = self.state.store.insert_event(&NewEvent {
            run_id: None,
            issue_identifier: "-",
            kind: "lifecycle",
            payload,
            ts: Utc::now(),
        });
    }

    fn park_issue_for_safety(&self, issue: &Issue, reason: &str) -> bool {
        let comment = format!(
            "Parking this issue in Needs Human.\n\nReason: {reason}\n\nThis is an orchestrator safety write; normal progress updates remain worker-owned."
        );
        match self.tracker.park_issue_needs_human(issue, &comment) {
            Ok(()) => {
                logging::ev(&issue.identifier, "parked", reason);
                self.hitl.notify(HitlNotification::new(
                    "park",
                    issue.identifier.clone(),
                    reason.to_string(),
                ));
                let _ = self.state.store.insert_event(&NewEvent {
                    run_id: None,
                    issue_identifier: &issue.identifier,
                    kind: "lifecycle",
                    payload: &format!("parked needs-human: {reason}"),
                    ts: Utc::now(),
                });
                true
            }
            Err(e) => {
                logging::ev(
                    &issue.identifier,
                    "park_error",
                    &format!("needs-human safety write failed: {e:#}"),
                );
                let _ = self.state.store.insert_event(&NewEvent {
                    run_id: None,
                    issue_identifier: &issue.identifier,
                    kind: "lifecycle",
                    payload: &format!("park_error needs-human safety write failed: {e:#}"),
                    ts: Utc::now(),
                });
                false
            }
        }
    }

    fn park_if_active_run_barrier_reached(&self, issue: &Issue) -> bool {
        let max_active_runs = self.effective_cfg.max_active_runs;
        if max_active_runs == 0 {
            return false;
        }
        let consecutive = match self
            .state
            .store
            .consecutive_completed_runs(&issue.id, max_active_runs + 1)
        {
            Ok(count) => count,
            Err(e) => {
                logging::ev(
                    &issue.identifier,
                    "park_barrier_error",
                    &format!("counting completed active runs failed: {e:#}"),
                );
                return false;
            }
        };
        if consecutive < max_active_runs {
            return false;
        }

        let reason = format!(
            "too many consecutive completed runs without leaving active state ({consecutive}/{max_active_runs})"
        );
        if self.park_issue_for_safety(issue, &reason) {
            self.record_park_barrier(issue, &reason);
        }
        true
    }

    fn record_park_barrier(&self, issue: &Issue, reason: &str) {
        let now = Utc::now();
        let run_id = new_run_id(&issue.identifier, &now);
        let workflow_path = self.paths.workflow_md().display().to_string();
        if let Err(e) = self.state.store.insert_run(&NewRun {
            run_id: &run_id,
            issue_id: &issue.id,
            issue_identifier: &issue.identifier,
            workspace: "",
            profile_json: None,
            workflow_path: Some(&workflow_path),
            workflow_sha: None,
            pid: 0,
            worker_id: Some("orchestrator"),
            started_at: now,
        }) {
            tracing::warn!(issue = %issue.identifier, "insert park_barrier run failed: {e:#}");
            return;
        }
        let _ = self.state.store.finish_run(
            &run_id,
            &RunFinish {
                outcome: RunStatus::ParkBarrier,
                exit_code: None,
                finished_at: now,
            },
        );
        self.state.history.push(HistoryEntry {
            identifier: issue.identifier.clone(),
            status: RunStatus::ParkBarrier,
            pid: 0,
            ended_at: now,
            note: reason.to_string(),
        });
        let _ = self.state.store.insert_event(&NewEvent {
            run_id: Some(&run_id),
            issue_identifier: &issue.identifier,
            kind: "lifecycle",
            payload: &format!("park_barrier {reason}"),
            ts: now,
        });
    }

    /// Record a finished run: update the in-memory history ring, write outcome
    /// to SQLite, release the claim, and persist a terminal lifecycle event.
    #[allow(clippy::too_many_arguments)]
    fn record_history(
        &mut self,
        run_id: &str,
        identifier: &str,
        issue_id: &str,
        issue: &Issue,
        workspace: &Path,
        status: RunStatus,
        pid: u32,
        note: &str,
        exit_code: Option<i32>,
        claim_id: Option<i64>,
    ) {
        let ended_at = Utc::now();
        if status != RunStatus::Killed {
            if let Err(e) = self.run_lifecycle_hook("after_run", issue, workspace) {
                logging::ev(
                    identifier,
                    "hook_error",
                    &format!("after_run failed: {e:#}"),
                );
            }
        }
        self.state.history.push(HistoryEntry {
            identifier: identifier.to_string(),
            status,
            pid,
            ended_at,
            note: note.to_string(),
        });
        if let Err(e) = self.state.store.finish_run(
            run_id,
            &RunFinish {
                outcome: status,
                exit_code,
                finished_at: ended_at,
            },
        ) {
            tracing::warn!(issue = %identifier, "finish_run SQLite write failed: {e:#}");
        }
        if let Some(cid) = claim_id {
            if let Err(e) = self.state.store.release_claim(cid, ended_at) {
                tracing::warn!(issue = %identifier, "release_claim SQLite write failed: {e:#}");
            }
        }
        self.claims.remove(issue_id);
        let _ = self.state.store.insert_event(&NewEvent {
            run_id: Some(run_id),
            issue_identifier: identifier,
            kind: "lifecycle",
            payload: &format!("{:?} {note}", status),
            ts: ended_at,
        });
        self.cleanup_workspace_if_needed(issue, workspace, status);
    }

    fn run_lifecycle_hook(
        &self,
        name: &str,
        issue: &Issue,
        workspace: &Path,
    ) -> anyhow::Result<()> {
        let script = match name {
            "after_create" => self.effective_cfg.hooks.after_create.as_deref(),
            "before_run" => self.effective_cfg.hooks.before_run.as_deref(),
            "after_run" => self.effective_cfg.hooks.after_run.as_deref(),
            "before_remove" => self.effective_cfg.hooks.before_remove.as_deref(),
            _ => None,
        };
        let Some(script) = script.filter(|s| !s.trim().is_empty()) else {
            return Ok(());
        };
        let project_id = issue
            .project_slug
            .as_deref()
            .or(self.effective_cfg.tracker_project_slug.as_deref())
            .unwrap_or(&self.agent_cfg.id);
        let mut command = Command::new("sh");
        dotenv::scrub_loaded_env(&mut command);
        let status = command
            .arg("-c")
            .arg(script)
            .current_dir(workspace)
            .env("AGENT_PROJECT_ID", project_id)
            .env("AGENT_ISSUE_ID", &issue.id)
            .env("AGENT_ISSUE_IDENTIFIER", &issue.identifier)
            .env("AGENT_WORKSPACE", workspace)
            .env_remove("LINEAR_API_KEY")
            .status()
            .with_context(|| format!("running {name} hook"))?;
        if !status.success() {
            anyhow::bail!("{name} hook exited with {status}");
        }
        Ok(())
    }

    fn cleanup_workspace_if_needed(&self, issue: &Issue, workspace: &Path, outcome: RunStatus) {
        if !self.effective_cfg.cleanup_on_terminal {
            return;
        }
        if !matches!(
            outcome,
            RunStatus::Terminal | RunStatus::HookFailed | RunStatus::DispatchFailed
        ) {
            return;
        }
        if let Err(e) = self.run_lifecycle_hook("before_remove", issue, workspace) {
            logging::ev(
                &issue.identifier,
                "hook_error",
                &format!("before_remove failed: {e:#}"),
            );
            return;
        }
        if let Err(e) = std::fs::remove_dir_all(workspace) {
            if workspace.exists() {
                logging::ev(
                    &issue.identifier,
                    "workspace_cleanup_error",
                    &format!("removing {}: {e}", workspace.display()),
                );
            }
        }
    }

    /// Request-kill every running child for the given reason.
    async fn kill_all(&mut self, reason: KillReason) {
        let make = |r: &KillReason| match r {
            KillReason::Timeout => KillReason::Timeout,
            KillReason::OperatorStop => KillReason::OperatorStop,
            KillReason::Reconcile => KillReason::Reconcile,
        };
        for slot in self.slots.iter_mut() {
            let pid = slot.handle.as_ref().map(|h| h.pid()).unwrap_or(0);
            if let Some(handle) = slot.handle.take() {
                handle.request_kill(make(&reason));
                let ended_at = Utc::now();
                self.state.history.push(HistoryEntry {
                    identifier: slot.identifier.clone(),
                    status: RunStatus::Killed,
                    pid,
                    ended_at,
                    note: "operator stop / shutdown".to_string(),
                });
                if let Err(e) = self.state.store.finish_run(
                    &slot.run_id,
                    &RunFinish {
                        outcome: RunStatus::Killed,
                        exit_code: None,
                        finished_at: ended_at,
                    },
                ) {
                    tracing::warn!(issue = %slot.identifier, "finish_run (kill_all) SQLite write failed: {e:#}");
                }
                if let Some(cid) = slot.claim_id {
                    if let Err(e) = self.state.store.release_claim(cid, ended_at) {
                        tracing::warn!(issue = %slot.identifier, "release_claim (kill_all) SQLite write failed: {e:#}");
                    }
                }
                self.claims.remove(&slot.issue.id);
                let _ = self.state.store.insert_event(&NewEvent {
                    run_id: Some(&slot.run_id),
                    issue_identifier: &slot.identifier,
                    kind: "lifecycle",
                    payload: "Killed operator stop / shutdown",
                    ts: ended_at,
                });
            }
        }
    }

    fn heartbeat_active_runs(&self) {
        let now = Utc::now();
        for slot in &self.slots {
            if slot
                .handle
                .as_ref()
                .map(|h| !h.is_finished())
                .unwrap_or(false)
            {
                let _ = self.state.store.insert_heartbeat(&NewHeartbeat {
                    run_id: &slot.run_id,
                    issue_identifier: &slot.identifier,
                    worker_id: "orchestrator",
                    ts: now,
                });
            }
        }
        let _ = self.state.store.prune_finished_run_heartbeats();
    }

    /// Write the active-run, queue, and retry snapshots to `AppState` for the
    /// dashboard. `candidates` is the list already fetched once this tick.
    async fn publish_snapshots(&self, candidates: &[Issue]) {
        let active_runs: Vec<ActiveRun> = self
            .slots
            .iter()
            .map(|slot| {
                let last_event = self.last_event_line(&slot.identifier).unwrap_or_default();
                ActiveRun {
                    run_id: slot.run_id.clone(),
                    identifier: slot.identifier.clone(),
                    state: slot.issue.state.clone(),
                    workspace: slot.workspace.clone(),
                    pid: slot.handle.as_ref().map(|h| h.pid()).unwrap_or(0),
                    started_at: slot.started_at,
                    last_event,
                    status: RunStatus::Running,
                }
            })
            .collect();
        // Legacy single-active field retained for callers that only display one.
        let active = active_runs.first().cloned();
        {
            let mut guard = self.state.active.write().await;
            *guard = active;
        }
        {
            let mut guard = self.state.active_runs.write().await;
            *guard = active_runs;
        }

        let busy: HashSet<String> = self.slots.iter().map(|s| s.identifier.clone()).collect();
        let retry_ids: HashSet<String> =
            self.retries.iter().map(|r| r.identifier.clone()).collect();
        let mut queue_items: Vec<QueueItem> = {
            let mut v: Vec<Issue> = candidates
                .iter()
                .filter(|i| !busy.contains(&i.identifier) && !retry_ids.contains(&i.identifier))
                .cloned()
                .collect();
            if self.tracker.sort_candidates_locally() {
                sort_candidates(&mut v);
            }
            v.into_iter()
                .map(|i| QueueItem {
                    identifier: i.identifier,
                    title: i.title,
                    state: i.state,
                    priority: i.priority,
                    created_at: i.created_at,
                })
                .collect()
        };
        queue_items.truncate(50);
        {
            let mut guard = self.state.queue.write().await;
            *guard = queue_items;
        }

        let retry_items: Vec<RetryItem> = self
            .retries
            .iter()
            .map(|r| RetryItem {
                identifier: r.identifier.clone(),
                attempt: r.attempt,
                due_at: r.due_at,
                last_error: r.last_error.clone(),
            })
            .collect();
        {
            let mut guard = self.state.retry.write().await;
            *guard = retry_items;
        }

        // Update rate-limit min-remaining from the tracker (LinearTracker updates
        // this on each API call; FileTracker returns None).
        if let Some(remaining) = self.tracker.rate_limit_remaining() {
            self.state
                .rate_limit_min_remaining
                .fetch_min(remaining, std::sync::atomic::Ordering::SeqCst);
        }
        {
            let mut guard = self.state.last_tick_at.write().await;
            *guard = Some(Utc::now());
        }
        self.state
            .version_tx
            .send_modify(|v| *v = v.wrapping_add(1));
    }

    fn last_event_line(&self, identifier: &str) -> Option<String> {
        let prefix = format!("child[{identifier}]:");
        self.state
            .events
            .snapshot()
            .into_iter()
            .rev()
            .find(|l| l.starts_with(&prefix))
    }
}

#[cfg(test)]
fn default_runner() -> Arc<dyn cap_runner::Runner> {
    default_runner_services()
        .get_named::<dyn cap_runner::Runner>("pi")
        .expect("pi runner is registered")
}

#[cfg(test)]
fn default_runner_services() -> host_api::ServiceRegistry {
    let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let mut ctx = host_api::RegisterCtx {
        bus: host_api::EventBus::new(),
        http: host_api::HttpRegistry::disabled(),
        services: host_api::ServiceRegistry::default(),
        paths: host_api::HostPaths::new(".").expect("current directory is canonicalizable"),
        config: host_api::ConfigStore::default(),
        shutdown: host_api::ShutdownToken::new(shutdown_rx),
    };
    for extension in [
        &runner_pi::PiRunnerExtension as &dyn host_api::Extension,
        &runner_claude::ClaudeRunnerExtension,
        &runner_codex::CodexRunnerExtension,
        &runner_cli::CliRunnerExtension,
        &runner_fake::FakeRunnerExtension,
    ] {
        futures::executor::block_on(extension.register(&mut ctx)).expect("runner registers");
    }
    ctx.services
}

fn runner_service_id(raw: &str) -> &str {
    if raw.trim().is_empty() {
        "pi"
    } else {
        raw
    }
}

/// Sort candidates: priority asc (null last), then `created_at` asc (null last),
/// then identifier asc.
pub(crate) fn sort_candidates(v: &mut [Issue]) {
    v.sort_by(|a, b| {
        let pa = a.priority;
        let pb = b.priority;
        let prio = match (pa, pb) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        };
        if prio != std::cmp::Ordering::Equal {
            return prio;
        }
        let ca = a.created_at;
        let cb = b.created_at;
        let created = match (ca, cb) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        };
        if created != std::cmp::Ordering::Equal {
            return created;
        }
        a.identifier.cmp(&b.identifier)
    });
}

/// Backoff for dispatch retries: `min(retry_backoff_ms * 2^attempt, 30min)`.
/// `attempt` is 1-based (first retry = 1).
pub(crate) fn backoff(retry_backoff_ms: u64, attempt: u32) -> Duration {
    let factor = 1u64.checked_shl(attempt).unwrap_or(u64::MAX);
    let ms = retry_backoff_ms.saturating_mul(factor);
    let d = Duration::from_millis(ms);
    if d > BACKOFF_CAP {
        BACKOFF_CAP
    } else {
        d
    }
}

fn run_before_remove(
    command: &str,
    workspace: &str,
    identifier: &str,
    run_id: &str,
) -> anyhow::Result<()> {
    let mut hook = std::process::Command::new("sh");
    dotenv::scrub_loaded_env(&mut hook);
    let status = hook
        .arg("-c")
        .arg(command)
        .env("AGENT_WORKSPACE", workspace)
        .env("AGENT_ISSUE_IDENTIFIER", identifier)
        .env("AGENT_RUN_ID", run_id)
        .status()
        .map_err(|e| anyhow::anyhow!("running before_remove hook: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow::anyhow!("before_remove exited with {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AgentConfig, DashboardConfig, HitlConfig, OrchestratorConfig, RunnerConfig, TrackerConfig,
        TrackerInner, WorkspaceConfig,
    };
    use crate::paths::AgentPaths;
    use crate::prompt::PromptRenderer;
    use crate::state::{AgentInfo, AppState};
    use crate::store::{Store, ACTIVE_CONTINUATION_EVENT};
    use crate::tracker::FileTracker;
    use crate::tracker::Tracker;
    use crate::workflow_config::{EffectiveLoopConfig, WorkflowFrontmatter};
    use anyhow::Result;
    use chrono::TimeZone;
    use std::net::{IpAddr, Ipv4Addr};
    use tempfile::tempdir;
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use tokio::sync::oneshot;

    fn issue(id: &str, prio: Option<i32>, created: Option<i64>) -> Issue {
        Issue::builder(id, id, id, "todo")
            .priority(prio)
            .created_at(created.map(|s| Utc.timestamp_opt(s, 0).unwrap()))
            .build()
    }

    fn finished_handle_for_test(pid: u32, kind: ExitKind) -> RunnerHandle {
        let (kill_tx, _kill_rx) = oneshot::channel::<KillReason>();
        let done = tokio::spawn(async move { kind });
        RunnerHandle::new(pid, kill_tx, done)
    }

    fn pending_handle_for_test(pid: u32, kind: ExitKind) -> RunnerHandle {
        let (kill_tx, kill_rx) = oneshot::channel::<KillReason>();
        let done = tokio::spawn(async move {
            let _ = kill_rx.await;
            kind
        });
        RunnerHandle::new(pid, kill_tx, done)
    }

    struct StaticTracker {
        issue: Issue,
        parks: Option<Arc<Mutex<Vec<String>>>>,
    }

    impl Tracker for StaticTracker {
        fn poll_candidates(&self) -> Result<Vec<Issue>> {
            Ok(Vec::new())
        }

        fn fetch_states(&self, ids: &[String]) -> Result<Vec<Issue>> {
            Ok(ids
                .iter()
                .filter(|id| id.as_str() == self.issue.identifier)
                .map(|_| self.issue.clone())
                .collect())
        }

        fn fetch_terminal(&self) -> Result<Vec<Issue>> {
            Ok(Vec::new())
        }

        fn fetch_one(&self, id: &str) -> Result<Option<Issue>> {
            if id == self.issue.identifier {
                Ok(Some(self.issue.clone()))
            } else {
                Ok(None)
            }
        }

        fn park_issue_needs_human(&self, _issue: &Issue, comment: &str) -> Result<()> {
            if let Some(parks) = &self.parks {
                parks.lock().unwrap().push(comment.to_string());
            }
            Ok(())
        }
    }

    struct MissingTracker;

    impl Tracker for MissingTracker {
        fn poll_candidates(&self) -> Result<Vec<Issue>> {
            Ok(Vec::new())
        }

        fn fetch_states(&self, _ids: &[String]) -> Result<Vec<Issue>> {
            Ok(Vec::new())
        }

        fn fetch_terminal(&self) -> Result<Vec<Issue>> {
            Ok(Vec::new())
        }

        fn fetch_one(&self, _id: &str) -> Result<Option<Issue>> {
            Ok(None)
        }
    }

    struct CandidateTracker {
        issue: Issue,
        parks: Arc<Mutex<Vec<String>>>,
        park_ok: bool,
    }

    impl Tracker for CandidateTracker {
        fn poll_candidates(&self) -> Result<Vec<Issue>> {
            Ok(vec![self.issue.clone()])
        }

        fn fetch_states(&self, ids: &[String]) -> Result<Vec<Issue>> {
            Ok(ids
                .iter()
                .filter(|id| id.as_str() == self.issue.identifier || id.as_str() == self.issue.id)
                .map(|_| self.issue.clone())
                .collect())
        }

        fn fetch_terminal(&self) -> Result<Vec<Issue>> {
            Ok(Vec::new())
        }

        fn fetch_one(&self, id: &str) -> Result<Option<Issue>> {
            if id == self.issue.identifier || id == self.issue.id {
                Ok(Some(self.issue.clone()))
            } else {
                Ok(None)
            }
        }

        fn park_issue_needs_human(&self, _issue: &Issue, comment: &str) -> Result<()> {
            if !self.park_ok {
                anyhow::bail!("simulated parking failure");
            }
            self.parks.lock().unwrap().push(comment.to_string());
            Ok(())
        }
    }

    fn test_agent_config() -> AgentConfig {
        AgentConfig {
            id: "test-agent".to_string(),
            name: "Test Agent".to_string(),
            tracker: TrackerConfig {
                use_: "files".to_string(),
                config: Some(TrackerInner {
                    path: "issues".into(),
                }),
                active_states: vec!["todo".to_string()],
                terminal_states: vec!["done".to_string()],
                project_slug: None,
                endpoint: None,
                needs_human: None,
            },
            runner: RunnerConfig {
                use_: "claude-code".to_string(),
                command: "claude".to_string(),
                model: None,
                max_run_timeout_ms: 1000,
                stall_timeout_ms: 300_000,
            },
            orchestrator: OrchestratorConfig {
                poll_interval_ms: 100,
                max_concurrent: 1,
                max_active_runs: 3,
                max_retries: 3,
                retry_backoff_ms: 1000,
            },
            hitl: HitlConfig::default(),
            workspace: WorkspaceConfig {
                root: "workspaces".into(),
            },
            dashboard: DashboardConfig {
                bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 7878,
                webhook_secret: None,
            },
        }
    }

    #[tokio::test]
    async fn abnormal_exit_with_needs_human_state_releases_without_retry() {
        let temp = TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join("logs")).unwrap();
        std::fs::write(temp.path().join("WORKFLOW.md"), "Do {{ issue.title }}").unwrap();

        let mut needs_issue = issue("ISSUE-1", None, None);
        needs_issue.state = "needs_human".to_string();
        let tracker = Arc::new(StaticTracker {
            issue: needs_issue.clone(),
            parks: None,
        });
        let agent_cfg = test_agent_config();
        let mut effective_cfg =
            EffectiveLoopConfig::merge(&agent_cfg, &WorkflowFrontmatter::default());
        effective_cfg.needs_human = Some("needs_human".to_string());

        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let store = Arc::new(Store::open(&temp.path().join("store.db")).unwrap());
        let state = AppState::new(
            AgentInfo {
                id: "test-agent".to_string(),
                folder: temp.path().display().to_string(),
                tracker: "files".to_string(),
                runner: "claude-code".to_string(),
            },
            control_tx,
            Arc::clone(&store),
            Vec::new(),
        );
        let prompt = PromptRenderer::load(&temp.path().join("WORKFLOW.md")).unwrap();
        let mut orchestrator = Orchestrator::new(
            agent_cfg,
            AgentPaths::new(temp.path().to_path_buf()),
            tracker,
            prompt,
            effective_cfg,
            state.clone(),
            control_rx,
        );
        let started_at = Utc::now();
        let run_id = new_run_id(&needs_issue.identifier, &started_at);
        store
            .insert_run(&NewRun {
                run_id: &run_id,
                issue_id: &needs_issue.id,
                issue_identifier: &needs_issue.identifier,
                workspace: "workspace",
                profile_json: None,
                workflow_path: None,
                workflow_sha: None,
                pid: 42,
                worker_id: None,
                started_at,
            })
            .unwrap();
        let claim_id = store
            .insert_claim(&NewClaim {
                run_id: &run_id,
                issue_id: &needs_issue.id,
                issue_identifier: &needs_issue.identifier,
                worker_id: "orchestrator",
                claimed_at: started_at,
            })
            .ok();
        orchestrator.slots.push(RunSlot {
            identifier: needs_issue.identifier.clone(),
            issue: needs_issue,
            workspace: "workspace".to_string(),
            handle: Some(finished_handle_for_test(42, ExitKind::Abnormal(Some(17)))),
            attempt: 1,
            run_id: run_id.clone(),
            started_at,
            claim_id,
            last_event_at: Arc::new(Mutex::new(started_at)),
        });
        tokio::task::yield_now().await;

        orchestrator.collect_finished().await;

        assert!(orchestrator.retries.is_empty());
        let history = state.history.snapshot();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].identifier, "ISSUE-1");
        assert_eq!(history[0].status, RunStatus::NeedsHuman);
        assert_eq!(history[0].note, "needs-human after abnormal exit");
        assert_eq!(store.claim_release_count_for_run(&run_id).unwrap(), (1, 1));
        let runs = store.list_runs_paged(0, 10).unwrap();
        assert_eq!(runs[0].outcome.as_deref(), Some("needs_human"));
        assert_eq!(runs[0].exit_code, Some(17));
    }

    #[tokio::test]
    async fn abnormal_exit_without_needs_human_config_and_missing_tracker_state_retries() {
        let temp = TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join("logs")).unwrap();
        std::fs::write(temp.path().join("WORKFLOW.md"), "Do {{ issue.title }}").unwrap();

        let active_issue = issue("ISSUE-1", None, None);
        let tracker = Arc::new(MissingTracker);
        let agent_cfg = test_agent_config();
        let mut effective_cfg =
            EffectiveLoopConfig::merge(&agent_cfg, &WorkflowFrontmatter::default());
        effective_cfg.needs_human = None;

        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let store = Arc::new(Store::open(&temp.path().join("store.db")).unwrap());
        let state = AppState::new(
            AgentInfo {
                id: "test-agent".to_string(),
                folder: temp.path().display().to_string(),
                tracker: "files".to_string(),
                runner: "claude-code".to_string(),
            },
            control_tx,
            Arc::clone(&store),
            Vec::new(),
        );
        let prompt = PromptRenderer::load(&temp.path().join("WORKFLOW.md")).unwrap();
        let mut orchestrator = Orchestrator::new(
            agent_cfg,
            AgentPaths::new(temp.path().to_path_buf()),
            tracker,
            prompt,
            effective_cfg,
            state.clone(),
            control_rx,
        );
        let started_at = Utc::now();
        let run_id = new_run_id(&active_issue.identifier, &started_at);
        orchestrator.slots.push(RunSlot {
            identifier: active_issue.identifier.clone(),
            issue: active_issue,
            workspace: "workspace".to_string(),
            handle: Some(finished_handle_for_test(42, ExitKind::Abnormal(None))),
            attempt: 1,
            run_id,
            started_at,
            claim_id: None,
            last_event_at: Arc::new(Mutex::new(started_at)),
        });
        tokio::task::yield_now().await;

        orchestrator.collect_finished().await;

        assert_eq!(orchestrator.retries.len(), 1);
        assert_eq!(orchestrator.retries[0].identifier, "ISSUE-1");
        assert_eq!(orchestrator.retries[0].attempt, 2);
        assert!(!orchestrator.retries[0].continuation);
        assert!(state.history.snapshot().is_empty());
    }

    #[tokio::test]
    async fn normal_exit_still_active_records_completed_without_parking() {
        let temp = TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join("logs")).unwrap();
        std::fs::write(temp.path().join("WORKFLOW.md"), "Do {{ issue.title }}").unwrap();

        let active_issue = issue("ISSUE-1", None, None);
        let parks = Arc::new(Mutex::new(Vec::new()));
        let tracker = Arc::new(StaticTracker {
            issue: active_issue.clone(),
            parks: Some(Arc::clone(&parks)),
        });
        let agent_cfg = test_agent_config();
        let effective_cfg = EffectiveLoopConfig::merge(&agent_cfg, &WorkflowFrontmatter::default());

        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let store = Arc::new(Store::open(&temp.path().join("store.db")).unwrap());
        let state = AppState::new(
            AgentInfo {
                id: "test-agent".to_string(),
                folder: temp.path().display().to_string(),
                tracker: "files".to_string(),
                runner: "claude-code".to_string(),
            },
            control_tx,
            Arc::clone(&store),
            Vec::new(),
        );
        let prompt = PromptRenderer::load(&temp.path().join("WORKFLOW.md")).unwrap();
        let mut orchestrator = Orchestrator::new(
            agent_cfg,
            AgentPaths::new(temp.path().to_path_buf()),
            tracker,
            prompt,
            effective_cfg,
            state.clone(),
            control_rx,
        );
        let started_at = Utc::now();
        let run_id = new_run_id(&active_issue.identifier, &started_at);
        store
            .insert_run(&NewRun {
                run_id: &run_id,
                issue_id: &active_issue.id,
                issue_identifier: &active_issue.identifier,
                workspace: "workspace",
                profile_json: None,
                workflow_path: None,
                workflow_sha: None,
                pid: 42,
                worker_id: None,
                started_at,
            })
            .unwrap();
        orchestrator.slots.push(RunSlot {
            identifier: active_issue.identifier.clone(),
            issue: active_issue.clone(),
            workspace: "workspace".to_string(),
            handle: Some(finished_handle_for_test(42, ExitKind::Normal)),
            attempt: 0,
            run_id: run_id.clone(),
            started_at,
            claim_id: None,
            last_event_at: Arc::new(Mutex::new(started_at)),
        });
        tokio::task::yield_now().await;

        orchestrator.collect_finished().await;

        assert_eq!(orchestrator.retries.len(), 1);
        assert!(parks.lock().unwrap().is_empty());
        assert_eq!(
            store
                .consecutive_completed_runs(&active_issue.id, 10)
                .unwrap(),
            1
        );
        let history = state.history.snapshot();
        assert_eq!(history[0].status, RunStatus::Succeeded);
    }

    #[tokio::test]
    async fn turn_timeout_records_interrupted_without_retry() {
        let temp = TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join("logs")).unwrap();
        std::fs::write(temp.path().join("WORKFLOW.md"), "Do {{ issue.title }}").unwrap();

        let active_issue = issue("ISSUE-1", None, None);
        let tracker = Arc::new(MissingTracker);
        let agent_cfg = test_agent_config();
        let effective_cfg = EffectiveLoopConfig::merge(&agent_cfg, &WorkflowFrontmatter::default());
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let store = Arc::new(Store::open(&temp.path().join("store.db")).unwrap());
        let state = AppState::new(
            AgentInfo {
                id: "test-agent".to_string(),
                folder: temp.path().display().to_string(),
                tracker: "files".to_string(),
                runner: "claude".to_string(),
            },
            control_tx,
            Arc::clone(&store),
            Vec::new(),
        );
        let prompt = PromptRenderer::load(&temp.path().join("WORKFLOW.md")).unwrap();
        let mut orchestrator = Orchestrator::new(
            agent_cfg,
            AgentPaths::new(temp.path().to_path_buf()),
            tracker,
            prompt,
            effective_cfg,
            state.clone(),
            control_rx,
        );
        let started_at = Utc::now();
        let run_id = new_run_id(&active_issue.identifier, &started_at);
        store
            .insert_run(&NewRun {
                run_id: &run_id,
                issue_id: &active_issue.id,
                issue_identifier: &active_issue.identifier,
                workspace: "workspace",
                profile_json: None,
                workflow_path: None,
                workflow_sha: None,
                pid: 42,
                worker_id: None,
                started_at,
            })
            .unwrap();
        orchestrator.slots.push(RunSlot {
            identifier: active_issue.identifier.clone(),
            issue: active_issue,
            workspace: "workspace".to_string(),
            handle: Some(finished_handle_for_test(
                42,
                ExitKind::Interrupted {
                    reason: "turn_timeout",
                },
            )),
            attempt: 1,
            run_id: run_id.clone(),
            started_at,
            claim_id: None,
            last_event_at: Arc::new(Mutex::new(started_at)),
        });
        tokio::task::yield_now().await;

        orchestrator.collect_finished().await;

        assert!(orchestrator.retries.is_empty());
        let history = state.history.snapshot();
        assert_eq!(history[0].status, RunStatus::Interrupted);
        assert_eq!(history[0].note, "turn_timeout");
        let runs = store.list_runs_paged(0, 10).unwrap();
        assert_eq!(runs[0].outcome.as_deref(), Some("interrupted"));
    }

    #[tokio::test]
    async fn stale_last_event_records_stalled_and_releases_claim() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("WORKFLOW.md"), "Do {{ issue.title }}").unwrap();

        let active_issue = issue("ISSUE-1", None, None);
        let parks = Arc::new(Mutex::new(Vec::new()));
        let tracker = Arc::new(StaticTracker {
            issue: active_issue.clone(),
            parks: Some(Arc::clone(&parks)),
        });
        let agent_cfg = test_agent_config();
        let mut effective_cfg =
            EffectiveLoopConfig::merge(&agent_cfg, &WorkflowFrontmatter::default());
        effective_cfg.stall_timeout_ms = 1;
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let store = Arc::new(Store::open(&temp.path().join("store.db")).unwrap());
        let state = AppState::new(
            AgentInfo {
                id: "test-agent".to_string(),
                folder: temp.path().display().to_string(),
                tracker: "files".to_string(),
                runner: "claude-code".to_string(),
            },
            control_tx,
            Arc::clone(&store),
            Vec::new(),
        );
        let prompt = PromptRenderer::load(&temp.path().join("WORKFLOW.md")).unwrap();
        let mut orchestrator = Orchestrator::new(
            agent_cfg,
            AgentPaths::new(temp.path().to_path_buf()),
            tracker,
            prompt,
            effective_cfg,
            state.clone(),
            control_rx,
        );
        let started_at = Utc::now();
        let stale_at = started_at - chrono::Duration::seconds(10);
        let run_id = new_run_id(&active_issue.identifier, &started_at);
        store
            .insert_run(&NewRun {
                run_id: &run_id,
                issue_id: &active_issue.id,
                issue_identifier: &active_issue.identifier,
                workspace: "workspace",
                profile_json: None,
                workflow_path: None,
                workflow_sha: None,
                pid: 42,
                worker_id: None,
                started_at,
            })
            .unwrap();
        let claim_id = store
            .insert_claim(&NewClaim {
                run_id: &run_id,
                issue_id: &active_issue.id,
                issue_identifier: &active_issue.identifier,
                worker_id: "orchestrator",
                claimed_at: started_at,
            })
            .ok();
        orchestrator.claims.insert(active_issue.id.clone());
        orchestrator.slots.push(RunSlot {
            identifier: active_issue.identifier.clone(),
            issue: active_issue,
            workspace: "workspace".to_string(),
            handle: Some(pending_handle_for_test(
                42,
                ExitKind::Interrupted { reason: "stalled" },
            )),
            attempt: 0,
            run_id: run_id.clone(),
            started_at,
            claim_id,
            last_event_at: Arc::new(Mutex::new(stale_at)),
        });

        orchestrator.reconcile().await;

        let history = state.history.snapshot();
        assert_eq!(history[0].status, RunStatus::Stalled);
        assert!(orchestrator.claims.is_empty());
        assert_eq!(store.claim_release_count_for_run(&run_id).unwrap(), (1, 1));
        let runs = store.list_runs_paged(0, 10).unwrap();
        assert_eq!(runs[0].outcome.as_deref(), Some("stalled"));
        let parked = parks.lock().unwrap();
        assert_eq!(parked.len(), 1);
        assert!(parked[0].contains("stalled no runner events"));
    }

    #[tokio::test]
    async fn abnormal_exit_with_needs_human_config_and_different_tracker_state_retries() {
        let temp = TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join("logs")).unwrap();
        std::fs::write(temp.path().join("WORKFLOW.md"), "Do {{ issue.title }}").unwrap();

        let active_issue = issue("ISSUE-1", None, None);
        let parks = Arc::new(Mutex::new(Vec::new()));
        let tracker = Arc::new(StaticTracker {
            issue: active_issue.clone(),
            parks: Some(Arc::clone(&parks)),
        });
        let agent_cfg = test_agent_config();
        let mut effective_cfg =
            EffectiveLoopConfig::merge(&agent_cfg, &WorkflowFrontmatter::default());
        effective_cfg.needs_human = Some("stuck".to_string());

        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let store = Arc::new(Store::open(&temp.path().join("store.db")).unwrap());
        let state = AppState::new(
            AgentInfo {
                id: "test-agent".to_string(),
                folder: temp.path().display().to_string(),
                tracker: "files".to_string(),
                runner: "claude-code".to_string(),
            },
            control_tx,
            Arc::clone(&store),
            Vec::new(),
        );
        let prompt = PromptRenderer::load(&temp.path().join("WORKFLOW.md")).unwrap();
        let mut orchestrator = Orchestrator::new(
            agent_cfg,
            AgentPaths::new(temp.path().to_path_buf()),
            tracker,
            prompt,
            effective_cfg,
            state.clone(),
            control_rx,
        );
        let started_at = Utc::now();
        let run_id = new_run_id(&active_issue.identifier, &started_at);
        orchestrator.slots.push(RunSlot {
            identifier: active_issue.identifier.clone(),
            issue: active_issue,
            workspace: "workspace".to_string(),
            handle: Some(finished_handle_for_test(42, ExitKind::Abnormal(None))),
            attempt: 1,
            run_id,
            started_at,
            claim_id: None,
            last_event_at: Arc::new(Mutex::new(started_at)),
        });
        tokio::task::yield_now().await;

        orchestrator.collect_finished().await;

        assert_eq!(orchestrator.retries.len(), 1);
        assert_eq!(orchestrator.retries[0].identifier, "ISSUE-1");
        assert_eq!(orchestrator.retries[0].attempt, 2);
        assert!(!orchestrator.retries[0].continuation);
        assert!(state.history.snapshot().is_empty());
        assert!(parks.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn abnormal_exit_at_retry_cap_parks_issue_needs_human() {
        let temp = TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join("logs")).unwrap();
        std::fs::write(temp.path().join("WORKFLOW.md"), "Do {{ issue.title }}").unwrap();

        let active_issue = issue("ISSUE-1", None, None);
        let parks = Arc::new(Mutex::new(Vec::new()));
        let tracker = Arc::new(StaticTracker {
            issue: active_issue.clone(),
            parks: Some(Arc::clone(&parks)),
        });
        let agent_cfg = test_agent_config();
        let effective_cfg = EffectiveLoopConfig::merge(&agent_cfg, &WorkflowFrontmatter::default());

        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let store = Arc::new(Store::open(&temp.path().join("store.db")).unwrap());
        let state = AppState::new(
            AgentInfo {
                id: "test-agent".to_string(),
                folder: temp.path().display().to_string(),
                tracker: "files".to_string(),
                runner: "claude-code".to_string(),
            },
            control_tx,
            Arc::clone(&store),
            Vec::new(),
        );
        let prompt = PromptRenderer::load(&temp.path().join("WORKFLOW.md")).unwrap();
        let mut orchestrator = Orchestrator::new(
            agent_cfg,
            AgentPaths::new(temp.path().to_path_buf()),
            tracker,
            prompt,
            effective_cfg,
            state.clone(),
            control_rx,
        );
        let started_at = Utc::now();
        let run_id = new_run_id(&active_issue.identifier, &started_at);
        store
            .insert_run(&NewRun {
                run_id: &run_id,
                issue_id: &active_issue.id,
                issue_identifier: &active_issue.identifier,
                workspace: "workspace",
                profile_json: None,
                workflow_path: None,
                workflow_sha: None,
                pid: 42,
                worker_id: None,
                started_at,
            })
            .unwrap();
        orchestrator.slots.push(RunSlot {
            identifier: active_issue.identifier.clone(),
            issue: active_issue,
            workspace: "workspace".to_string(),
            handle: Some(finished_handle_for_test(42, ExitKind::Abnormal(Some(1)))),
            attempt: 3,
            run_id,
            started_at,
            claim_id: None,
            last_event_at: Arc::new(Mutex::new(started_at)),
        });
        tokio::task::yield_now().await;

        orchestrator.collect_finished().await;

        assert!(orchestrator.retries.is_empty());
        let history = state.history.snapshot();
        assert_eq!(history[0].status, RunStatus::Failed);
        let parked = parks.lock().unwrap();
        assert_eq!(parked.len(), 1);
        assert!(parked[0].contains("worker error; retries exhausted"));
    }

    #[tokio::test]
    async fn dispatch_parks_when_completed_active_run_barrier_reached() {
        let temp = TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join("logs")).unwrap();
        std::fs::write(temp.path().join("WORKFLOW.md"), "Do {{ issue.title }}").unwrap();

        let active_issue = issue("ISSUE-1", None, None);
        let parks = Arc::new(Mutex::new(Vec::new()));
        let tracker = Arc::new(CandidateTracker {
            issue: active_issue.clone(),
            parks: Arc::clone(&parks),
            park_ok: true,
        });
        let agent_cfg = test_agent_config();
        let mut effective_cfg =
            EffectiveLoopConfig::merge(&agent_cfg, &WorkflowFrontmatter::default());
        effective_cfg.max_active_runs = 2;

        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let store = Arc::new(Store::open(&temp.path().join("store.db")).unwrap());
        for idx in 0..2 {
            let started_at = Utc::now() + chrono::Duration::milliseconds(idx);
            let run_id = format!("ISSUE-1-completed-{idx}");
            store
                .insert_run(&NewRun {
                    run_id: &run_id,
                    issue_id: &active_issue.id,
                    issue_identifier: &active_issue.identifier,
                    workspace: "workspace",
                    profile_json: None,
                    workflow_path: None,
                    workflow_sha: None,
                    pid: 42,
                    worker_id: None,
                    started_at,
                })
                .unwrap();
            store
                .finish_run(
                    &run_id,
                    &RunFinish {
                        outcome: RunStatus::Succeeded,
                        exit_code: Some(0),
                        finished_at: started_at,
                    },
                )
                .unwrap();
            store
                .insert_event(&NewEvent {
                    run_id: Some(&run_id),
                    issue_identifier: &active_issue.identifier,
                    kind: "lifecycle",
                    payload: ACTIVE_CONTINUATION_EVENT,
                    ts: started_at,
                })
                .unwrap();
        }
        let state = AppState::new(
            AgentInfo {
                id: "test-agent".to_string(),
                folder: temp.path().display().to_string(),
                tracker: "files".to_string(),
                runner: "claude-code".to_string(),
            },
            control_tx,
            Arc::clone(&store),
            Vec::new(),
        );
        let prompt = PromptRenderer::load(&temp.path().join("WORKFLOW.md")).unwrap();
        let mut orchestrator = Orchestrator::new(
            agent_cfg,
            AgentPaths::new(temp.path().to_path_buf()),
            tracker,
            prompt,
            effective_cfg,
            state.clone(),
            control_rx,
        );

        orchestrator.dispatch(&[active_issue.clone()]).await;

        assert!(orchestrator.slots.is_empty());
        let parked = parks.lock().unwrap();
        assert_eq!(parked.len(), 1);
        assert!(parked[0].contains("too many consecutive completed runs"));
        let runs = store.list_runs_paged(0, 10).unwrap();
        assert!(runs
            .iter()
            .any(|run| run.outcome.as_deref() == Some("park_barrier")));
        let history = state.history.snapshot();
        assert_eq!(history[0].status, RunStatus::ParkBarrier);
    }

    #[tokio::test]
    async fn failed_barrier_parking_write_does_not_record_park_barrier() {
        let temp = TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join("logs")).unwrap();
        std::fs::write(temp.path().join("WORKFLOW.md"), "Do {{ issue.title }}").unwrap();

        let active_issue = issue("ISSUE-1", None, None);
        let parks = Arc::new(Mutex::new(Vec::new()));
        let tracker = Arc::new(CandidateTracker {
            issue: active_issue.clone(),
            parks: Arc::clone(&parks),
            park_ok: false,
        });
        let agent_cfg = test_agent_config();
        let mut effective_cfg =
            EffectiveLoopConfig::merge(&agent_cfg, &WorkflowFrontmatter::default());
        effective_cfg.max_active_runs = 1;

        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let store = Arc::new(Store::open(&temp.path().join("store.db")).unwrap());
        let started_at = Utc::now();
        store
            .insert_run(&NewRun {
                run_id: "ISSUE-1-completed",
                issue_id: &active_issue.id,
                issue_identifier: &active_issue.identifier,
                workspace: "workspace",
                profile_json: None,
                workflow_path: None,
                workflow_sha: None,
                pid: 42,
                worker_id: None,
                started_at,
            })
            .unwrap();
        store
            .finish_run(
                "ISSUE-1-completed",
                &RunFinish {
                    outcome: RunStatus::Succeeded,
                    exit_code: Some(0),
                    finished_at: started_at,
                },
            )
            .unwrap();
        store
            .insert_event(&NewEvent {
                run_id: Some("ISSUE-1-completed"),
                issue_identifier: &active_issue.identifier,
                kind: "lifecycle",
                payload: ACTIVE_CONTINUATION_EVENT,
                ts: started_at,
            })
            .unwrap();
        let state = AppState::new(
            AgentInfo {
                id: "test-agent".to_string(),
                folder: temp.path().display().to_string(),
                tracker: "files".to_string(),
                runner: "claude-code".to_string(),
            },
            control_tx,
            Arc::clone(&store),
            Vec::new(),
        );
        let prompt = PromptRenderer::load(&temp.path().join("WORKFLOW.md")).unwrap();
        let mut orchestrator = Orchestrator::new(
            agent_cfg,
            AgentPaths::new(temp.path().to_path_buf()),
            tracker,
            prompt,
            effective_cfg,
            state.clone(),
            control_rx,
        );

        orchestrator.dispatch(&[active_issue.clone()]).await;

        assert!(orchestrator.slots.is_empty());
        assert!(parks.lock().unwrap().is_empty());
        assert_eq!(
            store
                .consecutive_completed_runs(&active_issue.id, 10)
                .unwrap(),
            1
        );
        let runs = store.list_runs_paged(0, 10).unwrap();
        assert!(!runs
            .iter()
            .any(|run| run.outcome.as_deref() == Some("park_barrier")));
        assert!(state.history.snapshot().is_empty());
    }

    #[test]
    fn sort_priority_null_last_then_created_then_id() {
        let mut v = vec![
            issue("C", None, Some(100)),
            issue("B", Some(2), Some(50)),
            issue("A", Some(2), Some(10)),
            issue("D", Some(1), None),
        ];
        sort_candidates(&mut v);
        let order: Vec<&str> = v.iter().map(|i| i.identifier.as_str()).collect();
        assert_eq!(order, vec!["D", "A", "B", "C"]);
    }

    #[test]
    fn backoff_grows_then_caps() {
        // formula: min(retry_backoff_ms * 2^attempt, 30min)
        assert_eq!(backoff(30_000, 1), Duration::from_secs(60));
        assert_eq!(backoff(30_000, 2), Duration::from_secs(120));
        assert_eq!(backoff(30_000, 3), Duration::from_secs(240));
        assert_eq!(backoff(30_000, 7), Duration::from_secs(30 * 60));
        assert_eq!(backoff(u64::MAX, 64), BACKOFF_CAP);
    }

    fn test_config(command: String) -> AgentConfig {
        AgentConfig {
            id: "test-agent".to_string(),
            name: "Test Agent".to_string(),
            tracker: TrackerConfig {
                use_: "files".to_string(),
                config: Some(TrackerInner {
                    path: "issues".into(),
                }),
                active_states: vec!["todo".to_string()],
                terminal_states: vec!["done".to_string()],
                project_slug: None,
                endpoint: None,
                needs_human: None,
            },
            runner: RunnerConfig {
                use_: "claude-code".to_string(),
                command,
                model: None,
                max_run_timeout_ms: 30_000,
                stall_timeout_ms: 300_000,
            },
            orchestrator: OrchestratorConfig {
                poll_interval_ms: 10,
                max_concurrent: 1,
                max_active_runs: 3,
                max_retries: 1,
                retry_backoff_ms: 10,
            },
            hitl: HitlConfig::default(),
            workspace: WorkspaceConfig {
                root: "workspaces".into(),
            },
            dashboard: DashboardConfig {
                bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 0,
                webhook_secret: None,
            },
        }
    }

    fn test_state(store: Arc<Store>) -> (AppState, mpsc::UnboundedReceiver<ControlMsg>) {
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let state = AppState::new(
            AgentInfo {
                id: "test-agent".to_string(),
                folder: "/tmp/test-agent".to_string(),
                tracker: "files".to_string(),
                runner: "claude-code".to_string(),
            },
            control_tx,
            store,
            Vec::new(),
        );
        (state, control_rx)
    }

    #[tokio::test]
    async fn control_and_shutdown_lifecycle_events_are_queryable() {
        let dir = tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("store.db")).unwrap());
        let (state, control_rx) = test_state(Arc::clone(&store));
        let cfg = test_config("/usr/bin/true".to_string());
        let root = dir.path().canonicalize().unwrap();
        let paths = AgentPaths::new(root.clone());
        let tracker = Arc::new(FileTracker::new(
            root.join("issues"),
            cfg.tracker.active_states.clone(),
            cfg.tracker.terminal_states.clone(),
        ));
        std::fs::write(root.join("WORKFLOW.md"), "noop").unwrap();
        let prompt = PromptRenderer::load(&root.join("WORKFLOW.md")).unwrap();
        let effective_cfg = EffectiveLoopConfig::merge(&cfg, &prompt.snapshot().frontmatter);
        let mut orch = Orchestrator::new(
            cfg,
            paths,
            tracker,
            prompt,
            effective_cfg,
            state,
            control_rx,
        );

        orch.handle_control(ControlMsg::Pause).await;
        orch.handle_control(ControlMsg::Resume).await;
        store
            .insert_event(&NewEvent {
                run_id: None,
                issue_identifier: "-",
                kind: "lifecycle",
                payload: "shutdown signal received, stopping",
                ts: Utc::now(),
            })
            .unwrap();

        let events = store.list_events_since("-", 0, 10).unwrap();
        let payloads: Vec<&str> = events.iter().map(|ev| ev.payload.as_str()).collect();
        assert!(payloads.contains(&"control paused"));
        assert!(payloads.contains(&"control resumed"));
        assert!(payloads.contains(&"shutdown signal received, stopping"));
    }

    #[tokio::test]
    async fn orchestrator_dispatch_heartbeats_and_releases_claim() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let issues = root.join("issues");
        let workspaces = root.join("workspaces");
        std::fs::create_dir_all(&issues).unwrap();
        std::fs::create_dir_all(&workspaces).unwrap();
        std::fs::write(
            issues.join("ALG-173.md"),
            "---\nid: alg-173\nidentifier: ALG-173\ntitle: SQLite persistence\nstate: todo\npriority: 1\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(root.join("WORKFLOW.md"), "Work on {{ issue.identifier }}").unwrap();

        let runner = root.join("sleep-runner.sh");
        std::fs::write(&runner, "#!/bin/sh\nsleep 5\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&runner).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&runner, perms).unwrap();
        }

        let store = Arc::new(Store::open(&root.join("data/store.db")).unwrap());
        let (state, control_rx) = test_state(Arc::clone(&store));
        let cfg = test_config(runner.display().to_string());
        let tracker = Arc::new(FileTracker::new(
            issues.clone(),
            cfg.tracker.active_states.clone(),
            cfg.tracker.terminal_states.clone(),
        ));
        let prompt = PromptRenderer::load(&root.join("WORKFLOW.md")).unwrap();
        let paths = AgentPaths::new(root.clone());
        let effective_cfg = EffectiveLoopConfig::merge(&cfg, &prompt.snapshot().frontmatter);
        let mut orch = Orchestrator::new(
            cfg,
            paths,
            tracker,
            prompt,
            effective_cfg,
            state,
            control_rx,
        );

        orch.tick().await;
        let runs = store.list_runs_paged(0, 10).unwrap();
        assert_eq!(runs.len(), 1);
        let run_id = runs[0].run_id.clone();
        orch.tick().await;
        assert!(
            store.heartbeat_count_for_run(&run_id).unwrap() >= 1,
            "the next tick should heartbeat the live run before reconciliation"
        );

        std::fs::write(
            issues.join("ALG-173.md"),
            "---\nid: alg-173\nidentifier: ALG-173\ntitle: SQLite persistence\nstate: done\npriority: 1\n---\nbody\n",
        )
        .unwrap();
        orch.tick().await;

        let (claims, released) = store.claim_release_count_for_run(&run_id).unwrap();
        assert_eq!(claims, 1);
        assert_eq!(released, 1);
    }

    // ---------------------------------------------------------------------------
    // poll_candidates called once per tick
    // ---------------------------------------------------------------------------

    struct CountingTracker {
        poll_count: Arc<std::sync::atomic::AtomicU32>,
    }

    impl Tracker for CountingTracker {
        fn poll_candidates(&self) -> Result<Vec<Issue>> {
            self.poll_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Vec::new())
        }
        fn fetch_states(&self, _ids: &[String]) -> Result<Vec<Issue>> {
            Ok(Vec::new())
        }
        fn fetch_terminal(&self) -> Result<Vec<Issue>> {
            Ok(Vec::new())
        }
        fn fetch_one(&self, _id: &str) -> Result<Option<Issue>> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn poll_candidates_called_once_per_tick() {
        let temp = TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join("logs")).unwrap();
        std::fs::write(temp.path().join("WORKFLOW.md"), "Do {{ issue.title }}").unwrap();

        let poll_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let tracker = Arc::new(CountingTracker {
            poll_count: Arc::clone(&poll_count),
        });
        let agent_cfg = test_agent_config();
        let effective_cfg = EffectiveLoopConfig::merge(&agent_cfg, &WorkflowFrontmatter::default());
        let store = Arc::new(Store::open(&temp.path().join("store.db")).unwrap());
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let state = AppState::new(
            AgentInfo {
                id: "test-agent".to_string(),
                folder: temp.path().display().to_string(),
                tracker: "files".to_string(),
                runner: "claude-code".to_string(),
            },
            control_tx,
            Arc::clone(&store),
            Vec::new(),
        );
        let prompt = PromptRenderer::load(&temp.path().join("WORKFLOW.md")).unwrap();
        let mut orchestrator = Orchestrator::new(
            agent_cfg,
            AgentPaths::new(temp.path().to_path_buf()),
            tracker,
            prompt,
            effective_cfg,
            state,
            control_rx,
        );

        orchestrator.tick().await;
        assert_eq!(
            poll_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "poll_candidates must be called exactly once per tick"
        );

        orchestrator.tick().await;
        assert_eq!(
            poll_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "poll_candidates must be called exactly once per tick (second tick)"
        );
    }

    #[test]
    fn run_before_remove_scrubs_dotenv_loaded_keys() {
        let temp = TempDir::new().unwrap();
        std::fs::write(
            temp.path().join(".env"),
            "AGENTROPY_TEST_BEFORE_REMOVE_SECRET=file\n",
        )
        .unwrap();
        std::env::remove_var("AGENTROPY_TEST_BEFORE_REMOVE_SECRET");
        crate::dotenv::load_agent_env(temp.path()).unwrap();

        run_before_remove(
            "test -z \"${AGENTROPY_TEST_BEFORE_REMOVE_SECRET:-}\"",
            temp.path().to_str().unwrap(),
            "ISSUE-1",
            "run-1",
        )
        .unwrap();

        std::env::remove_var("AGENTROPY_TEST_BEFORE_REMOVE_SECRET");
    }

    // ---------------------------------------------------------------------------
    // render failure inserts a runs row
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn render_failure_inserts_runs_row_with_dispatch_failed_outcome() {
        let temp = TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join("logs")).unwrap();
        // Template with undefined variable — minijinja strict mode will fail.
        std::fs::write(
            temp.path().join("WORKFLOW.md"),
            "Do {{ issue.undefined_field_xyz }}",
        )
        .unwrap();

        let active_issue = issue("ISSUE-1", None, None);
        let parks = Arc::new(Mutex::new(Vec::new()));
        let tracker = Arc::new(CandidateTracker {
            issue: active_issue.clone(),
            parks: Arc::clone(&parks),
            park_ok: true,
        });
        let agent_cfg = test_agent_config();
        let effective_cfg = EffectiveLoopConfig::merge(&agent_cfg, &WorkflowFrontmatter::default());

        let store = Arc::new(Store::open(&temp.path().join("store.db")).unwrap());
        let (state, control_rx) = test_state(Arc::clone(&store));
        let prompt = PromptRenderer::load(&temp.path().join("WORKFLOW.md")).unwrap();
        let mut orchestrator = Orchestrator::new(
            agent_cfg,
            AgentPaths::new(temp.path().to_path_buf()),
            tracker,
            prompt,
            effective_cfg,
            state.clone(),
            control_rx,
        );

        orchestrator.try_dispatch(active_issue.clone(), 0).await;

        // A runs row must exist even though the child was never spawned.
        let runs = store.list_runs_paged(0, 10).unwrap();
        assert_eq!(runs.len(), 1, "exactly one runs row must be inserted");
        assert_eq!(
            runs[0].outcome.as_deref(),
            Some("dispatch_failed"),
            "outcome must be dispatch_failed"
        );
        assert!(
            runs[0].finished_at.is_some(),
            "finished_at must be set on the failed row"
        );

        // History ring must also record the failure.
        let history = state.history.snapshot();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, RunStatus::DispatchFailed);

        // A retry must be queued (attempt < max_retries).
        assert_eq!(orchestrator.retries.len(), 1);
        assert_eq!(orchestrator.retries[0].identifier, "ISSUE-1");
    }
}
