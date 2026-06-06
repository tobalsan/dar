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
use crate::state::{ActiveRun, AppState, ControlMsg, HistoryEntry, QueueItem, RetryItem, RunStatus};
use crate::store::{new_run_id, NewEvent, NewRun, RunFinish};
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
    /// SQLite run_id for this dispatch attempt. Used for finish_run / event writes.
    run_id: String,
    started_at: DateTime<Utc>,
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
    cfg: AgentConfig,
    paths: AgentPaths,
    tracker: Arc<dyn crate::tracker::Tracker>,
    prompt: PromptRenderer,
    state: AppState,
    control_rx: UnboundedReceiver<ControlMsg>,

    // In-memory run registry.
    slots: Vec<RunSlot>,
    retries: Vec<Retry>,
}

impl Orchestrator {
    pub fn new(
        cfg: AgentConfig,
        paths: AgentPaths,
        tracker: Arc<dyn crate::tracker::Tracker>,
        prompt: PromptRenderer,
        state: AppState,
        control_rx: UnboundedReceiver<ControlMsg>,
    ) -> Self {
        Self {
            cfg,
            paths,
            tracker,
            prompt,
            state,
            control_rx,
            slots: Vec::new(),
            retries: Vec::new(),
        }
    }

    /// Main loop. Returns when `shutdown` flips true; kills any active children
    /// first.
    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        let poll = Duration::from_millis(self.cfg.orchestrator.poll_interval_ms);

        loop {
            // One full tick of the loop.
            self.tick().await;

            // Inter-tick sleep, but stay responsive to control + shutdown.
            // Receive a control message into a local to release the &mut borrow
            // on control_rx before invoking the (also &mut self) handler.
            let mut pending: Option<ControlMsg> = None;
            tokio::select! {
                _ = tokio::time::sleep(poll) => {}
                _ = shutdown.changed() => {}
                msg = self.control_rx.recv() => {
                    pending = msg; // None only if the channel is closed
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

    /// PRD steps 1-9 for one tick.
    async fn tick(&mut self) {
        // Drain any pending control messages before doing work.
        while let Ok(msg) = self.control_rx.try_recv() {
            self.handle_control(msg).await;
        }

        // Step 2: reconcile running runs.
        self.reconcile().await;

        // Steps 7/8: classify finished slots (continuation/backoff/succeed/fail).
        self.collect_finished().await;

        // Promote any due retries back into dispatchable candidates is handled
        // implicitly: due retries are dispatched in dispatch().

        // Steps 4-6: dispatch, unless paused.
        if !self.state.paused.load(Ordering::SeqCst) {
            self.dispatch().await;
        }

        // Refresh dashboard snapshots last so they reflect post-tick reality.
        self.publish_snapshots().await;
    }

    /// Step 2. Re-read each running issue's file; kill+cancel if it is missing,
    /// terminal, or in neither active nor terminal state.
    async fn reconcile(&mut self) {
        let active = &self.cfg.tracker.active_states;
        let terminal = &self.cfg.tracker.terminal_states;

        // Collect (index, status, note) for slots to terminate. A terminal issue
        // means the agent succeeded; missing or non-active means cancellation.
        let mut to_cancel: Vec<(usize, RunStatus, &'static str)> = Vec::new();

        for (idx, slot) in self.slots.iter().enumerate() {
            // Skip slots with no live child, or whose child has already finished
            // (collect_finished will classify a clean exit as Succeeded; reconcile
            // must not steal it as a cancellation).
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
                    } else if !active.contains(st) {
                        logging::ev(
                            &slot.identifier,
                            "reconcile",
                            &format!("issue state {st:?} neither active nor terminal; cancelling"),
                        );
                        to_cancel.push((idx, RunStatus::Cancelled, "non-active at reconcile"));
                    }
                    // else: still active -> keep running.
                }
                Ok(None) => {
                    logging::ev(&slot.identifier, "reconcile", "issue file missing; cancelling");
                    to_cancel.push((idx, RunStatus::Cancelled, "issue file missing"));
                }
                Err(e) => {
                    // Transient read failure: leave it running, log and move on.
                    logging::ev(
                        &slot.identifier,
                        "reconcile",
                        &format!("fetch_one error: {e:#}; keeping run"),
                    );
                }
            }
        }

        // Terminate from the back so indices stay valid.
        to_cancel.sort_unstable_by_key(|(idx, _, _)| *idx);
        for (idx, status, note) in to_cancel.into_iter().rev() {
            let mut slot = self.slots.remove(idx);
            let pid = slot.handle.as_ref().map(|h| h.pid()).unwrap_or(0);
            if let Some(handle) = slot.handle.take() {
                handle.request_kill(KillReason::Reconcile);
            }
            self.record_history(&slot.run_id, &slot.identifier, status, pid, note, None);
            // Not retried: terminal = done, missing/non-active = cancelled.
        }
    }

    /// Steps 7/8. For each slot whose child has finished, classify the exit and
    /// either succeed, schedule a continuation/backoff retry, or fail.
    async fn collect_finished(&mut self) {
        let active = &self.cfg.tracker.active_states.clone();
        let terminal = &self.cfg.tracker.terminal_states.clone();
        let max_retries = self.cfg.orchestrator.max_retries;

        // Indices of slots whose handle reports finished.
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
            let run_id = slot.run_id.clone();

            match exit {
                ExitKind::Normal => {
                    // Step 7: re-fetch the issue.
                    let state_now = self
                        .tracker
                        .fetch_one(&id)
                        .ok()
                        .flatten()
                        .map(|i| i.state);
                    match state_now {
                        Some(ref st) if terminal.contains(st) => {
                            logging::ev(&id, "succeeded", "terminal after normal exit");
                            self.record_history(&run_id, &id, RunStatus::Succeeded, pid, "terminal after normal exit", Some(0));
                        }
                        Some(ref st) if active.contains(st) => {
                            logging::ev(&id, "continuation", "still active after exit 0; retry 1s");
                            self.retries.push(Retry {
                                identifier: id.clone(),
                                attempt: slot.attempt, // continuation: not incremented
                                due_at: Utc::now() + chrono::Duration::from_std(CONTINUATION_DELAY).unwrap(),
                                last_error: String::new(),
                                continuation: true,
                            });
                        }
                        _ => {
                            // Missing or neither active nor terminal: succeed, no re-dispatch.
                            logging::ev(&id, "succeeded", "non-active after normal exit; releasing");
                            self.record_history(&run_id, &id, RunStatus::Succeeded, pid, "non-active after normal exit", Some(0));
                        }
                    }
                }
                ExitKind::Abnormal => {
                    // Step 8: backoff retry up to max_retries, else Failed.
                    if slot.attempt >= max_retries {
                        logging::ev(
                            &id,
                            "failed",
                            &format!("abnormal exit; retries exhausted ({}/{})", slot.attempt, max_retries),
                        );
                        self.record_history(&run_id, &id, RunStatus::Failed, pid, "abnormal exit; retries exhausted", None);
                    } else {
                        let next = slot.attempt + 1;
                        let delay = backoff(self.cfg.orchestrator.retry_backoff_ms, next);
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
        let max = self.cfg.orchestrator.max_concurrent;
        if self.slots.len() >= max {
            return;
        }

        // Identifiers currently running or pending retry are not re-dispatched
        // from the candidate poll (retries dispatch via their own path).
        let busy: HashSet<String> = self
            .slots
            .iter()
            .map(|s| s.identifier.clone())
            .collect();

        let now = Utc::now();

        // First, dispatch due retries (continuation + backoff), oldest first.
        // Take indices of due retries not already running.
        let mut due_idx: Vec<usize> = self
            .retries
            .iter()
            .enumerate()
            .filter(|(_, r)| r.due_at <= now && !busy.contains(&r.identifier))
            .map(|(i, _)| i)
            .collect();
        due_idx.sort_unstable();

        // Process due retries from the front; stop when slots fill.
        // Remove from back to keep indices valid, but we want oldest first, so
        // collect the retry items first then remove.
        let mut due_retries: Vec<Retry> = Vec::new();
        for idx in due_idx.into_iter().rev() {
            due_retries.push(self.retries.remove(idx));
        }
        due_retries.reverse(); // restore order

        for retry in due_retries {
            if self.slots.len() >= max {
                // No slot now; put it back to be picked up next tick.
                self.retries.push(retry);
                continue;
            }
            if self.slots.iter().any(|s| s.identifier == retry.identifier) {
                continue;
            }
            match self.tracker.fetch_one(&retry.identifier) {
                Ok(Some(issue)) if self.cfg.tracker.active_states.contains(&issue.state) => {
                    let label = if retry.continuation { "continuation" } else { "retry" };
                    logging::ev(&retry.identifier, "dispatch", &format!("from {label} attempt={}", retry.attempt));
                    self.try_dispatch(issue, retry.attempt).await;
                }
                _ => {
                    // Issue no longer active/exists: drop the retry silently.
                    logging::ev(&retry.identifier, "retry_drop", "no longer active; dropping retry");
                }
            }
        }

        if self.slots.len() >= max {
            return;
        }

        // Then, fresh candidates from the tracker.
        let mut candidates = match self.tracker.poll_candidates() {
            Ok(v) => v,
            Err(e) => {
                logging::ev("-", "poll_error", &format!("{e:#}"));
                return;
            }
        };

        // Exclude running, and exclude anything already retry-queued.
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
        // Strict-undefined render: failure means we must not spawn.
        let prompt = match self.prompt.render(&issue) {
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

        // Per-issue contained workspace.
        let ws_root = self.paths.workspace_root(&self.cfg.workspace);
        // Ensure the workspace root exists before issue_workspace canonicalizes.
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

        let started_at = Utc::now();
        let run_id = new_run_id(&issue.identifier, &started_at);

        let last_event_at = Arc::new(Mutex::new(started_at));
        let params = SpawnParams {
            command: &self.cfg.runner.command,
            workspace: &workspace,
            workspace_root: &ws_root,
            prompt,
            issue_id: issue.identifier.clone(),
            run_id: run_id.clone(),
            max_run_timeout_ms: self.cfg.runner.max_run_timeout_ms,
            events: Arc::clone(&self.state.events),
            store: Arc::clone(&self.state.store),
            last_event_at: Arc::clone(&last_event_at),
        };

        match runner::spawn(params).await {
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
        let max = self.cfg.orchestrator.max_retries;
        if attempt >= max {
            logging::ev(&issue.identifier, "failed", &format!("{err}; retries exhausted"));
            return;
        }
        let next = attempt + 1;
        let delay = backoff(self.cfg.orchestrator.retry_backoff_ms, next);
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
                // Operator Stop => Cancelled, not retried. Drop the slots.
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

    /// Record a finished run: update the in-memory history ring and the SQLite store.
    fn record_history(
        &self,
        run_id: &str,
        identifier: &str,
        status: RunStatus,
        pid: u32,
        note: &str,
        exit_code: Option<i32>,
    ) {
        let ended_at = Utc::now();
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
    }

    /// Request-kill every running child for the given reason.
    async fn kill_all(&mut self, reason: KillReason) {
        // KillReason is not Clone, so map to a per-slot constructor.
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
                    status: RunStatus::Cancelled,
                    pid,
                    ended_at,
                    note: "operator stop / shutdown".to_string(),
                });
                if let Err(e) = self.state.store.finish_run(
                    &slot.run_id,
                    &RunFinish {
                        outcome: RunStatus::Cancelled,
                        exit_code: None,
                        finished_at: ended_at,
                    },
                ) {
                    tracing::warn!(issue = %slot.identifier, "finish_run (kill_all) SQLite write failed: {e:#}");
                }
            }
        }
    }

    /// Write the active-run, queue, and retry snapshots to `AppState` for the
    /// dashboard.
    async fn publish_snapshots(&self) {
        // Active run (v0 max_concurrent typically 1; surface the first slot).
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

        // Queue: next candidates in dispatch order, excluding running.
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
        // Cap the displayed queue to a reasonable N.
        queue_items.truncate(50);
        {
            let mut guard = self.state.queue.write().await;
            *guard = queue_items;
        }

        // Retry queue snapshot.
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

    /// Best last-event line for the running slot: the most recent ring line that
    /// belongs to this issue, else empty.
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
        // priority: Some(n) before None; among Some, ascending.
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
        // created_at: Some before None; among Some, ascending.
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
    // Saturate the exponential so we never overflow before capping.
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
    use chrono::TimeZone;

    fn issue(id: &str, prio: Option<i32>, created: Option<i64>) -> Issue {
        Issue {
            id: id.to_string(),
            identifier: id.to_string(),
            title: id.to_string(),
            description: None,
            state: "todo".to_string(),
            priority: prio,
            assignees: vec![],
            labels: vec![],
            created_at: created.map(|s| Utc.timestamp_opt(s, 0).unwrap()),
            updated_at: None,
        }
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
        // prio 1 (D), then prio 2 by created asc (A then B), then null (C).
        assert_eq!(order, vec!["D", "A", "B", "C"]);
    }

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(backoff(1000, 1), Duration::from_millis(1000));
        assert_eq!(backoff(1000, 2), Duration::from_millis(2000));
        assert_eq!(backoff(1000, 3), Duration::from_millis(4000));
        // Cap at 5 minutes.
        assert_eq!(backoff(1000, 30), BACKOFF_CAP);
        // No overflow panic at huge attempt.
        assert_eq!(backoff(u64::MAX, 64), BACKOFF_CAP);
    }
}
