use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};

/// One issue as read from a tracker. The orchestrator never mutates issue
/// state; this struct is a read-only view.
#[non_exhaustive]
#[derive(Debug, Clone, serde::Serialize)]
pub struct Issue {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub description: Option<String>,
    pub url: Option<String>,
    pub state: String,
    pub priority: Option<i32>,
    pub assignees: Vec<String>,
    pub labels: Vec<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    /// Identifier of the parent issue, if any.
    pub parent_id: Option<String>,
    /// Identifiers of issues that block this one.
    pub blocked_by: Vec<String>,
    pub project_name: Option<String>,
    pub project_slug: Option<String>,
    /// Tracker-native fields that do not belong to the portable contract.
    pub metadata: BTreeMap<String, serde_json::Value>,
}

impl Issue {
    pub fn builder(
        id: impl Into<String>,
        identifier: impl Into<String>,
        title: impl Into<String>,
        state: impl Into<String>,
    ) -> IssueBuilder {
        IssueBuilder::new(id, identifier, title, state)
    }

    pub fn new(
        id: impl Into<String>,
        identifier: impl Into<String>,
        title: impl Into<String>,
        state: impl Into<String>,
    ) -> Self {
        Self::builder(id, identifier, title, state).build()
    }

    /// Exposes `issue.*` fields to the WORKFLOW.md minijinja template. Built via
    /// `Value::from_serialize` so field names map 1:1 with the struct above.
    pub fn for_template(&self) -> minijinja::Value {
        minijinja::Value::from_serialize(self)
    }
}

pub struct IssueBuilder {
    issue: Issue,
}

impl IssueBuilder {
    pub fn new(
        id: impl Into<String>,
        identifier: impl Into<String>,
        title: impl Into<String>,
        state: impl Into<String>,
    ) -> Self {
        Self {
            issue: Issue {
                id: id.into(),
                identifier: identifier.into(),
                title: title.into(),
                description: None,
                url: None,
                state: state.into(),
                priority: None,
                assignees: Vec::new(),
                labels: Vec::new(),
                created_at: None,
                updated_at: None,
                parent_id: None,
                blocked_by: Vec::new(),
                project_name: None,
                project_slug: None,
                metadata: BTreeMap::new(),
            },
        }
    }

    pub fn description(mut self, value: Option<String>) -> Self {
        self.issue.description = value;
        self
    }

    pub fn url(mut self, value: Option<String>) -> Self {
        self.issue.url = value;
        self
    }

    pub fn priority(mut self, value: Option<i32>) -> Self {
        self.issue.priority = value;
        self
    }

    pub fn assignees(mut self, value: Vec<String>) -> Self {
        self.issue.assignees = value;
        self
    }

    pub fn labels(mut self, value: Vec<String>) -> Self {
        self.issue.labels = value;
        self
    }

    pub fn created_at(mut self, value: Option<DateTime<Utc>>) -> Self {
        self.issue.created_at = value;
        self
    }

    pub fn updated_at(mut self, value: Option<DateTime<Utc>>) -> Self {
        self.issue.updated_at = value;
        self
    }

    pub fn parent_id(mut self, value: Option<String>) -> Self {
        self.issue.parent_id = value;
        self
    }

    pub fn blocked_by(mut self, value: Vec<String>) -> Self {
        self.issue.blocked_by = value;
        self
    }

    pub fn project_name(mut self, value: Option<String>) -> Self {
        self.issue.project_name = value;
        self
    }

    pub fn project_slug(mut self, value: Option<String>) -> Self {
        self.issue.project_slug = value;
        self
    }

    pub fn metadata(mut self, value: BTreeMap<String, serde_json::Value>) -> Self {
        self.issue.metadata = value;
        self
    }

    pub fn metadata_entry(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.issue.metadata.insert(key.into(), value);
        self
    }

    pub fn build(self) -> Issue {
        self.issue
    }
}

/// Read-only view over issues. Sync because fs reads are cheap; for network-backed
/// implementations the sync methods use `block_in_place` internally.
pub trait Tracker: Send + Sync {
    /// All issues whose state is in `active_states`. Implementations MUST skip
    /// issues that are blocked by any non-terminal issue.
    fn poll_candidates(&self) -> Result<Vec<Issue>>;
    /// Current state of the given issue ids (by id or identifier). Missing ids
    /// are simply omitted from the result.
    fn fetch_states(&self, ids: &[String]) -> Result<Vec<Issue>>;
    /// All issues whose state is in `terminal_states`.
    fn fetch_terminal(&self) -> Result<Vec<Issue>>;
    /// One issue by id or identifier; `None` if not found.
    fn fetch_one(&self, id: &str) -> Result<Option<Issue>>;
    /// Safety/parking write: move the issue to the configured needs-human state
    /// and add a comment explaining why the orchestrator parked it.
    fn park_issue_needs_human(&self, issue: &Issue, comment: &str) -> Result<()> {
        let _ = (issue, comment);
        bail!("tracker does not support needs-human safety writes")
    }
    /// Minimum rate-limit requests remaining seen since startup.
    /// Returns `None` when rate-limit tracking is not applicable (e.g. FileTracker).
    fn rate_limit_remaining(&self) -> Option<i64> {
        None
    }
    /// Whether the orchestrator should apply the local v0 candidate sort.
    /// Linear preserves API/native order.
    fn sort_candidates_locally(&self) -> bool {
        false
    }
    /// Re-resolve any cached auth secret from the environment and swap it in
    /// place, without rebuilding the tracker. Called after the agent rotates
    /// its `.env` and triggers a secret reload. Returns `true` when the cached
    /// secret changed. Default: no cached secret, nothing to do.
    fn reload_secrets(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone)]
pub struct TrackerBuildConfig {
    pub root: PathBuf,
    pub config_path: Option<PathBuf>,
    pub active_states: Vec<String>,
    pub terminal_states: Vec<String>,
    pub project_slug: Option<String>,
    pub endpoint: Option<String>,
    pub needs_human: Option<String>,
    pub team: Option<String>,
    pub assignee: Option<String>,
    pub delegate: Option<String>,
    pub labels: Vec<String>,
}

pub trait TrackerFactory: Send + Sync {
    fn build(&self, cfg: TrackerBuildConfig) -> Result<Arc<dyn Tracker>>;
}
