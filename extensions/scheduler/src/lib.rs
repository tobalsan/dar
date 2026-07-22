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
//! Hot reload (ALG-223): the runtime polls `cron/jobs.json` on a short interval
//! and refreshes its in-memory job set when the file changes, so an agent or a
//! human editing the file gets schedule changes applied within seconds without
//! restarting the host. Per-job `enabled: false` inside `cron/jobs.json` is
//! live-reloaded; the boot-time `extensions.scheduler.enabled` kill switch is
//! not (it is read once at start, immutable at runtime).
//!
//! Jobs can also be managed remotely over the host HTTP server under the
//! `/scheduler` namespace (list/create/update/delete; see [`http`]). Mutations
//! persist atomically to `cron/jobs.json` and re-arm the timer in-process.
//!
//! Reference: aihub `packages/extensions/scheduler`. Parity gaps in this slice
//! (tracked separately): no per-job model override, no `sessionId` continuity,
//! no CLI.

mod http;
mod output;
mod runner;
mod schedule;
mod service;
mod state;
mod store;
mod tab;
mod tools;

use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{bail, Result};
use cap_dashboard_tab::DashboardTabs;
use host_api::{Extension, RegisterCtx, StartCtx};
use serde::Deserialize;

use cap_runner::{DEFAULT_MAX_RUN_TIMEOUT_MS, DEFAULT_RUNNER_KIND};
use tool_registry::{ToolRegistryHandle, TOOL_REGISTRY_SERVICE};

use crate::service::SchedulerConfig;
use crate::state::SchedulerState;
use crate::tab::CronTab;

const DEFAULT_POLL_INTERVAL_MS: u64 = 2_000;
/// Default per-job execution timeout (10 minutes), matching aihub's scheduler
/// default. Overridable by `extensions.scheduler.jobTimeoutMs` and then by a
/// per-job `timeoutMs`.
pub(crate) const DEFAULT_JOB_TIMEOUT_MS: u64 = 600_000;

#[derive(Default)]
pub struct SchedulerExtension {
    /// Shared between the HTTP CRUD router (mounted in `register`) and the timer
    /// loop (spawned in `start`). Set in `register` only when the extension is
    /// enabled, so a disabled/absent extension mounts no routes and spawns no
    /// loop.
    state: OnceLock<Arc<SchedulerState>>,
    host_http_addr: Arc<Mutex<Option<std::net::SocketAddr>>>,
}

pub fn extension() -> Box<dyn Extension> {
    Box::new(SchedulerExtension::default())
}

/// `extensions.scheduler` config, read once at boot (`extensions.*` is frozen
/// after boot, so this is a boot-time switch — changing it requires a restart).
///
/// `enabled: false` is the kill switch: the extension still loads and the jobs
/// file stays readable/writable, but no timer is armed and no job fires. It is
/// not live-reloaded (per-job `enabled` inside `cron/jobs.json` is the live
/// toggle). `jobTimeoutMs` sets the per-run timeout default (overridable per job
/// via `timeoutMs`). `pollIntervalMs` tunes how quickly edits to
/// `cron/jobs.json` are picked up by the hot-reload poll.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SchedulerSettings {
    enabled: bool,
    #[serde(rename = "pollIntervalMs")]
    poll_interval_ms: u64,
    #[serde(rename = "jobTimeoutMs")]
    job_timeout_ms: Option<u64>,
}

impl Default for SchedulerSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_interval_ms: DEFAULT_POLL_INTERVAL_MS,
            job_timeout_ms: None,
        }
    }
}

impl SchedulerSettings {
    /// Parse the `extensions.scheduler` section, naming the offending field on
    /// failure so a boot error points at the problem. Rejects a zero
    /// `jobTimeoutMs` (a zero timeout would kill every run instantly).
    fn parse(value: &serde_json::Value) -> Result<Self> {
        let settings: SchedulerSettings = serde_json::from_value(value.clone())
            .map_err(|e| anyhow::anyhow!("invalid extensions.scheduler config: {e}"))?;
        if settings.job_timeout_ms == Some(0) {
            bail!("invalid extensions.scheduler config: jobTimeoutMs must be greater than 0");
        }
        Ok(settings)
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
    model: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    max_run_timeout_ms: u64,
}

impl Extension for SchedulerExtension {
    fn id(&self) -> &'static str {
        "scheduler"
    }

    fn agent_singleton(&self) -> bool {
        true
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            // Parse `extensions.scheduler` once. An absent section is the
            // opt-out (no HTTP routes, no state); a present-but-invalid section
            // is a boot error (same gate as `start`).
            let settings = match ctx.config.get(self.id()) {
                None => return Ok(()),
                Some(value) => SchedulerSettings::parse(value)?,
            };
            if !settings.enabled {
                return Ok(());
            }
            let root = ctx.paths.root().to_path_buf();
            let jobs = crate::store::load_jobs_checked(&root)
                .map_err(|err| anyhow::anyhow!("[scheduler] {err}"))?;
            let state = Arc::new(SchedulerState::new(jobs));
            let _ = self.state.set(Arc::clone(&state));

            // Contribute the read-only "Cron" dashboard tab via the
            // cap-dashboard-tab contract. Registered only on the enabled path, so
            // the tab is absent whenever the scheduler is not linked/enabled
            // (dist: no `extensions.scheduler` section; FSC: crate not composed).
            // Shares the same `SchedulerState` so the view reflects live runtime.
            DashboardTabs::shared(&mut ctx.services)?
                .add(Arc::new(CronTab::new(Arc::clone(&state), root.clone())))?;

            // run-now fires a job through the same path as a scheduled fire, so
            // the HTTP handlers need the static runner config + typed services.
            let config = build_scheduler_config(
                &root,
                ctx.paths.workflow_root(),
                &settings,
                Arc::clone(&self.host_http_addr),
            );
            let services = ctx.services.clone();

            // Register the model-facing scheduler management tools against the
            // shared host tool registry, so every tool-capable runner and
            // cap-chat backend can discover and call them. Resolved leniently:
            // a stripped composition without the registry still boots the
            // scheduler (HTTP + dashboard tab) without the tools. Registration
            // happens only here, on the enabled path, so the tools are
            // discoverable only when the scheduler extension is enabled.
            if let Ok(registry) = ctx
                .services
                .get_named::<dyn ToolRegistryHandle>(TOOL_REGISTRY_SERVICE)
            {
                tools::register_into(
                    registry.as_ref(),
                    tools::ToolDeps {
                        state: Arc::clone(&state),
                        root: root.clone(),
                        config: config.clone(),
                        services: services.clone(),
                    },
                )?;
            }

            let api_state = http::ApiState {
                state,
                root,
                config,
                services,
            };
            ctx.http.mount(host_api::HttpMount {
                namespace: "/scheduler".to_string(),
                router: http::router(api_state),
                routes: http::routes(),
                claim_root: false,
            })?;
            Ok(())
        })
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
            // Already validated in `register` (boot fails on bad config), so
            // this parse cannot fail; propagate rather than silently default,
            // which would risk re-enabling a kill-switched scheduler.
            let settings = SchedulerSettings::parse(value)?;
            if !settings.enabled {
                tracing::info!("[scheduler] Disabled via extensions.scheduler.enabled=false");
                return Ok(());
            }

            // `register` built and stored the shared state alongside the HTTP
            // router. Reuse it so the loop and the API observe the same jobs.
            let state = Arc::clone(
                self.state
                    .get()
                    .expect("scheduler state must be initialized during register when enabled"),
            );

            let root = ctx.paths.root().to_path_buf();
            *self
                .host_http_addr
                .lock()
                .expect("scheduler host address mutex poisoned") = ctx.host.http_addr();
            let config = build_scheduler_config(
                &root,
                ctx.paths.workflow_root(),
                &settings,
                Arc::clone(&self.host_http_addr),
            );

            let services = ctx.host.services.clone();
            let shutdown = ctx.shutdown.clone();
            tokio::spawn(async move {
                service::run(config, services, state, shutdown).await;
            });
            Ok(())
        })
    }
}

/// Build the static [`SchedulerConfig`] shared by the timer loop and the
/// run-now HTTP handler from the agent root + validated scheduler settings.
fn build_scheduler_config(
    root: &std::path::Path,
    workflow_root: &std::path::Path,
    settings: &SchedulerSettings,
    host_http_addr: Arc<Mutex<Option<std::net::SocketAddr>>>,
) -> SchedulerConfig {
    let (
        runner_kind,
        runner_command,
        runner_model,
        runner_provider,
        runner_thinking,
        max_run_timeout_ms,
    ) = read_runner_config(root);
    let poll_interval_ms = if settings.poll_interval_ms == 0 {
        DEFAULT_POLL_INTERVAL_MS
    } else {
        settings.poll_interval_ms
    };
    SchedulerConfig {
        root: root.to_path_buf(),
        workflow_root: workflow_root.to_path_buf(),
        host_http_addr,
        runner_kind,
        runner_command,
        runner_model,
        runner_provider,
        runner_thinking,
        system_context: read_system_context(root),
        max_run_timeout_ms,
        poll_interval_ms,
        job_timeout_ms: settings.job_timeout_ms.unwrap_or(DEFAULT_JOB_TIMEOUT_MS),
    }
}

fn read_system_context(root: &std::path::Path) -> Option<String> {
    let ctx = system_context::resolve_for(root);
    (!ctx.is_empty()).then_some(ctx.text)
}

/// Read `runner.use` / `runner.command` / `runner.max_run_timeout_ms` from
/// `agent.yaml`, applying the same defaults the orchestrator uses.
fn read_runner_config(
    root: &std::path::Path,
) -> (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    u64,
) {
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
            (
                kind,
                cfg.runner.command,
                cfg.runner.model,
                cfg.runner.provider,
                cfg.runner.thinking,
                timeout,
            )
        }
        None => (
            DEFAULT_RUNNER_KIND.to_string(),
            String::new(),
            None,
            None,
            None,
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
        let (kind, command, model, provider, thinking, timeout) = read_runner_config(dir.path());
        assert_eq!(kind, "fake");
        assert_eq!(command, "");
        assert_eq!(model, None);
        assert_eq!(provider, None);
        assert_eq!(thinking, None);
        assert_eq!(timeout, 1234);
    }

    #[test]
    fn defaults_runner_kind_to_pi_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("agent.yaml"), "id: a\nname: A\n").unwrap();
        let (kind, _command, model, provider, thinking, timeout) = read_runner_config(dir.path());
        assert_eq!(kind, "pi");
        assert_eq!(model, None);
        assert_eq!(provider, None);
        assert_eq!(thinking, None);
        assert_eq!(timeout, DEFAULT_MAX_RUN_TIMEOUT_MS);
    }

    #[test]
    fn reads_runner_thinking_from_agent_yaml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("agent.yaml"),
            "id: a\nname: A\nrunner:\n  use: pi\n  provider: anthropic\n  model: claude-opus-4-8\n  thinking: high\n",
        )
        .unwrap();
        let (_kind, _command, model, provider, thinking, _timeout) = read_runner_config(dir.path());
        assert_eq!(model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(provider.as_deref(), Some("anthropic"));
        assert_eq!(thinking.as_deref(), Some("high"));
    }

    #[test]
    fn reads_system_context_from_agent_yaml_system_files_and_skills() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("agent.yaml"),
            "id: a\nname: A\nsystem_files:\n  - SOUL.md\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("SOUL.md"), "identity").unwrap();
        let skill_dir = dir.path().join("skills").join("refactor");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: refactor\ndescription: Improve code shape\n---\nBody",
        )
        .unwrap();

        let context = read_system_context(dir.path()).unwrap();

        assert!(context.contains("<system-file path=\"SOUL.md\">"));
        assert!(context.contains("identity"));
        assert!(context.contains("<available_skills>"));
        assert!(context.contains("<name>refactor</name>"));
    }

    #[test]
    fn settings_default_enabled_true() {
        let s = SchedulerSettings::default();
        assert!(s.enabled);
        assert_eq!(s.poll_interval_ms, DEFAULT_POLL_INTERVAL_MS);
        assert_eq!(s.job_timeout_ms, None);
    }

    #[test]
    fn settings_parse_kill_switch() {
        let s = SchedulerSettings::parse(&serde_json::json!({ "enabled": false })).unwrap();
        assert!(!s.enabled);
        // Omitted poll interval falls back to the default.
        assert_eq!(s.poll_interval_ms, DEFAULT_POLL_INTERVAL_MS);
    }

    #[test]
    fn settings_parse_poll_interval() {
        let s: SchedulerSettings =
            serde_json::from_value(serde_json::json!({ "pollIntervalMs": 500 })).unwrap();
        assert!(s.enabled);
        assert_eq!(s.poll_interval_ms, 500);
    }

    #[test]
    fn settings_parse_job_timeout_ms() {
        let s = SchedulerSettings::parse(&serde_json::json!({ "jobTimeoutMs": 120000 })).unwrap();
        assert!(s.enabled);
        assert_eq!(s.job_timeout_ms, Some(120000));
    }

    #[test]
    fn settings_parse_empty_section_is_defaults() {
        let s = SchedulerSettings::parse(&serde_json::json!({})).unwrap();
        assert!(s.enabled);
        assert_eq!(s.job_timeout_ms, None);
    }

    #[test]
    fn settings_parse_rejects_unknown_field() {
        let err = SchedulerSettings::parse(&serde_json::json!({ "jobTimoutMs": 5 }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("extensions.scheduler"), "named error: {err}");
    }

    #[test]
    fn settings_parse_rejects_zero_timeout() {
        let err = SchedulerSettings::parse(&serde_json::json!({ "jobTimeoutMs": 0 }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("jobTimeoutMs"), "named error: {err}");
    }

    #[test]
    fn settings_parse_rejects_wrong_type() {
        let err = SchedulerSettings::parse(&serde_json::json!({ "enabled": "yes" }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("extensions.scheduler"), "named error: {err}");
    }
}
