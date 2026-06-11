//! File-backed tracker extension.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use cap_tracker::{Issue, Tracker, TrackerBuildConfig, TrackerFactory};
use chrono::{DateTime, Utc};
use host_api::{Extension, RegisterCtx};
use serde::Deserialize;

pub struct TrackerFilesExtension;

impl Extension for TrackerFilesExtension {
    fn id(&self) -> &'static str {
        "tracker-files"
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let factory: Arc<dyn TrackerFactory> = Arc::new(FileTrackerFactory);
            ctx.services
                .service::<dyn TrackerFactory>("files", factory)?;
            Ok(())
        })
    }
}

struct FileTrackerFactory;

impl TrackerFactory for FileTrackerFactory {
    fn build(&self, cfg: TrackerBuildConfig) -> Result<Arc<dyn Tracker>> {
        let issues_dir = cfg
            .config_path
            .context("tracker.config.path is required when tracker.use is \"files\"")?
            .to_owned();
        let issues_dir = cfg.root.join(issues_dir);
        Ok(Arc::new(FileTracker::new(
            issues_dir,
            cfg.active_states,
            cfg.terminal_states,
        )))
    }
}

pub struct FileTracker {
    issues_dir: PathBuf,
    active: Vec<String>,
    terminal: Vec<String>,
}

impl FileTracker {
    pub fn new(issues_dir: PathBuf, active: Vec<String>, terminal: Vec<String>) -> Self {
        Self {
            issues_dir,
            active,
            terminal,
        }
    }

    fn read_all(&self) -> Result<Vec<Issue>> {
        let mut out = Vec::new();

        let entries = match std::fs::read_dir(&self.issues_dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(
                    "issues dir {} does not exist; no candidates",
                    self.issues_dir.display()
                );
                return Ok(out);
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("reading issues dir {}", self.issues_dir.display()));
            }
        };

        let mut paths = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!("skipping unreadable dir entry: {e}");
                    continue;
                }
            };
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            paths.push(path);
        }
        paths.sort();

        for path in paths {
            match parse_issue_file(&path) {
                Ok(issue) => out.push(issue),
                Err(e) => {
                    tracing::warn!("skipping unparseable issue file {}: {e:#}", path.display());
                }
            }
        }

        Ok(out)
    }
}

impl Tracker for FileTracker {
    fn poll_candidates(&self) -> Result<Vec<Issue>> {
        let all = self.read_all()?;
        Ok(all
            .iter()
            .filter(|issue| {
                self.active.contains(&issue.state)
                    && issue.blocked_by.iter().all(|blocker| {
                        all_issue_state(&all, blocker)
                            .map(|state| self.terminal.contains(state))
                            .unwrap_or(false)
                    })
            })
            .cloned()
            .collect())
    }

    fn fetch_states(&self, ids: &[String]) -> Result<Vec<Issue>> {
        let all = self.read_all()?;
        Ok(all
            .into_iter()
            .filter(|i| ids.contains(&i.id) || ids.contains(&i.identifier))
            .collect())
    }

    fn fetch_terminal(&self) -> Result<Vec<Issue>> {
        let all = self.read_all()?;
        Ok(all
            .into_iter()
            .filter(|i| self.terminal.contains(&i.state))
            .collect())
    }

    fn fetch_one(&self, id: &str) -> Result<Option<Issue>> {
        let all = self.read_all()?;
        Ok(all.into_iter().find(|i| i.id == id || i.identifier == id))
    }

    fn sort_candidates_locally(&self) -> bool {
        true
    }
}

fn all_issue_state<'a>(issues: &'a [Issue], id: &str) -> Option<&'a String> {
    issues
        .iter()
        .find(|issue| issue.id == id || issue.identifier == id)
        .map(|issue| &issue.state)
}

#[derive(Debug, Default, Deserialize)]
struct Frontmatter {
    id: Option<String>,
    identifier: Option<String>,
    title: Option<String>,
    description: Option<String>,
    state: Option<String>,
    priority: Option<i32>,
    #[serde(default)]
    assignees: Vec<String>,
    #[serde(default)]
    labels: Vec<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    parent_id: Option<String>,
    #[serde(default)]
    blocked_by: Vec<String>,
    project_name: Option<String>,
    project_slug: Option<String>,
}

fn parse_issue_file(path: &Path) -> Result<Issue> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading issue file {}", path.display()))?;

    let (fm_src, body) = split_frontmatter(&raw);
    let fm: Frontmatter = match fm_src {
        Some(src) => serde_yaml::from_str(src).context("parsing issue frontmatter")?,
        None => Frontmatter::default(),
    };

    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();

    let identifier = fm.identifier.unwrap_or_else(|| stem.clone());
    let id = fm.id.unwrap_or_else(|| identifier.clone());
    let title = fm.title.unwrap_or_else(|| identifier.clone());
    let description = fm.description.or_else(|| {
        let trimmed = body.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    });

    Ok(
        Issue::builder(id, identifier, title, fm.state.unwrap_or_default())
            .description(description)
            .priority(fm.priority)
            .assignees(fm.assignees)
            .labels(fm.labels)
            .created_at(fm.created_at)
            .updated_at(fm.updated_at)
            .parent_id(fm.parent_id)
            .blocked_by(fm.blocked_by)
            .project_name(fm.project_name)
            .project_slug(fm.project_slug)
            .build(),
    )
}

fn split_frontmatter(src: &str) -> (Option<&str>, &str) {
    let rest = match src.strip_prefix("---\n") {
        Some(r) => r,
        None => match src.strip_prefix("---\r\n") {
            Some(r) => r,
            None => return (None, src),
        },
    };

    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" {
            let fm = &rest[..offset];
            let body = &rest[offset + line.len()..];
            return (Some(fm), body);
        }
        offset += line.len();
    }
    if rest[offset..].trim_end() == "---" {
        return (Some(&rest[..offset]), "");
    }
    (None, src)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_frontmatter_to_issue() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ALG-1.md");
        std::fs::write(
            &path,
            "---\nid: uuid-1\nidentifier: ALG-1\ntitle: Move it\nstate: Todo\npriority: 2\nassignees: [alice]\nlabels: [backend]\nparent_id: parent-1\nblocked_by: [ALG-0]\nproject_name: Agentropy\nproject_slug: agentropy\n---\nBody text\n",
        )
        .unwrap();

        let issue = parse_issue_file(&path).unwrap();

        assert_eq!(issue.id, "uuid-1");
        assert_eq!(issue.identifier, "ALG-1");
        assert_eq!(issue.title, "Move it");
        assert_eq!(issue.description.as_deref(), Some("Body text"));
        assert_eq!(issue.state, "Todo");
        assert_eq!(issue.priority, Some(2));
        assert_eq!(issue.assignees, vec!["alice"]);
        assert_eq!(issue.labels, vec!["backend"]);
        assert_eq!(issue.parent_id.as_deref(), Some("parent-1"));
        assert_eq!(issue.blocked_by, vec!["ALG-0"]);
        assert_eq!(issue.project_name.as_deref(), Some("Agentropy"));
        assert_eq!(issue.project_slug.as_deref(), Some("agentropy"));
    }

    #[test]
    fn poll_candidates_filters_active_and_blocked_by_non_terminal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("ALG-1.md"),
            "---\nstate: Todo\nblocked_by: [ALG-2]\n---\nblocked\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("ALG-2.md"), "---\nstate: Todo\n---\n").unwrap();
        std::fs::write(dir.path().join("ALG-3.md"), "---\nstate: Todo\n---\n").unwrap();
        std::fs::write(dir.path().join("ALG-4.md"), "---\nstate: Done\n---\n").unwrap();

        let tracker = FileTracker::new(
            dir.path().to_path_buf(),
            vec!["Todo".to_string()],
            vec!["Done".to_string()],
        );

        let candidates = tracker.poll_candidates().unwrap();

        assert_eq!(
            candidates
                .into_iter()
                .map(|issue| issue.identifier)
                .collect::<Vec<_>>(),
            vec!["ALG-2", "ALG-3"]
        );
    }

    #[test]
    fn poll_candidates_treats_missing_blockers_as_blocking() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("ALG-1.md"),
            "---\nstate: Todo\nblocked_by: [ALG-0]\n---\nblocked\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("ALG-2.md"), "---\nstate: Todo\n---\n").unwrap();

        let tracker = FileTracker::new(
            dir.path().to_path_buf(),
            vec!["Todo".to_string()],
            vec!["Done".to_string()],
        );

        let candidates = tracker.poll_candidates().unwrap();

        assert_eq!(
            candidates
                .into_iter()
                .map(|issue| issue.identifier)
                .collect::<Vec<_>>(),
            vec!["ALG-2"]
        );
    }
}
