//! `LinearTracker`: polls Linear's GraphQL API scoped to a project by slugId.
//!
//! Rate-limit handling:
//! - Reads `x-ratelimit-requests-remaining` and `x-ratelimit-requests-reset`
//!   headers on every response.
//! - When remaining ≤ 0 after a response: sleeps until reset + 1 s.
//! - On HTTP 429: sleeps for `Retry-After` seconds (or 60 s) then retries once.
//! - Tracks the minimum remaining seen for the dashboard RATE LIMIT stat.
//!
//! Blocked-issue skipping: `poll_candidates` fetches ALL project issues,
//! builds a state-lookup map, and omits any active issue whose `blockedBy`
//! list contains at least one non-terminal issue.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use super::Tracker;
use crate::domain::Issue;

const DEFAULT_ENDPOINT: &str = "https://api.linear.app/graphql";
/// Initial sentinel: no real observation yet.
const UNSET_MIN: i64 = i64::MAX;
/// Page size for GraphQL pagination.
const PAGE_SIZE: u64 = 50;

pub struct LinearTrackerConfig {
    pub endpoint: String,
    pub api_key: String,
    pub project_slug: String,
    pub active_states: Vec<String>,
    pub terminal_states: Vec<String>,
}

pub struct LinearTracker {
    client: reqwest::Client,
    endpoint: String,
    api_key: String,
    project_slug: String,
    active: Vec<String>,
    terminal: Vec<String>,
    /// Minimum `x-ratelimit-requests-remaining` observed across all requests.
    min_remaining: Arc<AtomicI64>,
}

impl LinearTracker {
    pub fn new(cfg: LinearTrackerConfig) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("building reqwest client for LinearTracker")?;
        Ok(Self {
            client,
            endpoint: if cfg.endpoint.is_empty() {
                DEFAULT_ENDPOINT.to_string()
            } else {
                cfg.endpoint
            },
            api_key: cfg.api_key,
            project_slug: cfg.project_slug,
            active: cfg.active_states,
            terminal: cfg.terminal_states,
            min_remaining: Arc::new(AtomicI64::new(UNSET_MIN)),
        })
    }

    // --- async internals ---

    /// Fetch every issue in the project (all states), paginated.
    async fn fetch_all_issues_async(&self) -> Result<Vec<RawIssue>> {
        let mut all: Vec<RawIssue> = Vec::new();
        let mut cursor: Option<String> = None;

        loop {
            let page = self.fetch_page_async(cursor.as_deref()).await?;
            all.extend(page.issues);
            if page.has_next_page {
                cursor = page.end_cursor;
            } else {
                break;
            }
        }
        Ok(all)
    }

    async fn fetch_page_async(&self, after: Option<&str>) -> Result<IssuePage> {
        // Language note: Linear uses `after: String` (nullable String, not
        // String!) so we pass null when absent.
        let query = r#"
query AgentropyCandidates($slug: String!, $after: String, $first: Int!) {
  projects(filter: { slugId: { eq: $slug } }, first: 1) {
    nodes {
      name
      slugId
      issues(first: $first, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id
          identifier
          title
          description
          url
          priority
          createdAt
          updatedAt
          state { name type }
          labels { nodes { name } }
          parent { id identifier }
          blockedBy { nodes { id identifier } }
        }
      }
    }
  }
}
"#;

        let vars = json!({
            "slug": self.project_slug,
            "after": after,
            "first": PAGE_SIZE,
        });

        let body = json!({ "query": query, "variables": vars });
        let response = self.send_with_rate_limit_async(body).await?;

        let project_node = response
            .pointer("/data/projects/nodes/0")
            .ok_or_else(|| anyhow!("project {:?} not found in Linear response", self.project_slug))?;

        let project_name = project_node["name"].as_str().map(String::from);
        let project_slug = project_node["slugId"].as_str().map(String::from);

        let issues_obj = project_node
            .get("issues")
            .ok_or_else(|| anyhow!("missing 'issues' node in project response"))?;

        let has_next_page = issues_obj
            .pointer("/pageInfo/hasNextPage")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let end_cursor = issues_obj
            .pointer("/pageInfo/endCursor")
            .and_then(Value::as_str)
            .map(String::from);

        let nodes = issues_obj["nodes"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        let mut issues = Vec::with_capacity(nodes.len());
        for node in nodes {
            match serde_json::from_value::<RawIssue>(node) {
                Ok(mut ri) => {
                    ri.project_name = project_name.clone();
                    ri.project_slug = project_slug.clone();
                    issues.push(ri);
                }
                Err(e) => {
                    tracing::warn!("LinearTracker: skipping unparseable issue node: {e}");
                }
            }
        }

        Ok(IssuePage {
            issues,
            has_next_page,
            end_cursor,
        })
    }

    /// Execute one GraphQL request. Handles:
    /// - Rate-limit header tracking (`x-ratelimit-requests-remaining` /
    ///   `x-ratelimit-requests-reset`).
    /// - Sleep until reset + 1 s when remaining ≤ 0.
    /// - HTTP 429: sleep for `Retry-After` (or 60 s), retry once.
    async fn send_with_rate_limit_async(&self, body: Value) -> Result<Value> {
        let resp = self
            .client
            .post(&self.endpoint)
            .header("Authorization", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("sending Linear GraphQL request")?;

        if resp.status().as_u16() == 429 {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(60);
            tracing::warn!(
                retry_after_secs = retry_after,
                "Linear rate limited (429); sleeping before retry"
            );
            tokio::time::sleep(Duration::from_secs(retry_after)).await;

            // Retry once.
            let resp2 = self
                .client
                .post(&self.endpoint)
                .header("Authorization", &self.api_key)
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await
                .context("sending Linear GraphQL request (retry after 429)")?;

            self.process_rate_limit_headers(&resp2.headers().clone()).await;
            let status = resp2.status();
            let text = resp2
                .text()
                .await
                .context("reading Linear response body (retry)")?;
            if !status.is_success() {
                bail!("Linear API returned HTTP {} after retry: {}", status, &text[..text.len().min(200)]);
            }
            return parse_graphql_body(&text);
        }

        // Capture headers before consuming the response body.
        let headers = resp.headers().clone();
        let status = resp.status();
        let text = resp
            .text()
            .await
            .context("reading Linear response body")?;

        self.process_rate_limit_headers(&headers).await;

        if !status.is_success() {
            bail!("Linear API returned HTTP {}: {}", status, &text[..text.len().min(200)]);
        }
        parse_graphql_body(&text)
    }

    /// Record `x-ratelimit-requests-remaining` into `min_remaining` and sleep
    /// when the bucket is exhausted.
    async fn process_rate_limit_headers(&self, headers: &reqwest::header::HeaderMap) {
        let remaining = headers
            .get("x-ratelimit-requests-remaining")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<i64>().ok());

        let reset_ts = headers
            .get("x-ratelimit-requests-reset")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<i64>().ok());

        if let Some(r) = remaining {
            // Track minimum using fetch_min (stable since Rust 1.45).
            self.min_remaining.fetch_min(r, Ordering::SeqCst);

            if r <= 0 {
                let wait_secs = reset_ts
                    .map(|ts| (ts - Utc::now().timestamp() + 1).max(0) as u64)
                    .unwrap_or(60);
                if wait_secs > 0 {
                    tracing::warn!(
                        wait_secs,
                        "Linear rate limit exhausted; sleeping until bucket resets"
                    );
                    tokio::time::sleep(Duration::from_secs(wait_secs)).await;
                }
            }
        }
    }

    // --- sync wrappers (bridge to the sync Tracker trait) ---

    fn run_async<F, T>(&self, fut: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(fut)
        })
    }

    fn poll_candidates_inner(&self) -> Result<Vec<Issue>> {
        self.run_async(self.do_poll_candidates())
    }

    async fn do_poll_candidates(&self) -> Result<Vec<Issue>> {
        let raw = self.fetch_all_issues_async().await?;

        // Build state lookup: identifier -> state_name
        let state_map: std::collections::HashMap<&str, &str> = raw
            .iter()
            .map(|r| (r.identifier.as_str(), r.state.name.as_str()))
            .collect();

        let mut out = Vec::new();
        for r in &raw {
            if !self.active.contains(&r.state.name) {
                continue;
            }
            // Skip if any fetched blocker is not terminal.
            let blocked = r.blocked_by.nodes.iter().any(|b| {
                state_map
                    .get(b.identifier.as_str())
                    .map(|s| !self.terminal.contains(&s.to_string()))
                    .unwrap_or(false) // unknown / cross-project blocker = not blocking
            });
            if !blocked {
                out.push(raw_to_issue(r));
            }
        }
        Ok(out)
    }

    fn fetch_all_inner(&self) -> Result<Vec<Issue>> {
        self.run_async(async {
            let raw = self.fetch_all_issues_async().await?;
            Ok(raw.iter().map(raw_to_issue).collect())
        })
    }
}

impl Tracker for LinearTracker {
    fn poll_candidates(&self) -> Result<Vec<Issue>> {
        self.poll_candidates_inner()
    }

    fn fetch_states(&self, ids: &[String]) -> Result<Vec<Issue>> {
        let all = self.fetch_all_inner()?;
        Ok(all
            .into_iter()
            .filter(|i| ids.contains(&i.id) || ids.contains(&i.identifier))
            .collect())
    }

    fn fetch_terminal(&self) -> Result<Vec<Issue>> {
        let all = self.fetch_all_inner()?;
        Ok(all
            .into_iter()
            .filter(|i| self.terminal.contains(&i.state))
            .collect())
    }

    fn fetch_one(&self, id: &str) -> Result<Option<Issue>> {
        let all = self.fetch_all_inner()?;
        Ok(all.into_iter().find(|i| i.id == id || i.identifier == id))
    }

    fn rate_limit_remaining(&self) -> Option<i64> {
        let v = self.min_remaining.load(Ordering::SeqCst);
        if v == UNSET_MIN {
            None
        } else {
            Some(v)
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_graphql_body(text: &str) -> Result<Value> {
    let v: Value = serde_json::from_str(text).context("parsing Linear GraphQL JSON response")?;
    if let Some(errors) = v.get("errors") {
        if !errors.as_array().map(|a| a.is_empty()).unwrap_or(true) {
            bail!("Linear GraphQL errors: {}", errors);
        }
    }
    Ok(v)
}

fn raw_to_issue(r: &RawIssue) -> Issue {
    Issue {
        id: r.id.clone(),
        identifier: r.identifier.clone(),
        title: r.title.clone(),
        description: r.description.clone(),
        url: r.url.clone(),
        state: r.state.name.clone(),
        priority: r.priority,
        assignees: Vec::new(),
        labels: r.labels.nodes.iter().map(|l| l.name.clone()).collect(),
        created_at: r.created_at,
        updated_at: r.updated_at,
        parent_id: r.parent.as_ref().map(|p| p.identifier.clone()),
        blocked_by: r
            .blocked_by
            .nodes
            .iter()
            .map(|b| b.identifier.clone())
            .collect(),
        project_name: r.project_name.clone(),
        project_slug: r.project_slug.clone(),
    }
}

// ---------------------------------------------------------------------------
// Serde structs for Linear GraphQL response
// ---------------------------------------------------------------------------

struct IssuePage {
    issues: Vec<RawIssue>,
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawIssue {
    id: String,
    identifier: String,
    title: String,
    description: Option<String>,
    url: Option<String>,
    priority: Option<i32>,
    #[serde(rename = "createdAt")]
    created_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(rename = "updatedAt")]
    updated_at: Option<chrono::DateTime<chrono::Utc>>,
    state: RawState,
    labels: RawLabelConnection,
    parent: Option<RawRef>,
    #[serde(rename = "blockedBy", default)]
    blocked_by: RawRefConnection,
    // injected after deserialization
    #[serde(skip)]
    project_name: Option<String>,
    #[serde(skip)]
    project_slug: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawState {
    name: String,
    #[allow(dead_code)]
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Debug, Default, Deserialize)]
struct RawLabelConnection {
    nodes: Vec<RawLabel>,
}

#[derive(Debug, Deserialize)]
struct RawLabel {
    name: String,
}

/// Wrapper for GraphQL connection fields that return `{ nodes: [...] }`.
#[derive(Debug, Default, Deserialize)]
struct RawRefConnection {
    nodes: Vec<RawRef>,
}

#[derive(Debug, Deserialize)]
struct RawRef {
    #[allow(dead_code)]
    id: String,
    identifier: String,
}
