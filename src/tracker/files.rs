//! `FileTracker`: reads `./issues/*.md` (Markdown with optional YAML
//! frontmatter). The filename stem is the identifier fallback. Frontmatter is
//! parsed into the `Issue`; the trailing Markdown body becomes `description`
//! when frontmatter does not set one.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::Tracker;
use crate::domain::Issue;

/// Reads issues from a directory of Markdown files. Holds the active/terminal
/// state sets so it can filter candidate and terminal issues.
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

    /// Read and parse every `*.md` file in the issues dir. Files that fail to
    /// parse are skipped with a logged warning rather than crashing the poll.
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
                return Err(e).with_context(|| {
                    format!("reading issues dir {}", self.issues_dir.display())
                });
            }
        };

        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!("skipping unreadable dir entry: {e}");
                    continue;
                }
            };
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
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
            .into_iter()
            .filter(|i| self.active.contains(&i.state))
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
        Ok(all
            .into_iter()
            .find(|i| i.id == id || i.identifier == id))
    }
}

/// YAML frontmatter helper. Every field is optional; defaults fill the gaps.
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
}

/// Parse one issue file: split optional `---`-delimited frontmatter, deserialize
/// it, and use the trimmed remainder as `description` if frontmatter omits it.
/// `identifier` falls back to the file stem; `id` falls back to `identifier`.
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

    let description = match fm.description {
        Some(d) => Some(d),
        None => {
            let trimmed = body.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
    };

    Ok(Issue {
        id,
        identifier,
        title,
        description,
        state: fm.state.unwrap_or_default(),
        priority: fm.priority,
        assignees: fm.assignees,
        labels: fm.labels,
        created_at: fm.created_at,
        updated_at: fm.updated_at,
    })
}

/// Split a leading `---\n ... \n---` frontmatter block from the body. Returns
/// `(Some(frontmatter_src), body)` when present, else `(None, whole_input)`.
fn split_frontmatter(src: &str) -> (Option<&str>, &str) {
    let rest = match src.strip_prefix("---\n") {
        Some(r) => r,
        None => match src.strip_prefix("---\r\n") {
            Some(r) => r,
            None => return (None, src),
        },
    };

    // Walk lines to find the closing "---" delimiter.
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
    // Final line without trailing newline.
    if rest[offset..].trim_end() == "---" {
        return (Some(&rest[..offset]), "");
    }

    // Unterminated frontmatter: no valid split, treat as plain body.
    (None, src)
}
