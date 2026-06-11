//! Renders `WORKFLOW.md` through minijinja with STRICT undefined behavior.
//!
//! Loads the WORKFLOW.md frontmatter into a `WorkflowSnapshot`, exposes
//! `{{ issue.* }}` and `{{ attempt }}` in the template context, and appends an
//! orchestrator/Linear context block after the rendered body.
//!
//! ## File watching
//!
//! `maybe_reload` compares the file's mtime on each call (typically once per
//! orchestrator tick). When the file has changed:
//!
//! - Successful re-parse → new snapshot cached; returns `Ok(true)`.
//! - Parse error + `allow_stale` (default `true`) → warning logged, stale
//!   snapshot kept, returns `Ok(false)`.
//! - Parse error + `allow_stale = false` → returns `Err`.
//!
//! Under strict undefined-behavior mode, any template variable that is not
//! present in the context (`{{ issue.nope }}`) causes `render` to return `Err`,
//! which the orchestrator treats as an abnormal attempt and schedules for
//! backoff retry.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use minijinja::{Environment, UndefinedBehavior};

use crate::domain::Issue;
use crate::workflow_config::{
    parse_workflow_md, WfLinearConfig, WfTrackerConfig, WorkflowSnapshot,
};

/// Renders the WORKFLOW.md prompt template for one issue attempt.
///
/// Holds the current best `WorkflowSnapshot` and the file path + last mtime
/// for mtime-based reload detection. `allow_stale` is re-read from the
/// snapshot on each reload so it can itself be changed by editing WORKFLOW.md.
pub struct PromptRenderer {
    path: PathBuf,
    snapshot: WorkflowSnapshot,
    last_mtime: Option<SystemTime>,
}

impl PromptRenderer {
    /// Read `WORKFLOW.md`, parse frontmatter + body, and build a renderer.
    pub fn load(workflow_md: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(workflow_md)
            .with_context(|| format!("reading WORKFLOW.md at {}", workflow_md.display()))?;
        let snapshot = parse_workflow_md(&raw)
            .with_context(|| format!("parsing WORKFLOW.md at {}", workflow_md.display()))?;
        let last_mtime = std::fs::metadata(workflow_md)
            .ok()
            .and_then(|m| m.modified().ok());
        Ok(Self {
            path: workflow_md.to_path_buf(),
            snapshot,
            last_mtime,
        })
    }

    /// The current (possibly stale) snapshot.
    pub fn snapshot(&self) -> &WorkflowSnapshot {
        &self.snapshot
    }

    /// Check whether WORKFLOW.md has changed on disk (mtime comparison). If it
    /// has, attempt to re-parse:
    ///
    /// - Successful re-parse: updates snapshot, returns `Ok(true)`.
    /// - Parse error + allow_stale: logs warning, keeps stale snapshot, returns `Ok(false)`.
    /// - Parse error + !allow_stale: returns `Err`.
    /// - No change: returns `Ok(false)`.
    pub fn maybe_reload(&mut self) -> Result<bool> {
        // Read current mtime; treat metadata errors as "changed" so we surface
        // the read error downstream.
        let meta = match std::fs::metadata(&self.path) {
            Ok(m) => m,
            Err(e) => {
                if self.allow_stale() {
                    crate::logging::ev(
                        "-",
                        "workflow_reload",
                        &format!("metadata error (stale snapshot kept): {e:#}"),
                    );
                    return Ok(false);
                }
                return Err(anyhow::anyhow!("WORKFLOW.md unreadable: {e:#}"));
            }
        };

        let mtime = meta.modified().ok();
        if mtime == self.last_mtime {
            return Ok(false);
        }

        // File changed — try to reload.
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(r) => r,
            Err(e) => {
                if self.allow_stale() {
                    crate::logging::ev(
                        "-",
                        "workflow_reload",
                        &format!("read error (stale snapshot kept): {e:#}"),
                    );
                    return Ok(false);
                }
                return Err(anyhow::anyhow!("WORKFLOW.md read error: {e:#}"));
            }
        };

        match parse_workflow_md(&raw) {
            Ok(new_snapshot) => {
                self.snapshot = new_snapshot;
                self.last_mtime = mtime;
                crate::logging::ev("-", "workflow_reload", "WORKFLOW.md reloaded successfully");
                Ok(true)
            }
            Err(e) => {
                if self.allow_stale() {
                    crate::logging::ev(
                        "-",
                        "workflow_reload",
                        &format!("parse error (stale snapshot kept): {e:#}"),
                    );
                    // Advance mtime so we don't retry every tick until file changes again.
                    self.last_mtime = mtime;
                    Ok(false)
                } else {
                    Err(e.context("WORKFLOW.md parse error (allow_stale=false)"))
                }
            }
        }
    }

    /// Render the template for one issue at the given attempt number under
    /// strict undefined-behavior mode. After the rendered body, an orchestrator
    /// + Linear context block is appended.
    ///
    /// Returns `Err` if the template references an absent variable or fails to
    /// compile; the orchestrator must not spawn the child on error.
    pub fn render(&self, issue: &Issue, attempt: u32, max_retries: u32) -> Result<String> {
        let mut env = Environment::new();
        env.set_undefined_behavior(UndefinedBehavior::Strict);
        env.add_template("workflow", &self.snapshot.body)
            .context("compiling WORKFLOW.md template")?;
        let tmpl = env
            .get_template("workflow")
            .context("loading compiled WORKFLOW.md template")?;
        let mut out = tmpl
            .render(minijinja::context! {
                issue => issue.for_template(),
                attempt => attempt,
            })
            .context("rendering WORKFLOW.md (strict-undefined)")?;

        let needs_human = self
            .snapshot
            .frontmatter
            .tracker
            .as_ref()
            .and_then(needs_human_from_tracker);

        out.push_str(&build_context_appendix(
            issue,
            attempt,
            max_retries,
            needs_human.as_deref(),
            self.snapshot.frontmatter.linear.as_ref(),
        ));

        Ok(out)
    }

    // Whether to tolerate a reload parse error (default: true).
    fn allow_stale(&self) -> bool {
        self.snapshot
            .frontmatter
            .polling
            .as_ref()
            .and_then(|p| p.allow_stale)
            .unwrap_or(true)
    }
}

/// Extract the `needs_human` state name from a tracker config section,
/// checking flat form first, then legacy nested form.
fn needs_human_from_tracker(tc: &WfTrackerConfig) -> Option<String> {
    tc.needs_human
        .clone()
        .or_else(|| tc.states.as_ref().and_then(|s| s.needs_human.clone()))
}

/// Build the orchestrator + Linear context block appended after the rendered
/// template body.
fn build_context_appendix(
    issue: &Issue,
    attempt: u32,
    max_retries: u32,
    needs_human: Option<&str>,
    linear: Option<&WfLinearConfig>,
) -> String {
    let mut lines: Vec<String> = Vec::new();

    // Display attempt as 1-based for readability.
    let attempt_display = attempt + 1;
    lines.push(format!(
        "\n\n---\n**Orchestrator context** — attempt {attempt_display} of {max_retries} for `{}`",
        issue.identifier
    ));

    // Issue metadata: title, description, url (per PRD §4).
    lines.push(format!("**Title:** {}", issue.title));
    if let Some(desc) = &issue.description {
        if !desc.trim().is_empty() {
            lines.push(format!("**Description:**\n{desc}"));
        }
    }
    if let Some(url) = &issue.url {
        lines.push(format!("**URL:** {url}"));
    }

    if let Some(state) = needs_human {
        lines.push(format!(
            "If the issue cannot be resolved automatically, set `state:` to `{state}` in the issue file."
        ));
    }

    if let Some(lin) = linear {
        let has_linear = lin.project.is_some() || lin.team.is_some();
        if has_linear {
            lines.push(String::new());
            lines.push("**Linear integration**".to_string());
            if let Some(proj) = &lin.project {
                lines.push(format!("- Project: {proj}"));
            }
            if let Some(team) = &lin.team {
                lines.push(format!("- Team: {team}"));
            }
        }
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Issue;
    use tempfile::NamedTempFile;

    fn make_issue(id: &str) -> Issue {
        Issue::builder(id, id, "Test issue", "todo")
            .description(Some("Do the thing.".to_string()))
            .build()
    }

    #[test]
    fn render_basic_no_frontmatter() {
        use std::io::Write;
        let mut f = NamedTempFile::new().unwrap();
        writeln!(
            f,
            "Issue: {{{{ issue.identifier }}}} attempt={{{{ attempt }}}}"
        )
        .unwrap();
        let renderer = PromptRenderer::load(f.path()).unwrap();
        let out = renderer.render(&make_issue("PROJ-1"), 1, 3).unwrap();
        assert!(out.contains("Issue: PROJ-1 attempt=1"), "got: {out}");
        // Orchestrator context appended (attempt is 1-based: attempt+1=2 of 3).
        assert!(out.contains("attempt 2 of 3"), "got: {out}");
    }

    #[test]
    fn render_with_frontmatter_stripped() {
        use std::io::Write;
        let mut f = NamedTempFile::new().unwrap();
        write!(
            f,
            "---\ntracker:\n  needs_human: stuck\n---\nDo {{{{ issue.title }}}}"
        )
        .unwrap();
        let renderer = PromptRenderer::load(f.path()).unwrap();
        let out = renderer.render(&make_issue("T-1"), 2, 3).unwrap();
        assert!(out.contains("Do Test issue"), "got: {out}");
        assert!(
            out.contains("stuck"),
            "needs_human state in appendix; got: {out}"
        );
    }

    #[test]
    fn render_attempt_variable() {
        use std::io::Write;
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "attempt={{{{ attempt }}}}").unwrap();
        let renderer = PromptRenderer::load(f.path()).unwrap();
        let out = renderer.render(&make_issue("X-1"), 3, 5).unwrap();
        assert!(out.contains("attempt=3"), "got: {out}");
    }

    #[test]
    fn render_strict_undefined_errors() {
        use std::io::Write;
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "{{{{ issue.nonexistent_field }}}}").unwrap();
        let renderer = PromptRenderer::load(f.path()).unwrap();
        assert!(renderer.render(&make_issue("X-1"), 0, 3).is_err());
    }

    #[test]
    fn maybe_reload_detects_change() {
        use std::io::Write;
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "v1 {{{{ issue.identifier }}}}").unwrap();
        let mut renderer = PromptRenderer::load(f.path()).unwrap();

        // Force mtime change by writing again after a brief wait.
        // On macOS mtime resolution can be 1s; use a small sleep to be safe.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        {
            let mut f2 = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(f.path())
                .unwrap();
            write!(f2, "v2 {{{{ issue.identifier }}}}").unwrap();
        }

        let reloaded = renderer.maybe_reload().unwrap();
        assert!(reloaded, "should have detected mtime change");
        let out = renderer.render(&make_issue("I-1"), 0, 3).unwrap();
        assert!(out.contains("v2"), "got: {out}");
    }

    #[test]
    fn maybe_reload_stale_on_parse_error() {
        use std::io::Write;
        let mut f = NamedTempFile::new().unwrap();
        // Initial: valid YAML frontmatter with allow_stale = true (default).
        write!(f, "---\ntracker:\n  active_states: [todo]\n---\nbody").unwrap();
        let mut renderer = PromptRenderer::load(f.path()).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1100));
        {
            let mut f2 = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(f.path())
                .unwrap();
            // Invalid YAML.
            write!(f2, "---\n  bad: [unclosed\n---\nbody").unwrap();
        }

        // With allow_stale = true (default), parse error must not propagate.
        let result = renderer.maybe_reload();
        assert!(result.is_ok(), "allow_stale should swallow parse errors");
        // Snapshot unchanged: still has the original tracker config.
        assert!(renderer.snapshot().frontmatter.tracker.is_some());
    }

    #[test]
    fn appendix_includes_title_and_description() {
        use std::io::Write;
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "body").unwrap();
        let renderer = PromptRenderer::load(f.path()).unwrap();
        let mut issue = make_issue("ALG-1");
        issue.title = "My Feature".to_string();
        issue.description = Some("Do the thing.".to_string());
        let out = renderer.render(&issue, 0, 3).unwrap();
        assert!(out.contains("**Title:** My Feature"), "got: {out}");
        assert!(out.contains("**Description:**"), "got: {out}");
        assert!(out.contains("Do the thing."), "got: {out}");
    }

    #[test]
    fn appendix_includes_url_when_present() {
        use std::io::Write;
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "body").unwrap();
        let renderer = PromptRenderer::load(f.path()).unwrap();
        let mut issue = make_issue("ALG-2");
        issue.url = Some("https://linear.app/org/issue/ALG-2".to_string());
        let out = renderer.render(&issue, 0, 3).unwrap();
        assert!(
            out.contains("**URL:** https://linear.app/org/issue/ALG-2"),
            "got: {out}"
        );
    }

    #[test]
    fn appendix_omits_url_when_absent() {
        use std::io::Write;
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "body").unwrap();
        let renderer = PromptRenderer::load(f.path()).unwrap();
        let out = renderer.render(&make_issue("ALG-3"), 0, 3).unwrap();
        assert!(
            !out.contains("**URL:**"),
            "URL line should be absent; got: {out}"
        );
    }

    #[test]
    fn default_workflow_body_parses_and_renders_strict() {
        use crate::cli::DEFAULT_WORKFLOW_MD_BODY;
        use std::io::Write;

        let mut f = NamedTempFile::new().unwrap();
        write!(f, "{DEFAULT_WORKFLOW_MD_BODY}").unwrap();
        let renderer = PromptRenderer::load(f.path()).unwrap();
        let issue = make_issue("ALG-176");
        let out = renderer.render(&issue, 0, 3).unwrap();
        // The body contains {{ issue.identifier }} and {{ issue.title }} —
        // verify they were substituted (strict mode would error on unknowns).
        assert!(
            out.contains("ALG-176"),
            "identifier not substituted; got: {out}"
        );
        assert!(
            out.contains("Test issue"),
            "title not substituted; got: {out}"
        );
        assert!(
            out.contains("Orchestrator context"),
            "appendix missing; got: {out}"
        );
    }

    #[test]
    fn maybe_reload_allow_stale_camel_case_false_surfaces_error() {
        use std::io::Write;
        let mut f = NamedTempFile::new().unwrap();
        // Use camelCase allowStale=false so parse errors surface.
        write!(f, "---\npolling:\n  allowStale: false\n---\nbody").unwrap();
        let mut renderer = PromptRenderer::load(f.path()).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1100));
        {
            let mut f2 = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(f.path())
                .unwrap();
            write!(f2, "---\n  bad: [unclosed\n---\nbody").unwrap();
        }

        // allowStale=false (via camelCase alias) means parse errors must surface.
        let result = renderer.maybe_reload();
        assert!(
            result.is_err(),
            "allowStale=false should surface parse errors; got Ok"
        );
    }
}
