//! Integration tests link `cap-tracker` as an external crate: passing here is
//! the proof that the `#[non_exhaustive]` `Issue` (no struct-literal allowed
//! outside the crate) is constructible through the builder alone.

use std::collections::BTreeMap;

use anyhow::Result;
use cap_tracker::{Issue, Tracker};
use chrono::{TimeZone, Utc};

#[test]
fn issue_builder_round_trips_all_fields() {
    let created = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
    let updated = Utc.with_ymd_and_hms(2026, 1, 3, 6, 7, 8).unwrap();
    let mut metadata = BTreeMap::new();
    metadata.insert("native_field".to_string(), serde_json::json!({"a": 1}));

    let issue = Issue::builder("id-1", "PROJ-42", "Fix the bug", "todo")
        .description(Some("body".into()))
        .url(Some("https://example.com/PROJ-42".into()))
        .priority(Some(2))
        .assignees(vec!["alice".into()])
        .labels(vec!["bug".into(), "p2".into()])
        .created_at(Some(created))
        .updated_at(Some(updated))
        .parent_id(Some("PROJ-1".into()))
        .blocked_by(vec!["PROJ-7".into()])
        .project_name(Some("Proj".into()))
        .project_slug(Some("proj".into()))
        .metadata(metadata.clone())
        .metadata_entry("rate_limit_remaining", serde_json::json!(99))
        .build();

    assert_eq!(issue.id, "id-1");
    assert_eq!(issue.identifier, "PROJ-42");
    assert_eq!(issue.title, "Fix the bug");
    assert_eq!(issue.state, "todo");
    assert_eq!(issue.description.as_deref(), Some("body"));
    assert_eq!(issue.url.as_deref(), Some("https://example.com/PROJ-42"));
    assert_eq!(issue.priority, Some(2));
    assert_eq!(issue.assignees, vec!["alice".to_string()]);
    assert_eq!(issue.labels, vec!["bug".to_string(), "p2".to_string()]);
    assert_eq!(issue.created_at, Some(created));
    assert_eq!(issue.updated_at, Some(updated));
    assert_eq!(issue.parent_id.as_deref(), Some("PROJ-1"));
    assert_eq!(issue.blocked_by, vec!["PROJ-7".to_string()]);
    assert_eq!(issue.project_name.as_deref(), Some("Proj"));
    assert_eq!(issue.project_slug.as_deref(), Some("proj"));
    metadata.insert("rate_limit_remaining".to_string(), serde_json::json!(99));
    assert_eq!(issue.metadata, metadata);
}

#[test]
fn issue_new_defaults_optional_fields() {
    let issue = Issue::new("id-2", "PROJ-1", "Title", "in_progress");

    assert_eq!(issue.id, "id-2");
    assert_eq!(issue.identifier, "PROJ-1");
    assert_eq!(issue.title, "Title");
    assert_eq!(issue.state, "in_progress");
    assert_eq!(issue.description, None);
    assert_eq!(issue.url, None);
    assert_eq!(issue.priority, None);
    assert!(issue.assignees.is_empty());
    assert!(issue.labels.is_empty());
    assert_eq!(issue.created_at, None);
    assert_eq!(issue.updated_at, None);
    assert_eq!(issue.parent_id, None);
    assert!(issue.blocked_by.is_empty());
    assert_eq!(issue.project_name, None);
    assert_eq!(issue.project_slug, None);
    assert!(issue.metadata.is_empty());
}

/// Minimal tracker implementing only the required methods, so the defaulted
/// trait methods are exercised as an external implementor sees them.
struct StubTracker;

impl Tracker for StubTracker {
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

#[test]
fn park_issue_needs_human_errors_by_default() {
    let issue = Issue::new("id-1", "PROJ-42", "Title", "todo");
    let err = StubTracker
        .park_issue_needs_human(&issue, "stuck")
        .unwrap_err();
    assert!(
        err.to_string().contains("needs-human"),
        "unexpected error: {err}"
    );
}

#[test]
fn sort_candidates_locally_defaults_to_false() {
    assert!(!StubTracker.sort_candidates_locally());
}

#[test]
fn rate_limit_remaining_defaults_to_none() {
    assert_eq!(StubTracker.rate_limit_remaining(), None);
}
