//! The Issue domain struct exactly per PRD, shared by tracker, orchestrator,
//! runner, and dashboard view.

use chrono::{DateTime, Utc};

/// One issue as read from a tracker. The orchestrator never mutates issue
/// state; this struct is a read-only view.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Issue {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub description: Option<String>,
    pub state: String,
    pub priority: Option<i32>,
    pub assignees: Vec<String>,
    pub labels: Vec<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
}

impl Issue {
    /// Exposes `issue.*` fields to the WORKFLOW.md minijinja template. Built via
    /// `Value::from_serialize` so field names map 1:1 with the struct above.
    pub fn for_template(&self) -> minijinja::Value {
        minijinja::Value::from_serialize(self)
    }
}
