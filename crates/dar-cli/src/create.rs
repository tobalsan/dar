//! `dar create`: scaffold a new agent folder (agent.yaml + .gitignore).
//!
//! Fills the gap where nothing writes `agent.yaml` today: `init-build` only
//! writes the `.dar` crate + `.gitignore`, and `init-workflow` only writes
//! `WORKFLOW.md`. Works both interactively (a TTY wizard) and non-interactively
//! (flags + derived defaults).

use std::fs;
use std::io::{IsTerminal, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::cli::CreateArgs;
use crate::composer;

/// Result of a `create` run, so the caller can decide whether to also scaffold
/// `WORKFLOW.md` via the `init-workflow` host command.
#[derive(Debug)]
pub struct CreateOutcome {
    pub loop_enabled: bool,
}

/// Resolved agent settings after applying flags, prompts, and defaults.
#[derive(Debug, PartialEq)]
struct Settings {
    id: String,
    name: String,
    runner: String,
    /// Written only when `runner != "codex"` and non-empty.
    provider: Option<String>,
    /// Written only when non-empty (else the runner's own default applies).
    model: Option<String>,
    orchestrator_loop: bool,
}

/// Scaffold the agent folder at `root`. Refuses if `agent.yaml` already exists.
pub fn run(root: &Path, args: &CreateArgs) -> Result<CreateOutcome> {
    fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    let agent_yaml = root.join("agent.yaml");
    if agent_yaml.exists() {
        bail!(
            "{} already exists; refusing to overwrite",
            agent_yaml.display()
        );
    }

    // Canonicalize (the folder now exists) so `.`/`..` resolve to a real
    // basename before deriving identity; fall back to the raw path if it fails.
    let identity_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let (default_id, default_name) = derive_identity(&identity_root);
    let settings = resolve_settings(args, &default_id, &default_name)?;

    fs::write(&agent_yaml, render_agent_yaml(&settings))
        .with_context(|| format!("writing {}", agent_yaml.display()))?;
    composer::ensure_agent_gitignore(root)?;

    Ok(CreateOutcome {
        loop_enabled: settings.orchestrator_loop,
    })
}

/// Derive `(id, name)` from the config folder's basename: `id` slugified, `name`
/// titled. Falls back to `agent` / `Agent` when the basename yields no slug.
fn derive_identity(root: &Path) -> (String, String) {
    let base = root.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let id = slugify(base);
    let id = if id.is_empty() {
        "agent".to_string()
    } else {
        id
    };
    let name = titleize(&id);
    (id, name)
}

/// Apply flags, prompts (only on a TTY), and defaults to produce final settings.
fn resolve_settings(args: &CreateArgs, default_id: &str, default_name: &str) -> Result<Settings> {
    let default_runner = args.runner.clone().unwrap_or_else(|| "pi".to_string());

    if std::io::stdin().is_terminal() {
        let id = prompt("id", default_id)?;
        let name = prompt("name", default_name)?;
        let runner = prompt("runner", &default_runner)?;
        let provider = if runner == "codex" {
            None
        } else {
            non_empty(&prompt("provider", args.provider.as_deref().unwrap_or(""))?)
        };
        let model = non_empty(&prompt("model", args.model.as_deref().unwrap_or(""))?);
        let orchestrator_loop = prompt_bool("enable orchestrator loop", args.orchestrator)?;
        return Ok(Settings {
            id,
            name,
            runner,
            provider,
            model,
            orchestrator_loop,
        });
    }

    // Non-interactive: flags, then defaults. No prompts, no errors.
    let provider = if default_runner == "codex" {
        None
    } else {
        args.provider.as_deref().and_then(non_empty)
    };
    Ok(Settings {
        id: default_id.to_string(),
        name: default_name.to_string(),
        runner: default_runner,
        provider,
        model: args.model.as_deref().and_then(non_empty),
        orchestrator_loop: args.orchestrator,
    })
}

/// Render `agent.yaml` text. Built as text (the config tree is deserialize-only)
/// in the style of `example-agent.yaml`.
fn render_agent_yaml(s: &Settings) -> String {
    let mut out = String::new();
    out.push_str(&format!("id: {}\n", yaml_scalar(&s.id)));
    out.push_str(&format!("name: {}\n", yaml_scalar(&s.name)));
    out.push_str("\nrunner:\n");
    out.push_str(&format!("  use: {}\n", yaml_scalar(&s.runner)));
    if s.runner != "codex" {
        if let Some(provider) = &s.provider {
            out.push_str(&format!("  provider: {}\n", yaml_scalar(provider)));
        }
    }
    if let Some(model) = &s.model {
        out.push_str(&format!("  model: {}\n", yaml_scalar(model)));
    }
    // Loop config (states/polling/workspace) lives in WORKFLOW.md frontmatter,
    // but the composer still needs a `tracker.use` here to link the tracker
    // extension crate into a per-agent binary (build-time selection only). Emit
    // it to match the tracker `init-workflow` scaffolds by default.
    if s.orchestrator_loop {
        out.push_str("\ntracker:\n");
        out.push_str(&format!("  use: {}\n", yaml_scalar(DEFAULT_TRACKER_USE)));
    }
    out
}

/// Tracker crate the composer links when `--orchestrator` scaffolds a loop.
/// Matches the default (bare) `init-workflow` scaffold, which is Linear.
const DEFAULT_TRACKER_USE: &str = "linear";

/// Render a string as a single-line YAML scalar, quoting/escaping via `serde_yaml`
/// so free-text (interactive) values can't produce a broken or misparsed file.
fn yaml_scalar(value: &str) -> String {
    serde_yaml::to_string(value)
        .expect("serializing a string scalar cannot fail")
        .trim_end()
        .to_string()
}

/// Lowercase; collapse each run of non-alphanumeric chars to a single `-`; trim
/// leading/trailing `-`.
fn slugify(input: &str) -> String {
    let mut slug = String::new();
    let mut pending_dash = false;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            pending_dash = false;
            slug.extend(ch.to_lowercase());
        } else {
            pending_dash = true;
        }
    }
    slug
}

/// Split a slug on `-`, capitalize each word, join with spaces.
fn titleize(slug: &str) -> String {
    slug.split('-')
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Flush the prompt written to stdout and read one line of input from stdin.
fn read_reply() -> Result<String> {
    std::io::stdout().flush().context("flushing prompt")?;
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("reading input")?;
    Ok(line)
}

/// Prompt for a value, returning `default` on empty input.
fn prompt(label: &str, default: &str) -> Result<String> {
    if default.is_empty() {
        print!("{label}: ");
    } else {
        print!("{label} [{default}]: ");
    }
    let line = read_reply()?;
    let trimmed = line.trim();
    Ok(if trimmed.is_empty() {
        default.to_string()
    } else {
        trimmed.to_string()
    })
}

/// Prompt for a yes/no value, returning `default` on empty input.
fn prompt_bool(label: &str, default: bool) -> Result<bool> {
    let hint = if default { "Y/n" } else { "y/N" };
    print!("{label} [{hint}]: ");
    let line = read_reply()?;
    Ok(match line.trim().to_ascii_lowercase().as_str() {
        "" => default,
        "y" | "yes" => true,
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_collapses_and_trims() {
        assert_eq!(slugify("My Agent"), "my-agent");
        assert_eq!(slugify("  weird__Name!! "), "weird-name");
        assert_eq!(slugify("already-slug"), "already-slug");
        assert_eq!(slugify("UPPER"), "upper");
        assert_eq!(slugify("!!!"), "");
        assert_eq!(slugify("a1b2"), "a1b2");
    }

    #[test]
    fn titleize_capitalizes_words() {
        assert_eq!(titleize("my-agent"), "My Agent");
        assert_eq!(titleize("upper"), "Upper");
        assert_eq!(titleize(""), "");
    }

    #[test]
    fn derive_identity_falls_back_when_no_slug() {
        assert_eq!(
            derive_identity(Path::new("/tmp/!!!")),
            ("agent".to_string(), "Agent".to_string())
        );
        assert_eq!(
            derive_identity(Path::new("/tmp/My Cool Agent")),
            ("my-cool-agent".to_string(), "My Cool Agent".to_string())
        );
    }

    fn settings(
        runner: &str,
        provider: Option<&str>,
        model: Option<&str>,
        loop_: bool,
    ) -> Settings {
        Settings {
            id: "my-agent".to_string(),
            name: "My Agent".to_string(),
            runner: runner.to_string(),
            provider: provider.map(str::to_string),
            model: model.map(str::to_string),
            orchestrator_loop: loop_,
        }
    }

    #[test]
    fn render_passive_agent_omits_trio_and_optional_lines() {
        let yaml = render_agent_yaml(&settings("pi", None, None, false));
        assert_eq!(yaml, "id: my-agent\nname: My Agent\n\nrunner:\n  use: pi\n");
        assert!(!yaml.contains("tracker:"));
        assert!(!yaml.contains("provider:"));
        assert!(!yaml.contains("model:"));
    }

    #[test]
    fn render_includes_provider_and_model_when_present() {
        let yaml = render_agent_yaml(&settings("pi", Some("anthropic"), Some("sonnet"), false));
        assert!(yaml.contains("  use: pi\n"));
        assert!(yaml.contains("  provider: anthropic\n"));
        assert!(yaml.contains("  model: sonnet\n"));
    }

    #[test]
    fn render_drops_provider_for_codex() {
        let yaml = render_agent_yaml(&settings("codex", Some("openai"), None, false));
        assert!(yaml.contains("  use: codex\n"));
        assert!(!yaml.contains("provider:"));
    }

    #[test]
    fn render_writes_only_tracker_use_selector_when_loop_enabled() {
        // Loop config (states/polling/workspace) lives in WORKFLOW.md
        // frontmatter, so it never appears in agent.yaml. The only tracker
        // key here is the build-time `use` selector the composer needs to
        // link the tracker crate.
        let with_loop = render_agent_yaml(&settings("pi", None, None, true));
        let without_loop = render_agent_yaml(&settings("pi", None, None, false));
        assert_ne!(with_loop, without_loop);
        assert!(with_loop.contains("tracker:\n  use: linear\n"));
        assert!(!with_loop.contains("active_states"));
        assert!(!with_loop.contains("orchestrator:"));
        assert!(!with_loop.contains("workspace:"));
        assert!(!without_loop.contains("tracker:"));
    }

    #[test]
    fn render_quotes_and_escapes_special_characters() {
        let mut s = settings("pi", None, None, false);
        s.id = "foo #x".to_string();
        s.name = "My \"Cool\" Agent".to_string();
        let yaml = render_agent_yaml(&s);
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("agent.yaml"), &yaml).unwrap();
        let cfg = orchestrator::config::load(temp.path()).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.id, "foo #x");
        assert_eq!(cfg.name, "My \"Cool\" Agent");
    }

    #[test]
    fn generated_yaml_loads_and_validates_passive() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::write(
            root.join("agent.yaml"),
            render_agent_yaml(&settings("pi", None, None, false)),
        )
        .unwrap();
        let cfg = orchestrator::config::load(root).unwrap();
        cfg.validate().unwrap();
    }

    #[test]
    fn generated_yaml_loads_and_validates_with_loop() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::write(
            root.join("agent.yaml"),
            render_agent_yaml(&settings("pi", None, None, true)),
        )
        .unwrap();
        let cfg = orchestrator::config::load(root).unwrap();
        cfg.validate().unwrap();
    }

    #[test]
    fn run_writes_agent_yaml_and_gitignore_non_interactive() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("my-agent");
        let args = CreateArgs {
            path: Some(root.clone()),
            runner: None,
            provider: None,
            model: None,
            orchestrator: false,
        };

        let outcome = run(&root, &args).unwrap();

        assert!(!outcome.loop_enabled);
        let yaml = std::fs::read_to_string(root.join("agent.yaml")).unwrap();
        assert!(yaml.contains("id: my-agent"));
        assert!(yaml.contains("  use: pi\n"));
        assert!(root.join(".gitignore").exists());
        let cfg = orchestrator::config::load(&root).unwrap();
        cfg.validate().unwrap();
    }

    #[test]
    fn run_marks_loop_enabled_without_writing_trio() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("my-agent");
        let args = CreateArgs {
            path: Some(root.clone()),
            runner: None,
            provider: None,
            model: None,
            orchestrator: true,
        };

        let outcome = run(&root, &args).unwrap();

        assert!(outcome.loop_enabled);
        let yaml = std::fs::read_to_string(root.join("agent.yaml")).unwrap();
        // Build-time tracker selector only; no loop config in agent.yaml.
        assert!(yaml.contains("tracker:\n  use: linear\n"));
        assert!(!yaml.contains("active_states"));
        assert!(!yaml.contains("orchestrator:"));
        assert!(!yaml.contains("workspace:"));
        let cfg = orchestrator::config::load(&root).unwrap();
        cfg.validate().unwrap();
    }

    #[test]
    fn run_refuses_to_overwrite_existing_agent_yaml() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::write(root.join("agent.yaml"), "id: x\n").unwrap();
        let args = CreateArgs {
            path: Some(root.to_path_buf()),
            runner: None,
            provider: None,
            model: None,
            orchestrator: false,
        };

        let err = run(root, &args).unwrap_err().to_string();
        assert!(err.contains("already exists"), "unexpected error: {err}");
    }
}
