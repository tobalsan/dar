//! Renders `WORKFLOW.md` through minijinja with STRICT undefined behavior.
//!
//! The template body (frontmatter stripped) is rendered against `{{ issue.* }}`.
//! Under strict mode, accessing a genuinely-absent key (e.g. `{{ issue.nope }}`)
//! returns an error, which the runner must treat as a failed attempt (it must
//! not spawn the child). Note minijinja distinguishes "undefined" from "none":
//! an `Option::None` field serializes to a defined-as-none value, so
//! `{{ issue.description }}` on a `None` does NOT error.

use std::path::Path;

use anyhow::{Context, Result};
use minijinja::{Environment, UndefinedBehavior};

use crate::domain::Issue;

/// Holds the workflow template source (frontmatter already stripped).
pub struct PromptRenderer {
    template_src: String,
}

impl PromptRenderer {
    /// Read `WORKFLOW.md`, strip optional leading YAML frontmatter, and keep the
    /// Markdown body as the prompt template.
    pub fn load(workflow_md: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(workflow_md)
            .with_context(|| format!("reading WORKFLOW.md at {}", workflow_md.display()))?;
        let body = strip_frontmatter(&raw);
        Ok(Self {
            template_src: body.to_string(),
        })
    }

    /// Render the template against a single issue under strict-undefined mode.
    /// Returns an error if the template references an absent variable.
    pub fn render(&self, issue: &Issue) -> Result<String> {
        let mut env = Environment::new();
        env.set_undefined_behavior(UndefinedBehavior::Strict);
        env.add_template("workflow", &self.template_src)
            .context("compiling WORKFLOW.md template")?;
        let tmpl = env
            .get_template("workflow")
            .context("loading compiled WORKFLOW.md template")?;
        let out = tmpl
            .render(minijinja::context! { issue => issue.for_template() })
            .context("rendering WORKFLOW.md (strict-undefined)")?;
        Ok(out)
    }
}

/// If the source begins with a `---\n ... \n---` YAML frontmatter block, return
/// only the body that follows it; otherwise return the source unchanged.
fn strip_frontmatter(src: &str) -> &str {
    // Frontmatter must be at the very start: a line that is exactly "---".
    let rest = match src.strip_prefix("---\n") {
        Some(r) => r,
        None => match src.strip_prefix("---\r\n") {
            Some(r) => r,
            None => return src,
        },
    };

    // Find the closing delimiter line ("---" on its own line).
    if let Some(end) = find_closing_delim(rest) {
        rest[end..].trim_start_matches(['\r', '\n']).trim_start()
    } else {
        // Unterminated frontmatter: treat the whole input as body to avoid
        // silently dropping content.
        src
    }
}

/// Returns the byte offset in `s` just past a closing `---` delimiter line, or
/// `None` if there is no closing delimiter.
fn find_closing_delim(s: &str) -> Option<usize> {
    let mut offset = 0usize;
    for line in s.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" {
            return Some(offset + line.len());
        }
        offset += line.len();
    }
    // Handle a final line with no trailing newline.
    if s[offset..].trim_end() == "---" {
        return Some(s.len());
    }
    None
}
