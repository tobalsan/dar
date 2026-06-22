//! Scheduler extension (ALG-219 walking skeleton).
//!
//! On boot it loads the agent's `cron/jobs.json`, computes each enabled job's
//! next fire from cron + IANA timezone + optional `startAt`, arms a single timer
//! for the earliest, and when due spawns the agent's configured default runner
//! (`runner.use`) with the job's `payload.message` as prompt. The captured
//! response is written to `cron/output/<job_id>/<timestamp>.md` with aihub-shape
//! frontmatter. All due jobs at a tick run concurrently; the timer re-arms after
//! each tick. A malformed jobs file logs one warning and is treated as empty.
//!
//! Reference: aihub `packages/extensions/scheduler`. Parity gaps in this slice
//! (tracked separately): no per-job model override, no `sessionId` continuity,
//! no HTTP/CLI, no hot reload, no overlap/timeout guards.

mod output;
mod runner;
mod schedule;
mod service;
mod store;

use anyhow::Result;
use host_api::{Extension, StartCtx};
use serde::Deserialize;

use crate::service::SchedulerConfig;

/// Default runner kind when `runner.use` is empty, matching the orchestrator's
/// `runner_service_id` fallback.
const DEFAULT_RUNNER_KIND: &str = "pi";
const DEFAULT_MAX_RUN_TIMEOUT_MS: u64 = 3_600_000;

pub struct SchedulerExtension;

pub fn extension() -> Box<dyn Extension> {
    Box::new(SchedulerExtension)
}

/// `extensions.scheduler` config. `enabled: false` is a runtime kill switch:
/// the extension still loads, but no timer is armed and no jobs fire.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
struct SchedulerSettings {
    enabled: bool,
}

impl Default for SchedulerSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Subset of `agent.yaml` the scheduler needs to resolve and fire the default
/// runner. Read directly from the agent root since the per-extension
/// `ConfigStore` only exposes the `extensions.scheduler` section.
#[derive(Debug, Deserialize)]
struct AgentRunnerConfig {
    #[serde(default)]
    runner: RunnerSection,
}

#[derive(Debug, Default, Deserialize)]
struct RunnerSection {
    #[serde(rename = "use", default)]
    use_: String,
    #[serde(default)]
    command: String,
    #[serde(default)]
    max_run_timeout_ms: u64,
}

impl Extension for SchedulerExtension {
    fn id(&self) -> &'static str {
        "scheduler"
    }

    fn start<'a>(&'a self, ctx: StartCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            // Presence of the `extensions.scheduler` section is the opt-in
            // gate. The FSC composer only links this crate when the section
            // exists, so a composed binary without it never reaches here. The
            // shipped dist binary always links the crate, so we also gate at
            // runtime: an absent section means "behave exactly as today" (do
            // not arm a timer, do not read cron/jobs.json).
            let Some(value) = ctx.config.get(self.id()) else {
                return Ok(());
            };
            let settings =
                serde_json::from_value::<SchedulerSettings>(value.clone()).unwrap_or_default();
            if !settings.enabled {
                tracing::info!("[scheduler] Disabled via extensions.scheduler.enabled=false");
                return Ok(());
            }

            let root = ctx.paths.root().to_path_buf();
            let runner = read_runner_config(&root);
            let config = SchedulerConfig {
                root,
                runner_kind: runner.0,
                runner_command: runner.1,
                max_run_timeout_ms: runner.2,
            };

            let services = ctx.host.services.clone();
            let shutdown = ctx.shutdown.clone();
            tokio::spawn(async move {
                service::run(config, services, shutdown).await;
            });
            Ok(())
        })
    }
}

/// Read `runner.use` / `runner.command` / `runner.max_run_timeout_ms` from
/// `agent.yaml`, applying the same defaults the orchestrator uses.
fn read_runner_config(root: &std::path::Path) -> (String, String, u64) {
    let path = root.join("agent.yaml");
    let parsed = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_yaml::from_str::<AgentRunnerConfig>(&raw).ok());
    match parsed {
        Some(cfg) => {
            let kind = if cfg.runner.use_.trim().is_empty() {
                DEFAULT_RUNNER_KIND.to_string()
            } else {
                cfg.runner.use_
            };
            let timeout = if cfg.runner.max_run_timeout_ms == 0 {
                DEFAULT_MAX_RUN_TIMEOUT_MS
            } else {
                cfg.runner.max_run_timeout_ms
            };
            (kind, cfg.runner.command, timeout)
        }
        None => (
            DEFAULT_RUNNER_KIND.to_string(),
            String::new(),
            DEFAULT_MAX_RUN_TIMEOUT_MS,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_runner_kind_and_timeout_from_agent_yaml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("agent.yaml"),
            "id: a\nname: A\nrunner:\n  use: fake\n  max_run_timeout_ms: 1234\n",
        )
        .unwrap();
        let (kind, command, timeout) = read_runner_config(dir.path());
        assert_eq!(kind, "fake");
        assert_eq!(command, "");
        assert_eq!(timeout, 1234);
    }

    #[test]
    fn defaults_runner_kind_to_pi_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("agent.yaml"), "id: a\nname: A\n").unwrap();
        let (kind, _command, timeout) = read_runner_config(dir.path());
        assert_eq!(kind, "pi");
        assert_eq!(timeout, DEFAULT_MAX_RUN_TIMEOUT_MS);
    }

    #[test]
    fn settings_default_enabled_true() {
        assert!(SchedulerSettings::default().enabled);
    }

    #[test]
    fn settings_parse_kill_switch() {
        let s: SchedulerSettings =
            serde_json::from_value(serde_json::json!({ "enabled": false })).unwrap();
        assert!(!s.enabled);
    }
}
