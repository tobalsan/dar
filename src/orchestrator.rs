//! The orchestration loop (PRD steps 1-9).
//!
//! Owns an in-memory run registry of `max_concurrent` slots, a retry queue with
//! exponential backoff, and the short continuation retry. Ticks every
//! `poll_interval_ms`, draining `ControlMsg`s between/within ticks. It observes
//! issue state and controls child-process lifetime; it NEVER writes issue state.
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

use anyhow::Result;
use chrono::{DateTime, Utc};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::watch;

use crate::config::AgentConfig;
use crate::domain::Issue;
use crate::logging;
use crate::paths::{issue_workspace, AgentPaths};
use crate::prompt::PromptRenderer;
use crate::runner::{self, ExitKind, KillReason, RunnerHandle, SpawnParams};
use crate::state::{ActiveRun, AppState, ControlMsg, QueueItem, RetryItem, RunStatus};
use crate::tracker;
use crate::workflow_config::EffectiveLoopConfig;
use std::sync::Mutex;

/// Max backoff cap for abnormal-exit retries (5 minutes).
const BACKOFF_CAP: Duration = Duration::from_secs(300);
/// Short continuation retry delay for normal-exit-but-still-active (1s).
const CONTINUATION_DELAY: Duration = Duration::from_secs(1);

/// One occupied run slot.
struct RunSlot {
    identifier: String,
    issue: Issue,
    workspace: String,
    handle: Option<RunnerHandle>,
    attempt: u32,
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
    prompt: PromptRenderer,
    /// Effective loop config derived from agent_cfg + WORKFLOW.md frontmatter.
    /// Re-derived on every successful WORKFLOW.md reload.
    effective_cfg: EffectiveLoopConfig,
    state: AppState,
    control_rx: UnboundedReceiver<ControlMsg>,

    // In-memory run registry.
    slots: Vec<RunSlot>,
    retries: Vec<Retry>,
}

impl Orchestrator {
    pub fn new(
        agent_cfg: AgentConfig,
        paths: AgentPaths,
        tracker: Arc<dyn crate::tracker::Tracker>,
        prompt: PromptRenderer,
        effective_cfg: EffectiveLoopConfig,
        state: AppState,
        control_rx: UnboundedReceiver<ControlMsg>,
    ) -> Self {
        Self {
            agent_cfg,
            paths,
            tracker,
            prompt,
            effective_cfg,
            state,
            control_rx,
            slots: Vec::new(),
            retries: Vec::new(),
        }
    }

    /// Main loop. Returns when `shutdown` flips true; kills any active children
    /// first.
    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        loop {
            let poll = Duration::from_millis(self.effective_cfg.poll_interval_ms);

            // One full tick of the loop.
            self.tick().await;

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

        logging::ev("-", "shutdown", "orchestrator stopping; killing active runs");
        self.kill_all(KillReason::OperatorStop).await;
        Ok(())
    }

    /// PRD steps 1-9 for one tick, prefixed with a WORKFLOW.md reload check.
    async fn tick(&mut self) {
        // Drain any pending control messages before doing work.
        while let Ok(msg) = self.control_rx.try_recv() {
            self.handle_control(msg).await;
        }

        // Check for WORKFLOW.md changes and refresh effective config.
        self.maybe_reload_workflow();

        // Step 2: reconcile running runs.
        self.reconcile().await;

        // Steps 7/8: classify finished slots (continuation/backoff/succeed/fail).
        self.collect_finished().await;

        // Steps 4-6: dispatch, unless paused.
        if !self.state.paused.load(Ordering::SeqCst) {
            self.dispatch().await;
        }

        // Refresh dashboard snapshots last so they reflect post-tick reality.
        self.publish_snapshots().await;
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
                let states_changed = new_eff.active_states != self.effective_cfg.active_states
                    || new_eff.terminal_states != self.effective_cfg.terminal_states;

                if states_changed {
                    let mut tracker_cfg = self.agent_cfg.tracker.clone();
                    tracker_cfg.active_states = new_eff.active_states.clone();
                    tracker_cfg.terminal_states = new_eff.terminal_states.clone();
                    match tracker::build(&tracker_cfg, &self.paths) {
                        Ok(t) => {
                            self.tracker = t;
                            logging::ev(
                                "-",
                                "workflow_reload",
                                &format!(
                                    "tracker rebuilt: active={:?} terminal={:?}",
                                    new_eff.active_states, new_eff.terminal_states
                                ),
                            );
                        }
                        Err(e) => {
                            // Keep old state lists so tracker and effective_cfg
                            // stay consistent; other fields (poll interval,
                            // runner, etc.) still get the new values.
                            new_eff.active_states =
                                self.effective_cfg.active_states.clone();
                            new_eff.terminal_states =
                                self.effective_cfg.terminal_states.clone();
                            logging::ev(
                                "-",
                                "workflow_reload",
                                &format!(
                                    "tracker rebuild failed (state lists unchanged): {e:#}"
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

        let mut to_cancel: Vec<(usize, RunStatus, &'static str)> = Vec::new();

        for (idx, slot) in self.slots.iter().enumerate() {
            match &slot.handle {
                None => continue,
                Some(h) if h.is_finished() => continue,
                Some(_) => {}
            }
            match self.tracker.fetch_one(&slot.identifier) {
                Ok(Some(issue)) => {
                    let st = &issue.state;
                    if terminal.contains(st) {
                        logging::ev(&slot.identifier, "reconcile", "issue terminal; finishing");
                        to_cancel.push((idx, RunStatus::Succeeded, "terminal at reconcile"));
                    } else if needs_human == Some(st.as_str()) {
                        logging::ev(
                            &slot.identifier,
                            "reconcile",
                            &format!("issue state {st:?} is needs-human; releasing without retry"),
                        );
                        to_cancel.push((idx, RunStatus::NeedsHuman, "needs-human at reconcile"));
                    } else if !active.contains(st) {
                        logging::ev(
                            &slot.identifier,
                            "reconcile",
                            &format!("issue state {st:?} neither active nor terminal; cancelling"),
                        );
                        to_cancel.push((idx, RunStatus::Cancelled, "non-active at reconcile"));
                    }
                }
                Ok(None) => {
                    logging::ev(&slot.identifier, "reconcile", "issue file missing; cancelling");
                    to_cancel.push((idx, RunStatus::Cancelled, "issue file missing"));
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

        to_cancel.sort_unstable_by_key(|(idx, _, _)| *idx);
        for (idx, status, note) in to_cancel.into_iter().rev() {
            let mut slot = self.slots.remove(idx);
            let pid = slot.handle.as_ref().map(|h| h.pid()).unwrap_or(0);
            if let Some(handle) = slot.handle.take() {
                handle.request_kill(KillReason::Reconcile);
            }
            self.record_history(&slot.identifier, status, pid, note);
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
            let handle = match slot.handle.take() {
                Some(h) => h,
                None => continue,
            };
            let exit = handle.wait().await;
            let id = slot.identifier.clone();

            match exit {
                ExitKind::Normal => {
                    let state_now = self
                        .tracker
                        .fetch_one(&id)
                        .ok()
                        .flatten()
                        .map(|i| i.state);
                    match state_now {
                        Some(ref st) if terminal.contains(st) => {
                            logging::ev(&id, "succeeded", "terminal after normal exit");
                            self.record_history(&id, RunStatus::Succeeded, pid, "terminal after normal exit");
                        }
                        Some(ref st) if needs_human.as_deref() == Some(st.as_str()) => {
                            logging::ev(&id, "needs_human", "needs-human state after normal exit; releasing without retry");
                            self.record_history(&id, RunStatus::NeedsHuman, pid, "needs-human after normal exit");
                        }
                        Some(ref st) if active.contains(st) => {
                            logging::ev(&id, "continuation", "still active after exit 0; retry 1s");
                            self.retries.push(Retry {
                                identifier: id.clone(),
                                attempt: slot.attempt,
                                due_at: Utc::now() + chrono::Duration::from_std(CONTINUATION_DELAY).unwrap(),
                                last_error: String::new(),
                                continuation: true,
                            });
                        }
                        _ => {
                            logging::ev(&id, "succeeded", "non-active after normal exit; releasing");
                            self.record_history(&id, RunStatus::Succeeded, pid, "non-active after normal exit");
                        }
                    }
                }
                ExitKind::Abnormal => {
                    let state_now = self
                        .tracker
                        .fetch_one(&id)
                        .ok()
                        .flatten()
                        .map(|i| i.state);
                    if matches!(
                        (state_now.as_deref(), needs_human.as_deref()),
                        (Some(st), Some(needs_human)) if st == needs_human
                    ) {
                        logging::ev(
                            &id,
                            "needs_human",
                            "needs-human after abnormal exit; releasing without retry",
                        );
                        self.record_history(&id, RunStatus::NeedsHuman, pid, "needs-human after abnormal exit");
                    } else if slot.attempt >= max_retries {
                        logging::ev(
                            &id,
                            "failed",
                            &format!("abnormal exit; retries exhausted ({}/{})", slot.attempt, max_retries),
                        );
                        self.record_history(&id, RunStatus::Failed, pid, "abnormal exit; retries exhausted");
                    } else {
                        let next = slot.attempt + 1;
                        let delay = backoff(self.effective_cfg.retry_backoff_ms, next);
                        let due = Utc::now() + chrono::Duration::from_std(delay).unwrap();
                        logging::ev(
                            &id,
                            "retry_queued",
                            &format!("abnormal exit; attempt {next}/{max_retries} in {}ms", delay.as_millis()),
                        );
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

    /// Steps 4-6. Build the candidate set, sort, and dispatch into free slots.
    async fn dispatch(&mut self) {
        let max = self.effective_cfg.max_concurrent;
        if self.slots.len() >= max {
            return;
        }

        let busy: HashSet<String> = self
            .slots
            .iter()
            .map(|s| s.identifier.clone())
            .collect();

        let now = Utc::now();

        // First, dispatch due retries (continuation + backoff), oldest first.
        let mut due_idx: Vec<usize> = self
            .retries
            .iter()
            .enumerate()
            .filter(|(_, r)| r.due_at <= now && !busy.contains(&r.identifier))
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
                    let label = if retry.continuation { "continuation" } else { "retry" };
                    logging::ev(&retry.identifier, "dispatch", &format!("from {label} attempt={}", retry.attempt));
                    self.try_dispatch(issue, retry.attempt).await;
                }
                _ => {
                    logging::ev(&retry.identifier, "retry_drop", "no longer active; dropping retry");
                }
            }
        }

        if self.slots.len() >= max {
            return;
        }

        let mut candidates = match self.tracker.poll_candidates() {
            Ok(v) => v,
            Err(e) => {
                logging::ev("-", "poll_error", &format!("{e:#}"));
                return;
            }
        };

        let retry_ids: HashSet<String> = self.retries.iter().map(|r| r.identifier.clone()).collect();
        candidates.retain(|i| {
            !busy.contains(&i.identifier)
                && !busy.contains(&i.id)
                && !retry_ids.contains(&i.identifier)
                && !retry_ids.contains(&i.id)
        });

        sort_candidates(&mut candidates);

        for issue in candidates {
            if self.slots.len() >= max {
                break;
            }
            if self.slots.iter().any(|s| s.identifier == issue.identifier) {
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
        let max_retries = self.effective_cfg.max_retries;
        let prompt = match self.prompt.render(&issue, attempt, max_retries) {
            Ok(p) => p,
            Err(e) => {
                logging::ev(
                    &issue.identifier,
                    "render_error",
                    &format!("WORKFLOW.md render failed: {e:#}"),
                );
                self.schedule_backoff_after_render_failure(&issue, attempt, &format!("render error: {e}"));
                return;
            }
        };

        let ws_root = self.paths.root.join(&self.effective_cfg.workspace_root);
        if let Err(e) = std::fs::create_dir_all(&ws_root) {
            logging::ev(
                &issue.identifier,
                "workspace_error",
                &format!("creating workspace root {}: {e}", ws_root.display()),
            );
            self.schedule_backoff_after_render_failure(&issue, attempt, "workspace root error");
            return;
        }
        let workspace = match issue_workspace(&ws_root, &issue.identifier) {
            Ok(w) => w,
            Err(e) => {
                logging::ev(
                    &issue.identifier,
                    "workspace_error",
                    &format!("{e:#}"),
                );
                self.schedule_backoff_after_render_failure(&issue, attempt, "workspace error");
                return;
            }
        };

        let last_event_at = Arc::new(Mutex::new(Utc::now()));
        let params = SpawnParams {
            command: &self.effective_cfg.runner_command,
            runner_kind: &self.effective_cfg.runner_kind,
            model: self.effective_cfg.model.clone(),
            workspace: &workspace,
            workspace_root: &ws_root,
            prompt,
            issue_id: issue.identifier.clone(),
            max_run_timeout_ms: self.effective_cfg.max_run_timeout_ms,
            events: Arc::clone(&self.state.events),
            last_event_at: Arc::clone(&last_event_at),
        };

        match runner::spawn(params).await {
            Ok(handle) => {
                self.slots.push(RunSlot {
                    identifier: issue.identifier.clone(),
                    workspace: workspace.display().to_string(),
                    issue,
                    handle: Some(handle),
                    attempt,
                });
            }
            Err(e) => {
                logging::ev(&issue.identifier, "spawn_error", &format!("{e:#}"));
                self.schedule_backoff_after_render_failure(&issue, attempt, "spawn error");
            }
        }
    }

    /// A pre-spawn failure (render/workspace/spawn) is an abnormal attempt:
    /// schedule a backoff retry up to max_retries, else log Failed.
    fn schedule_backoff_after_render_failure(&mut self, issue: &Issue, attempt: u32, err: &str) {
        let max = self.effective_cfg.max_retries;
        if attempt >= max {
            logging::ev(&issue.identifier, "failed", &format!("{err}; retries exhausted"));
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
                self.kill_all(KillReason::OperatorStop).await;
                self.slots.clear();
            }
            ControlMsg::Pause => {
                self.state.paused.store(true, Ordering::SeqCst);
                logging::ev("-", "control", "paused");
            }
            ControlMsg::Resume => {
                self.state.paused.store(false, Ordering::SeqCst);
                logging::ev("-", "control", "resumed");
            }
        }
    }

    /// Record a finished run in the dashboard history ring.
    fn record_history(&self, identifier: &str, status: RunStatus, pid: u32, note: &str) {
        self.state.history.push(crate::state::HistoryEntry {
            identifier: identifier.to_string(),
            status,
            pid,
            ended_at: Utc::now(),
            note: note.to_string(),
        });
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
                self.state.history.push(crate::state::HistoryEntry {
                    identifier: slot.identifier.clone(),
                    status: RunStatus::Cancelled,
                    pid,
                    ended_at: Utc::now(),
                    note: "operator stop / shutdown".to_string(),
                });
            }
        }
    }

    /// Write the active-run, queue, and retry snapshots to `AppState` for the
    /// dashboard.
    async fn publish_snapshots(&self) {
        let active = self.slots.first().map(|slot| {
            let last_event = self
                .last_event_line(&slot.identifier)
                .unwrap_or_default();
            ActiveRun {
                identifier: slot.identifier.clone(),
                state: slot.issue.state.clone(),
                workspace: slot.workspace.clone(),
                pid: slot.handle.as_ref().map(|h| h.pid()).unwrap_or(0),
                started_at: slot
                    .handle
                    .as_ref()
                    .map(|h| h.started_at)
                    .unwrap_or_else(Utc::now),
                last_event,
                status: RunStatus::Running,
            }
        });
        {
            let mut guard = self.state.active.write().await;
            *guard = active;
        }

        let busy: HashSet<String> = self.slots.iter().map(|s| s.identifier.clone()).collect();
        let retry_ids: HashSet<String> =
            self.retries.iter().map(|r| r.identifier.clone()).collect();
        let mut queue_items: Vec<QueueItem> = match self.tracker.poll_candidates() {
            Ok(mut v) => {
                v.retain(|i| !busy.contains(&i.identifier) && !retry_ids.contains(&i.identifier));
                sort_candidates(&mut v);
                v.into_iter()
                    .map(|i| QueueItem {
                        identifier: i.identifier,
                        title: i.title,
                        state: i.state,
                        priority: i.priority,
                        created_at: i.created_at,
                    })
                    .collect()
            }
            Err(_) => Vec::new(),
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

/// Sort candidates: priority asc (null last), then `created_at` asc (null last),
/// then identifier asc.
pub(crate) fn sort_candidates(v: &mut Vec<Issue>) {
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

/// Backoff for abnormal-exit retries: `min(retry_backoff_ms * 2^(attempt-1), 5min)`.
/// `attempt` is 1-based.
pub(crate) fn backoff(retry_backoff_ms: u64, attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1);
    let factor = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    let ms = retry_backoff_ms.saturating_mul(factor);
    let d = Duration::from_millis(ms);
    if d > BACKOFF_CAP {
        BACKOFF_CAP
    } else {
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AgentConfig, DashboardConfig, OrchestratorConfig, RunnerConfig, TrackerConfig,
        TrackerInner, WorkspaceConfig,
    };
    use crate::paths::AgentPaths;
    use crate::prompt::PromptRenderer;
    use crate::state::{AgentInfo, AppState};
    use crate::tracker::Tracker;
    use crate::workflow_config::{EffectiveLoopConfig, WorkflowFrontmatter};
    use anyhow::Result;
    use chrono::TimeZone;
    use std::net::{IpAddr, Ipv4Addr};
    use tempfile::TempDir;
    use tokio::sync::mpsc;

    fn issue(id: &str, prio: Option<i32>, created: Option<i64>) -> Issue {
        Issue {
            id: id.to_string(),
            identifier: id.to_string(),
            title: id.to_string(),
            description: None,
            url: None,
            state: "todo".to_string(),
            priority: prio,
            assignees: vec![],
            labels: vec![],
            created_at: created.map(|s| Utc.timestamp_opt(s, 0).unwrap()),
            updated_at: None,
        }
    }

    struct StaticTracker {
        issue: Issue,
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

    fn test_agent_config() -> AgentConfig {
        AgentConfig {
            id: "test-agent".to_string(),
            name: "Test Agent".to_string(),
            tracker: TrackerConfig {
                use_: "files".to_string(),
                config: TrackerInner {
                    path: "issues".into(),
                },
                active_states: vec!["todo".to_string()],
                terminal_states: vec!["done".to_string()],
            },
            runner: RunnerConfig {
                use_: "claude-code".to_string(),
                command: "claude".to_string(),
                max_run_timeout_ms: 1000,
            },
            orchestrator: OrchestratorConfig {
                poll_interval_ms: 100,
                max_concurrent: 1,
                max_retries: 3,
                retry_backoff_ms: 1000,
            },
            workspace: WorkspaceConfig {
                root: "workspaces".into(),
            },
            dashboard: DashboardConfig {
                bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 7878,
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
        });
        let agent_cfg = test_agent_config();
        let mut effective_cfg =
            EffectiveLoopConfig::merge(&agent_cfg, &WorkflowFrontmatter::default());
        effective_cfg.needs_human = Some("needs_human".to_string());

        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let state = AppState::new(
            AgentInfo {
                id: "test-agent".to_string(),
                folder: temp.path().display().to_string(),
                tracker: "files".to_string(),
                runner: "claude-code".to_string(),
            },
            control_tx,
            temp.path().join("logs/history.jsonl"),
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
        orchestrator.slots.push(RunSlot {
            identifier: needs_issue.identifier.clone(),
            issue: needs_issue,
            workspace: "workspace".to_string(),
            handle: Some(RunnerHandle::finished_for_test(42, ExitKind::Abnormal)),
            attempt: 1,
        });
        tokio::task::yield_now().await;

        orchestrator.collect_finished().await;

        assert!(orchestrator.retries.is_empty());
        let history = state.history.snapshot();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].identifier, "ISSUE-1");
        assert_eq!(history[0].status, RunStatus::NeedsHuman);
        assert_eq!(history[0].note, "needs-human after abnormal exit");
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
        let state = AppState::new(
            AgentInfo {
                id: "test-agent".to_string(),
                folder: temp.path().display().to_string(),
                tracker: "files".to_string(),
                runner: "claude-code".to_string(),
            },
            control_tx,
            temp.path().join("logs/history.jsonl"),
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
        orchestrator.slots.push(RunSlot {
            identifier: active_issue.identifier.clone(),
            issue: active_issue,
            workspace: "workspace".to_string(),
            handle: Some(RunnerHandle::finished_for_test(42, ExitKind::Abnormal)),
            attempt: 1,
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
    async fn abnormal_exit_with_needs_human_config_and_different_tracker_state_retries() {
        let temp = TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join("logs")).unwrap();
        std::fs::write(temp.path().join("WORKFLOW.md"), "Do {{ issue.title }}").unwrap();

        let active_issue = issue("ISSUE-1", None, None);
        let tracker = Arc::new(StaticTracker {
            issue: active_issue.clone(),
        });
        let agent_cfg = test_agent_config();
        let mut effective_cfg =
            EffectiveLoopConfig::merge(&agent_cfg, &WorkflowFrontmatter::default());
        effective_cfg.needs_human = Some("stuck".to_string());

        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let state = AppState::new(
            AgentInfo {
                id: "test-agent".to_string(),
                folder: temp.path().display().to_string(),
                tracker: "files".to_string(),
                runner: "claude-code".to_string(),
            },
            control_tx,
            temp.path().join("logs/history.jsonl"),
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
        orchestrator.slots.push(RunSlot {
            identifier: active_issue.identifier.clone(),
            issue: active_issue,
            workspace: "workspace".to_string(),
            handle: Some(RunnerHandle::finished_for_test(42, ExitKind::Abnormal)),
            attempt: 1,
        });
        tokio::task::yield_now().await;

        orchestrator.collect_finished().await;

        assert_eq!(orchestrator.retries.len(), 1);
        assert_eq!(orchestrator.retries[0].identifier, "ISSUE-1");
        assert_eq!(orchestrator.retries[0].attempt, 2);
        assert!(!orchestrator.retries[0].continuation);
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
        assert_eq!(backoff(1000, 1), Duration::from_millis(1000));
        assert_eq!(backoff(1000, 2), Duration::from_millis(2000));
        assert_eq!(backoff(1000, 3), Duration::from_millis(4000));
        assert_eq!(backoff(1000, 30), BACKOFF_CAP);
        assert_eq!(backoff(u64::MAX, 64), BACKOFF_CAP);
    }
}
