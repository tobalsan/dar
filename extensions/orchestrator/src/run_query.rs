//! `RunQuery` service over the SQLite `Store`.
//!
//! Registered under id `"orchestrator"` during `register()` so the dashboard
//! can resolve it from the service registry without importing this crate. The
//! store is only opened in `start()`, so the wrapper holds a late-bound
//! `OnceLock<Arc<Store>>`: methods return `None`/empty until the store is set.

use std::sync::{Arc, OnceLock};

use orchestrator_api::{EventRow, RunQuery, RunRow};

use crate::store::Store;

/// Late-bound `RunQuery` backed by the orchestrator's SQLite store.
#[derive(Default)]
pub struct RunQueryWrapper {
    store: Arc<OnceLock<Arc<Store>>>,
}

impl RunQueryWrapper {
    /// Build a wrapper sharing the given (initially empty) store cell. Register
    /// the wrapper early, then call [`RunQueryWrapper::set_store`] on a clone
    /// holding the same cell once the store is opened in `start()`.
    pub fn new(store: Arc<OnceLock<Arc<Store>>>) -> Self {
        Self { store }
    }

    /// Bind the opened store. No-op if already set.
    pub fn set_store(&self, store: Arc<Store>) {
        let _ = self.store.set(store);
    }
}

impl RunQuery for RunQueryWrapper {
    fn run(&self, run_id: &str) -> Option<RunRow> {
        let store = self.store.get()?;
        store.get_run(run_id).ok().flatten().map(into_api_run)
    }

    fn events_for_run(&self, run_id: &str, since: i64, limit: usize) -> Vec<EventRow> {
        let Some(store) = self.store.get() else {
            return Vec::new();
        };
        store
            .list_events_for_run(run_id, since, limit)
            .unwrap_or_default()
            .into_iter()
            .map(into_api_event)
            .collect()
    }
}

fn into_api_run(r: crate::store::RunRow) -> RunRow {
    RunRow {
        run_id: r.run_id,
        issue_id: r.issue_id,
        issue_identifier: r.issue_identifier,
        workspace: r.workspace,
        profile_json: r.profile_json,
        workflow_path: r.workflow_path,
        workflow_sha: r.workflow_sha,
        pid: r.pid,
        worker_id: r.worker_id,
        started_at: r.started_at,
        finished_at: r.finished_at,
        outcome: r.outcome,
        exit_code: r.exit_code,
        process_alive: r.process_alive,
    }
}

fn into_api_event(e: crate::store::EventRow) -> EventRow {
    EventRow {
        event_id: e.event_id,
        run_id: e.run_id,
        issue_identifier: e.issue_identifier,
        kind: e.kind,
        payload: e.payload,
        ts: e.ts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapper_returns_none_and_empty_before_store_set() {
        let cell: Arc<OnceLock<Arc<Store>>> = Arc::new(OnceLock::new());
        let wrapper = RunQueryWrapper::new(cell);
        assert!(wrapper.run("nope").is_none());
        assert!(wrapper.events_for_run("nope", 0, 10).is_empty());
    }
}
