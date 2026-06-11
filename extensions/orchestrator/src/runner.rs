//! Runner contract re-exports plus the glue impls that bind this crate's
//! event ring and SQLite store to the runner event traits.
//!
//! The five runner backends now live in their own extension crates
//! (`extensions/runner-{pi,claude,codex,cli,fake}`), each registering itself
//! as a typed `dyn cap_runner::Runner` service. Shared spawn/supervision logic
//! lives in `runner-core`.

use chrono::{DateTime, Utc};

pub use cap_runner::{ExitKind, KillReason, RunnerHandle, SpawnParams};
pub use runner_core::{term_then_kill, wait_for_pids_dead};

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
