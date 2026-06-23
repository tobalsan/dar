//! `agentropy doctor` preflight.
//!
//! Validates the three things the run loop relies on, printing findings to
//! stderr and returning a process exit code (0 = all good, 1 = at least one
//! check failed). Sets up no file logging; this is a one-shot check.
//!
//! Checks:
//!   1. `agent.yaml` parses and passes `AgentConfig::validate`.
//!   2. `WORKFLOW.md` exists and loads as a prompt template.
//!   3. The configured tracker builds and its issues dir can be polled.

use std::path::Path;
use std::process::Command;

use anyhow::Context;
use host_api::ServiceRegistry;
use orchestrator::config;
use orchestrator::dotenv::LoadReport;
use orchestrator::paths::AgentPaths;
use orchestrator::prompt::PromptRenderer;
use orchestrator::tracker;
use orchestrator::workflow_config::EffectiveLoopConfig;

/// Run all preflight checks against `root`. Returns the process exit code.
pub fn run(root: &Path, dotenv: &LoadReport, services: ServiceRegistry) -> anyhow::Result<i32> {
    let paths = AgentPaths::new(root.to_path_buf());
    let mut ok = true;

    if dotenv.found {
        pass(&format!(
            ".env loaded from {} ({} loaded, {} already set)",
            dotenv.path.display(),
            dotenv.loaded.len(),
            dotenv.skipped_existing.len()
        ));
    } else {
        pass(&format!(".env not found at {}", dotenv.path.display()));
    }

    match check_toolchain(&paths.root) {
        Ok(msg) => pass(&msg),
        Err(e) => {
            fail(&format!("Rust toolchain unavailable: {e:#}"));
            ok = false;
        }
    }

    // 1. Config.
    let cfg = match config::load(&paths.root) {
        Ok(cfg) => match cfg.validate() {
            Ok(()) => {
                pass(&format!("agent.yaml valid (id={})", cfg.id));
                Some(cfg)
            }
            Err(e) => {
                fail(&format!("agent.yaml invalid: {e:#}"));
                ok = false;
                None
            }
        },
        Err(e) => {
            fail(&format!("agent.yaml: {e:#}"));
            ok = false;
            None
        }
    };

    // 1b. System files: required entries present + containment.
    if let Some(cfg) = cfg.as_ref() {
        match orchestrator::system_context::resolve(&paths.root, cfg.system_files.as_deref()) {
            Ok(ctx) => pass(&format!(
                "system files resolve ({} file(s))",
                ctx.files.len()
            )),
            Err(e) => {
                fail(&format!("system files: {e}"));
                ok = false;
            }
        }
    }

    // 2. WORKFLOW.md prompt template.
    let prompt = match PromptRenderer::load(&paths.workflow_md()) {
        Ok(prompt) => {
            pass("WORKFLOW.md loads");
            Some(prompt)
        }
        Err(e) => {
            fail(&format!("WORKFLOW.md: {e:#}"));
            ok = false;
            None
        }
    };

    // 3. Tracker (only if config parsed, since build needs it).
    if let (Some(cfg), Some(prompt)) = (cfg, prompt) {
        let effective_cfg = EffectiveLoopConfig::merge(&cfg, &prompt.snapshot().frontmatter);
        let mut tracker_cfg = cfg.tracker.clone();
        tracker_cfg.use_ = effective_cfg.tracker_kind.clone();
        tracker_cfg.active_states = effective_cfg.active_states.clone();
        tracker_cfg.terminal_states = effective_cfg.terminal_states.clone();
        tracker_cfg.project_slug = effective_cfg.tracker_project_slug.clone();
        tracker_cfg.endpoint = Some(effective_cfg.tracker_endpoint.clone());
        tracker_cfg.needs_human = effective_cfg.needs_human.clone();
        tracker_cfg.team = effective_cfg.tracker_team.clone();
        tracker_cfg.assignee = effective_cfg.tracker_assignee.clone();
        tracker_cfg.label = (!effective_cfg.tracker_labels.is_empty())
            .then(|| orchestrator::config::StringOrVec::List(effective_cfg.tracker_labels.clone()));

        match tracker::build_configured(&services, &tracker_cfg, paths.root.clone()) {
            Ok(t) => match t.poll_candidates() {
                Ok(issues) => pass(&format!(
                    "tracker '{}' reachable ({} active issue(s))",
                    tracker_cfg.use_,
                    issues.len()
                )),
                Err(e) => {
                    fail(&format!("tracker poll failed: {e:#}"));
                    ok = false;
                }
            },
            Err(e) => {
                fail(&format!("tracker build failed: {e:#}"));
                ok = false;
            }
        }

        let runner_id = if effective_cfg.runner_kind.trim().is_empty() {
            "pi"
        } else {
            &effective_cfg.runner_kind
        };
        match services.get_named::<dyn cap_runner::Runner>(runner_id) {
            Ok(_) => pass(&format!("runner '{runner_id}' registered")),
            Err(e) => {
                fail(&format!("runner resolution failed: {e:#}"));
                ok = false;
            }
        }

        match orchestrator::thinking::validate_thinking_for_runner(
            runner_id,
            effective_cfg.thinking.as_deref(),
        ) {
            Ok(()) => match effective_cfg.thinking.as_deref() {
                Some(level) if !level.trim().is_empty() => {
                    if matches!(runner_id, "cli" | "fake") {
                        pass(&format!(
                            "thinking level '{level}' ignored by runner '{runner_id}'"
                        ))
                    } else {
                        pass(&format!(
                            "thinking level '{level}' valid for runner '{runner_id}'"
                        ))
                    }
                }
                _ => pass("thinking level not set (runner default applies)"),
            },
            Err(e) => {
                fail(&format!("{e:#}"));
                ok = false;
            }
        }

        if runner_id == "opencode" {
            match check_opencode(&cfg.runner.command) {
                Ok(msg) => pass(&msg),
                Err(e) => {
                    fail(&format!("opencode setup unusable: {e:#}"));
                    ok = false;
                }
            }
        }
    }

    if ok {
        eprintln!("doctor: all checks passed");
        Ok(0)
    } else {
        eprintln!("doctor: one or more checks failed");
        Ok(1)
    }
}

fn check_opencode(command: &str) -> anyhow::Result<String> {
    let command = if command.trim().is_empty() {
        "opencode"
    } else {
        command
    };
    let output = Command::new(command)
        .arg("--version")
        .output()
        .map_err(|e| anyhow::anyhow!("cannot run `{command} --version`: {e}"))?;
    if !output.status.success() {
        anyhow::bail!("`{command} --version` exited with {}", output.status);
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(if version.is_empty() {
        format!("opencode CLI available ({command})")
    } else {
        format!("opencode CLI available ({command} {version})")
    })
}

fn check_toolchain(root: &Path) -> anyhow::Result<String> {
    let pinned = pinned_toolchain(root)?;
    let cargo_version = command_version("cargo")?;
    let rustc_version = command_version("rustc")?;
    if !tool_version_satisfies_channel(&cargo_version, &pinned) {
        anyhow::bail!(
            "cargo is `{cargo_version}`, but `.agentropy/rust-toolchain.toml` requires Rust `{pinned}` or newer; install/switch Rust via rustup"
        );
    }
    if !tool_version_satisfies_channel(&rustc_version, &pinned) {
        anyhow::bail!(
            "rustc is `{rustc_version}`, but `.agentropy/rust-toolchain.toml` requires Rust `{pinned}` or newer; install/switch Rust via rustup"
        );
    }
    Ok(format!(
        "Rust toolchain available (cargo {cargo_version}, rustc {rustc_version})"
    ))
}

pub fn check_static_build_prereqs(target: &str) -> anyhow::Result<String> {
    let targets = installed_rust_targets()?;
    check_static_build_prereqs_with(target, Some(&targets), &present_musl_linkers())
}

fn check_static_build_prereqs_with(
    target: &str,
    installed_targets: Option<&str>,
    present_linkers: &[&str],
) -> anyhow::Result<String> {
    if !target.ends_with("-unknown-linux-musl") {
        anyhow::bail!("static builds require a Linux musl target, got `{target}`");
    }
    let installed_targets = installed_targets.context("checking installed Rust targets")?;
    if !installed_targets.lines().any(|line| line.trim() == target) {
        anyhow::bail!("rust target `{target}` is not installed; run `rustup target add {target}`");
    }
    if !target_musl_linker_present(target, present_linkers) {
        anyhow::bail!(
            "musl linker for `{target}` not found; install musl-tools/musl-gcc or build with cross/container"
        );
    }
    Ok(format!("static build prerequisites available for {target}"))
}

fn installed_rust_targets() -> anyhow::Result<String> {
    let output = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .map_err(|e| anyhow::anyhow!("rustup not found - install Rust via rustup: {e}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "`rustup target list --installed` exited with {}",
            output.status
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn present_musl_linkers() -> Vec<&'static str> {
    [
        "musl-gcc",
        "x86_64-linux-musl-gcc",
        "aarch64-linux-musl-gcc",
    ]
    .into_iter()
    .filter(|command| command_exists(command))
    .collect()
}

fn target_musl_linker_present(target: &str, present_linkers: &[&str]) -> bool {
    let target_linker = match target {
        "x86_64-unknown-linux-musl" => "x86_64-linux-musl-gcc",
        "aarch64-unknown-linux-musl" => "aarch64-linux-musl-gcc",
        _ => return false,
    };
    present_linkers.contains(&target_linker)
        || (target_arch_matches(target) && present_linkers.contains(&"musl-gcc"))
}

fn target_arch_matches(target: &str) -> bool {
    target
        .strip_suffix("-unknown-linux-musl")
        .is_some_and(|arch| arch == std::env::consts::ARCH)
}

fn command_exists(command: &str) -> bool {
    Command::new(command)
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn pinned_toolchain(root: &Path) -> anyhow::Result<String> {
    let path = root.join(".agentropy/rust-toolchain.toml");
    let content =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let value = content
        .parse::<toml::Value>()
        .with_context(|| format!("parsing {}", path.display()))?;
    value
        .get("toolchain")
        .and_then(|t| t.get("channel"))
        .and_then(toml::Value::as_str)
        .map(str::to_string)
        .context("missing [toolchain].channel")
}

fn command_version(command: &str) -> anyhow::Result<String> {
    let output = Command::new(command)
        .arg("--version")
        .output()
        .map_err(|e| anyhow::anyhow!("{command} not found - install Rust via rustup: {e}"))?;
    if !output.status.success() {
        anyhow::bail!("`{command} --version` exited with {}", output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn tool_version_satisfies_channel(version_output: &str, channel: &str) -> bool {
    match (
        first_version_tuple(version_output),
        first_version_tuple(channel),
    ) {
        (Some(actual), Some(required)) => actual >= required,
        _ => version_output.contains(channel),
    }
}

fn first_version_tuple(text: &str) -> Option<(u64, u64, u64)> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-'))
        .find_map(parse_version_tuple)
}

fn parse_version_tuple(token: &str) -> Option<(u64, u64, u64)> {
    let mut parts = token.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts
        .next()
        .and_then(|part| part.split('-').next())
        .filter(|part| !part.is_empty())
        .map(str::parse)
        .transpose()
        .ok()?
        .unwrap_or(0);
    Some((major, minor, patch))
}

fn pass(msg: &str) {
    eprintln!("  ok   {msg}");
}

fn fail(msg: &str) {
    eprintln!("  FAIL {msg}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn opencode_check_reports_missing_command() {
        let err = check_opencode("__missing_opencode_for_agentropy_test__").unwrap_err();
        assert!(err.to_string().contains("cannot run"));
    }

    #[test]
    fn opencode_check_reports_version_from_configured_command() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("opencode");
        std::fs::write(&script, "#!/bin/sh\necho 9.9.9\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let msg = check_opencode(script.to_str().unwrap()).unwrap();
        assert!(msg.contains("9.9.9"), "{msg}");
    }

    #[test]
    fn toolchain_version_accepts_equal_or_newer_numeric_channel() {
        assert!(tool_version_satisfies_channel("rustc 1.83.0", "1.83"));
        assert!(tool_version_satisfies_channel("cargo 1.96.0", "1.83"));
        assert!(tool_version_satisfies_channel("rustc 1.83.1", "1.83.0"));
    }

    #[test]
    fn toolchain_version_rejects_older_numeric_channel() {
        assert!(!tool_version_satisfies_channel("rustc 1.82.9", "1.83"));
        assert!(!tool_version_satisfies_channel("cargo 1.83.0", "1.83.1"));
    }

    #[test]
    fn toolchain_version_falls_back_to_exact_channel_match() {
        assert!(tool_version_satisfies_channel("rustc stable", "stable"));
        assert!(!tool_version_satisfies_channel("rustc beta", "stable"));
    }

    #[test]
    fn toolchain_check_reports_too_old_version() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".agentropy")).unwrap();
        std::fs::write(
            dir.path().join(".agentropy/rust-toolchain.toml"),
            "[toolchain]\nchannel = \"999.0\"\n",
        )
        .unwrap();
        let err = check_toolchain(dir.path()).unwrap_err();
        assert!(err.to_string().contains("requires Rust `999.0` or newer"));
    }

    #[test]
    fn static_preflight_requires_installed_rust_target() {
        let err = check_static_build_prereqs_with(
            "x86_64-unknown-linux-musl",
            Some("x86_64-apple-darwin\n"),
            &["x86_64-linux-musl-gcc"],
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("rust target `x86_64-unknown-linux-musl` is not installed"),
            "{err:#}"
        );
    }

    #[test]
    fn static_preflight_requires_musl_linker() {
        let err = check_static_build_prereqs_with(
            "x86_64-unknown-linux-musl",
            Some("x86_64-unknown-linux-musl\n"),
            &[],
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("musl linker for `x86_64-unknown-linux-musl` not found"),
            "{err:#}"
        );
    }

    #[test]
    fn static_preflight_requires_linker_for_requested_target() {
        let err = check_static_build_prereqs_with(
            "aarch64-unknown-linux-musl",
            Some("aarch64-unknown-linux-musl\n"),
            &["x86_64-linux-musl-gcc"],
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("musl linker for `aarch64-unknown-linux-musl` not found"),
            "{err:#}"
        );
    }
}
