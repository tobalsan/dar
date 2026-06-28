//! Bridge between the pure `system-files` resolver and the on-bus payload.
//!
//! Loads the agent's declared `system_files` from `agent.yaml`, resolves
//! `AGENTS.md` + those entries, appends any workspace `skills/` block, and
//! projects the result into the retained [`system_files::bus::SystemContext`]
//! payload. Non-fatal warnings are logged; a hard resolution error (missing
//! `required` file or containment violation) is logged and collapses to an
//! empty context so boot continues — preflight/`doctor` are the gates that fail.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use system_files::bus::SystemContext;
use system_files::{ResolveError, SystemFileEntry};

/// Minimal view of `agent.yaml`: only the `system_files` key this extension
/// needs. Decoupled from the orchestrator's full config tree so the substrate
/// stays independent of the loop. Unknown keys are ignored.
#[derive(Debug, Default, Deserialize)]
struct SystemFilesConfig {
    #[serde(default)]
    system_files: Option<Vec<SystemFileEntry>>,
}

fn load_entries(root: &Path) -> Option<Vec<SystemFileEntry>> {
    let path = root.join("agent.yaml");
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) => {
            tracing::warn!("reading {} for system files: {e}", path.display());
            return None;
        }
    };
    match serde_yaml::from_str::<SystemFilesConfig>(&raw) {
        Ok(cfg) => cfg.system_files,
        Err(e) => {
            tracing::error!("parsing {} for system files: {e}", path.display());
            None
        }
    }
}

/// Resolve the agent's system context into the retained-topic payload.
///
/// Warnings are logged; a hard error is logged and collapses to an empty
/// context (boot continues — preflight is the gate that rejects bad config).
pub fn resolve_for(root: &Path) -> SystemContext {
    let entries = load_entries(root);
    match resolve(root, entries.as_deref()) {
        Ok(ctx) => ctx,
        Err(e) => {
            tracing::error!("resolving system context: {e}");
            SystemContext::default()
        }
    }
}

/// Resolve into the contract payload, surfacing any [`ResolveError`].
pub fn resolve(
    root: &Path,
    entries: Option<&[SystemFileEntry]>,
) -> Result<SystemContext, ResolveError> {
    let resolved = system_files::resolve(root, entries)?;
    for warning in &resolved.warnings {
        tracing::warn!("system file: {warning}");
    }

    let mut payload: SystemContext = resolved.into();
    append_workspace_skills(root, &mut payload.text);
    Ok(payload)
}

#[derive(Debug, Deserialize)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
    #[serde(default, rename = "disable-model-invocation")]
    disable_model_invocation: bool,
}

fn append_workspace_skills(root: &Path, text: &mut String) {
    let skills = discover_workspace_skills(root);
    if skills.is_empty() {
        return;
    }

    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("\nThe following skills provide specialized instructions for specific tasks.\n");
    text.push_str(
        "Use the read tool to load a skill's file when the task matches its description.\n",
    );
    text.push_str("When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.\n\n");
    text.push_str("<available_skills>\n");
    for skill in skills {
        text.push_str("  <skill>\n");
        text.push_str(&format!("    <name>{}</name>\n", escape_xml(&skill.name)));
        text.push_str(&format!(
            "    <description>{}</description>\n",
            escape_xml(&skill.description)
        ));
        text.push_str(&format!(
            "    <location>{}</location>\n",
            escape_xml(&skill.path.display().to_string())
        ));
        text.push_str("  </skill>\n");
    }
    text.push_str("</available_skills>\n");
}

#[derive(Debug, PartialEq, Eq)]
struct WorkspaceSkill {
    name: String,
    description: String,
    path: PathBuf,
}

fn discover_workspace_skills(root: &Path) -> Vec<WorkspaceSkill> {
    let skills_dir = root.join("skills");
    let Ok(entries) = fs::read_dir(&skills_dir) else {
        return Vec::new();
    };

    let mut skills = Vec::new();
    for entry in entries.flatten() {
        let skill_md = entry.path().join("SKILL.md");
        if !skill_md.is_file() {
            continue;
        }
        match load_workspace_skill(&skill_md) {
            Ok(Some(skill)) => skills.push(skill),
            Ok(None) => {}
            Err(e) => tracing::warn!("skill file: {}: {e}", skill_md.display()),
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
    skills
}

fn load_workspace_skill(path: &Path) -> anyhow::Result<Option<WorkspaceSkill>> {
    let raw = fs::read_to_string(path)?;
    let Some(frontmatter) = raw.strip_prefix("---\n") else {
        return Ok(None);
    };
    let Some((yaml, _body)) = frontmatter.split_once("\n---") else {
        return Ok(None);
    };
    let fm: SkillFrontmatter = serde_yaml::from_str(yaml)?;
    if fm.disable_model_invocation {
        return Ok(None);
    }
    let Some(description) = fm.description.filter(|s| !s.trim().is_empty()) else {
        return Ok(None);
    };
    let name = fm.name.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| {
        path.parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("skill")
            .to_string()
    });
    Ok(Some(WorkspaceSkill {
        name,
        description,
        path: path.to_path_buf(),
    }))
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(dir: &Path, rel: &str, contents: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn resolves_agents_md_into_payload() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "identity").unwrap();

        let ctx = resolve(dir.path(), None).unwrap();

        assert_eq!(ctx.files.len(), 1);
        assert_eq!(ctx.files[0].path, "AGENTS.md");
        assert!(ctx.text.contains("identity"));
    }

    #[test]
    fn appends_workspace_skills_to_system_context() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "identity").unwrap();
        let skill_dir = dir.path().join("skills").join("refactor");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: refactor\ndescription: Improve code shape\n---\nBody",
        )
        .unwrap();

        let ctx = resolve(dir.path(), None).unwrap();

        assert!(ctx.text.contains("<available_skills>"), "{}", ctx.text);
        assert!(ctx.text.contains("<name>refactor</name>"), "{}", ctx.text);
        assert!(
            ctx.text
                .contains("<description>Improve code shape</description>"),
            "{}",
            ctx.text
        );
        assert!(
            ctx.text.contains(&format!(
                "<location>{}</location>",
                skill_dir.join("SKILL.md").display()
            )),
            "{}",
            ctx.text
        );
    }

    #[test]
    fn skips_disable_model_invocation_skills() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("skills").join("hidden");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: hidden\ndescription: Hidden\ndisable-model-invocation: true\n---\nBody",
        )
        .unwrap();

        let ctx = resolve(dir.path(), None).unwrap();

        assert!(!ctx.text.contains("<available_skills>"), "{}", ctx.text);
    }

    #[test]
    fn missing_required_surfaces_error() {
        let dir = TempDir::new().unwrap();
        let entries = vec![SystemFileEntry::Detailed {
            path: "missing.md".to_string(),
            required: true,
        }];
        let err = resolve(dir.path(), Some(&entries)).unwrap_err();
        assert!(matches!(err, ResolveError::MissingRequired { .. }));
    }

    #[test]
    fn resolve_for_degrades_to_empty_on_required_missing() {
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            "agent.yaml",
            "id: a\nname: A\nrunner:\n  use: fake\nsystem_files:\n  - path: nope.md\n    required: true\n",
        );
        let ctx = resolve_for(dir.path());
        assert!(ctx.is_empty());
    }

    #[test]
    fn resolve_for_reads_system_files_from_agent_yaml() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "AGENTS.md", "identity");
        write(dir.path(), "SOUL.md", "soul body");
        write(
            dir.path(),
            "agent.yaml",
            "id: a\nname: A\nrunner:\n  use: fake\nsystem_files:\n  - SOUL.md\n",
        );

        let ctx = resolve_for(dir.path());

        let order: Vec<_> = ctx.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(order, vec!["AGENTS.md", "SOUL.md"]);
        assert!(ctx.text.contains("identity"));
        assert!(ctx.text.contains("soul body"));
    }

    #[test]
    fn resolve_for_passive_config_without_loop_publishes_context() {
        // Passive agent: no tracker/orchestrator/workspace trio. The substrate
        // must still resolve AGENTS.md + configured system_files.
        let dir = TempDir::new().unwrap();
        write(dir.path(), "AGENTS.md", "i am passive");
        write(dir.path(), "SOUL.md", "my soul");
        write(
            dir.path(),
            "agent.yaml",
            "id: passive\nname: Passive\nrunner:\n  use: pi\nsystem_files:\n  - SOUL.md\n",
        );

        let ctx = resolve_for(dir.path());

        assert!(!ctx.is_empty());
        let order: Vec<_> = ctx.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(order, vec!["AGENTS.md", "SOUL.md"]);
        assert!(ctx.text.contains("i am passive"));
        assert!(ctx.text.contains("my soul"));
    }

    #[test]
    fn resolve_for_missing_agent_yaml_is_empty() {
        let dir = TempDir::new().unwrap();
        let ctx = resolve_for(dir.path());
        assert!(ctx.is_empty());
    }

    #[test]
    fn resolve_for_agent_yaml_without_system_files_resolves_agents_md_only() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "AGENTS.md", "identity");
        write(dir.path(), "ignored.md", "not referenced");
        write(
            dir.path(),
            "agent.yaml",
            "id: a\nname: A\nrunner:\n  use: fake\n",
        );

        let ctx = resolve_for(dir.path());

        assert_eq!(ctx.files.len(), 1);
        assert_eq!(ctx.files[0].path, "AGENTS.md");
    }
}
