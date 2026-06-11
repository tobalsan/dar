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
