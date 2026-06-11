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

use crate::config;
use crate::dotenv::LoadReport;
use crate::paths::AgentPaths;
use crate::prompt::PromptRenderer;
use crate::tracker;

/// Run all preflight checks against `root`. Returns the process exit code.
pub fn run(root: &Path, dotenv: &LoadReport) -> anyhow::Result<i32> {
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
    match PromptRenderer::load(&paths.workflow_md()) {
        Ok(_) => pass("WORKFLOW.md loads"),
        Err(e) => {
            fail(&format!("WORKFLOW.md: {e:#}"));
            ok = false;
        }
    }

    // 3. Tracker (only if config parsed, since build needs it).
    if let Some(cfg) = cfg {
        match tracker::build(&cfg.tracker, &paths) {
            Ok(t) => match t.poll_candidates() {
                Ok(issues) => pass(&format!(
                    "tracker '{}' reachable ({} active issue(s))",
                    cfg.tracker.use_,
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
    }

    if ok {
        eprintln!("doctor: all checks passed");
        Ok(0)
    } else {
        eprintln!("doctor: one or more checks failed");
        Ok(1)
    }
}

fn pass(msg: &str) {
    eprintln!("  ok   {msg}");
}

fn fail(msg: &str) {
    eprintln!("  FAIL {msg}");
}
