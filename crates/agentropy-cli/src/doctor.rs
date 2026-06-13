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
}
