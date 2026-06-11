mod cli;
mod config;
mod dashboard;
mod doctor;
mod domain;
mod dotenv;
mod export;
mod hitl;
mod logging;
mod orchestrator;
mod paths;
mod prompt;
mod runner;
mod state;
mod store;
mod tracker;
mod workflow_config;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use host_api::{Extension, ShutdownToken, StartCtx, APP_DONE_TOPIC};
use tokio::sync::{mpsc, watch};

pub use cli::{Cli, Command};
use hitl::{BurstHitlNotifier, HitlNotification, HitlNotify};
use paths::AgentPaths;
use state::{AgentInfo, AppState};
use workflow_config::EffectiveLoopConfig;

pub struct MonolithExtension;

impl Extension for MonolithExtension {
    fn id(&self) -> &'static str {
        "agentropy-monolith"
    }

    fn start<'a>(&'a self, ctx: StartCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            logging::set_event_bus(ctx.host.bus.clone());
            let bus = ctx.host.bus.clone();
            let shutdown = ctx.shutdown.clone();
            tokio::spawn(async move {
                if let Err(e) = run_cli_with_shutdown(shutdown).await {
                    tracing::error!("agentropy monolith exited: {e:#}");
                }
                let _ = bus.publish(APP_DONE_TOPIC, true);
            });
            Ok(())
        })
    }
}

pub fn cli_command_is_run<I, S>(args: I) -> Result<bool>
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString> + Clone,
{
    let cli = Cli::try_parse_from(args)?;
    Ok(matches!(cli.command, Command::Run(_)))
}

pub fn host_root_from_args<I, S>(args: I) -> Result<std::path::PathBuf>
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString> + Clone,
{
    let cli = Cli::try_parse_from(args)?;
    let root = match cli.command {
        Command::Run(args) => args.resolve_root()?,
        Command::Doctor(args) => args.resolve_root()?,
        Command::InitWorkflow(args) => args.resolve_root()?,
        Command::Export(args) => args.resolve_root()?,
    };
    Ok(root)
}

pub async fn run_cli() -> Result<()> {
    run_cli_inner(Cli::parse(), None).await
}

async fn run_cli_with_shutdown(shutdown: ShutdownToken) -> Result<()> {
    run_cli_inner(Cli::parse(), Some(shutdown)).await
}

pub async fn run_parsed_cli(cli: Cli) -> Result<()> {
    run_cli_inner(cli, None).await
}

async fn run_cli_inner(cli: Cli, host_shutdown: Option<ShutdownToken>) -> Result<()> {
    match cli.command {
        Command::Run(args) => {
            let root = args.resolve_root()?;
            dotenv::load_agent_env(&root)?;
            run(root, host_shutdown).await
        }
        Command::Doctor(args) => {
            let root = args.resolve_root()?;
            let dotenv_report = dotenv::load_agent_env(&root)?;
            let code = doctor::run(&root, &dotenv_report)?;
            std::process::exit(code);
        }
        Command::InitWorkflow(args) => {
            let root = args.resolve_root()?;
            if args.linear_project_slug.is_none()
                && args.linear_project.is_none()
                && !args.expose_graphql_tool
            {
                cli::init_workflow(&root, args.force)
            } else {
                cli::init_workflow_with_options(
                    &root,
                    args.force,
                    args.linear_project_slug.as_deref(),
                    args.linear_project.as_deref(),
                    args.expose_graphql_tool,
                )
            }
        }
        Command::Export(args) => {
            let root = args.resolve_root()?;
            export_command(root)
        }
    }
}

fn export_command(root: std::path::PathBuf) -> Result<()> {
    let paths = AgentPaths::new(root);
    let result = export::export_linear_project_from_paths(&paths)?;
    println!(
        "exported {} issues to {}",
        result.issue_count,
        result.dir.display()
    );
    Ok(())
}

async fn run(root: std::path::PathBuf, host_shutdown: Option<ShutdownToken>) -> Result<()> {
    let paths = AgentPaths::new(root);

    std::fs::create_dir_all(paths.logs_dir())
        .with_context(|| format!("creating logs dir {}", paths.logs_dir().display()))?;

    let _log_guard = logging::init(&paths.log_file())?;

    let agent_cfg = config::load(&paths.root)?;
    agent_cfg.validate().context("invalid agent.yaml")?;
    let hitl = BurstHitlNotifier::from_config(&agent_cfg.hitl.notifier)?;

    let prompt = match prompt::PromptRenderer::load(&paths.workflow_md()) {
        Ok(prompt) => prompt,
        Err(e) => {
            notify_startup_error(&hitl, &format!("loading WORKFLOW.md failed: {e:#}"));
            return Err(e);
        }
    };
    let effective_cfg = EffectiveLoopConfig::merge(&agent_cfg, &prompt.snapshot().frontmatter);

    let mut tracker_cfg = agent_cfg.tracker.clone();
    tracker_cfg.use_ = effective_cfg.tracker_kind.clone();
    tracker_cfg.active_states = effective_cfg.active_states.clone();
    tracker_cfg.terminal_states = effective_cfg.terminal_states.clone();
    tracker_cfg.project_slug = effective_cfg.tracker_project_slug.clone();
    tracker_cfg.endpoint = Some(effective_cfg.tracker_endpoint.clone());
    tracker_cfg.needs_human = effective_cfg.needs_human.clone();
    let tracker = match tracker::build(&tracker_cfg, &paths) {
        Ok(tracker) => tracker,
        Err(e) => {
            notify_startup_error(&hitl, &format!("building tracker failed: {e:#}"));
            return Err(e);
        }
    };

    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let store = Arc::new(
        match store::Store::open(&paths.store_db()).context("opening SQLite persistence store") {
            Ok(store) => store,
            Err(e) => {
                notify_startup_error(&hitl, &format!("{e:#}"));
                return Err(e);
            }
        },
    );
    match store.open_run_pids() {
        Ok(pids) if !pids.is_empty() => {
            for &pid in &pids {
                runner::term_then_kill(pid, std::time::Duration::from_secs(5));
            }
            runner::wait_for_pids_dead(&pids, std::time::Duration::from_secs(8));
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("loading stale run PIDs failed: {e:#}"),
    }
    if let Err(e) = store.mark_crashed_runs() {
        tracing::warn!("mark_crashed_runs failed: {e:#}");
    }
    let history_seed = store
        .load_recent_runs(state::HistoryRing::CAP)
        .unwrap_or_default();

    let agent_info = AgentInfo {
        id: agent_cfg.id.clone(),
        folder: paths.root.display().to_string(),
        tracker: effective_cfg.tracker_kind.clone(),
        runner: effective_cfg.runner_kind.clone(),
    };
    let app_state = AppState::new(agent_info, control_tx, Arc::clone(&store), history_seed);

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let dash_workflow_snapshot = prompt.snapshot().clone();

    let orchestrator = orchestrator::Orchestrator::with_hitl_notifier(
        agent_cfg.clone(),
        paths.clone(),
        Arc::clone(&tracker),
        prompt,
        effective_cfg.clone(),
        app_state.clone(),
        control_rx,
        Arc::clone(&hitl),
    );

    let orch_shutdown = shutdown_rx.clone();
    let mut orch_task = tokio::spawn(async move { orchestrator.run(orch_shutdown).await });

    let bind = effective_cfg.dashboard_bind;
    let port = effective_cfg.dashboard_port;
    let dash_state = app_state.clone();
    let dash_agent_cfg = agent_cfg.clone();
    let dash_paths = paths.clone();
    let dash_effective_cfg = effective_cfg.clone();
    let dash_shutdown = shutdown_rx.clone();
    let mut dash_task = tokio::spawn(async move {
        dashboard::serve(
            dash_state,
            dashboard::ServeConfig {
                agent_cfg: dash_agent_cfg,
                paths: dash_paths,
                workflow_snapshot: dash_workflow_snapshot,
                effective_cfg: dash_effective_cfg,
                bind,
                port,
            },
            dash_shutdown,
        )
        .await
    });

    tokio::select! {
        result = &mut dash_task => {
            let err = startup_task_error("dashboard", result);
            notify_startup_error(&hitl, &format!("{err:#}"));
            let _ = shutdown_tx.send(true);
            let _ = orch_task.await;
            return Err(err);
        }
        result = &mut orch_task => {
            let err = startup_task_error("orchestrator", result);
            notify_startup_error(&hitl, &format!("{err:#}"));
            let _ = shutdown_tx.send(true);
            let _ = dash_task.await;
            return Err(err);
        }
        _ = tokio::time::sleep(Duration::from_millis(100)) => {}
    }

    logging::ev(
        "-",
        "startup",
        &format!("agentropy running; dashboard on http://{bind}:{port}/"),
    );

    wait_for_signal(host_shutdown).await?;
    logging::ev("-", "shutdown", "signal received, stopping");
    let _ = store.insert_event(&store::NewEvent {
        run_id: None,
        issue_identifier: "-",
        kind: "lifecycle",
        payload: "shutdown signal received, stopping",
        ts: chrono::Utc::now(),
    });
    let _ = shutdown_tx.send(true);

    let _ = orch_task.await;
    let _ = dash_task.await;
    hitl.stop();

    Ok(())
}

fn notify_startup_error(hitl: &Arc<dyn HitlNotify>, message: &str) {
    hitl.notify(HitlNotification::new("startup-error", "-", message));
    hitl.stop();
}

fn startup_task_error(
    name: &str,
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
) -> anyhow::Error {
    match result {
        Ok(Ok(())) => anyhow!("{name} exited during startup"),
        Ok(Err(e)) => e.context(format!("{name} startup failed")),
        Err(e) => anyhow!("{name} task failed during startup: {e}"),
    }
}

async fn wait_for_signal(host_shutdown: Option<ShutdownToken>) -> Result<()> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).context("installing SIGTERM handler")?;
    if let Some(mut host_shutdown) = host_shutdown {
        tokio::select! {
            r = tokio::signal::ctrl_c() => { r.context("installing SIGINT handler")?; }
            _ = term.recv() => {}
            _ = host_shutdown.cancelled() => {}
        }
    } else {
        tokio::select! {
            r = tokio::signal::ctrl_c() => { r.context("installing SIGINT handler")?; }
            _ = term.recv() => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod host_boot_tests {
    use super::*;

    #[test]
    fn host_root_uses_run_dir_arg() {
        let temp = tempfile::tempdir().unwrap();
        let root = host_root_from_args([
            "agentropy".into(),
            "run".into(),
            "--dir".into(),
            temp.path().as_os_str().to_os_string(),
        ])
        .unwrap();
        assert_eq!(root, temp.path().canonicalize().unwrap());
    }

    #[test]
    fn foreground_host_is_only_used_for_run_command() {
        assert!(cli_command_is_run(["agentropy", "run"]).unwrap());
        assert!(!cli_command_is_run(["agentropy", "export"]).unwrap());
        assert!(!cli_command_is_run(["agentropy", "init-workflow"]).unwrap());
    }

    #[test]
    fn clap_display_errors_remain_successful_exits() {
        let err = Cli::try_parse_from(["agentropy", "--help"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
        let err = Cli::try_parse_from(["agentropy", "run", "--help"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
    }
}
