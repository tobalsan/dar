//! Agentropy v0 binary entrypoint.
//!
//! Parses the CLI, initializes tracing (file appender + terminal stderr), and
//! dispatches to `run` (the long-running orchestrator + dashboard) or `doctor`
//! (config/template/tracker preflight).

mod cli;
mod config;
mod dashboard;
mod doctor;
mod domain;
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
use tokio::sync::{mpsc, watch};

use cli::{Cli, Command};
use hitl::{BurstHitlNotifier, HitlNotification, HitlNotify};
use paths::AgentPaths;
use state::{AgentInfo, AppState};
use workflow_config::EffectiveLoopConfig;

fn main() {
    if let Err(e) = main_inner() {
        eprintln!("agentropy: {e:#}");
        std::process::exit(1);
    }
}

#[tokio::main(worker_threads = 2)]
async fn main_inner() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Run(args) => {
            let root = args.resolve_root()?;
            run(root).await
        }
        Command::Doctor(args) => {
            let root = args.resolve_root()?;
            let code = doctor::run(&root)?;
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

/// The long-running `agentropy run` command. Wires up logging, config, tracker,
/// prompt renderer, shared state, then spawns the orchestrator loop and the
/// dashboard server, both observing a shared shutdown signal.
async fn run(root: std::path::PathBuf) -> Result<()> {
    let paths = AgentPaths::new(root);

    std::fs::create_dir_all(paths.logs_dir())
        .with_context(|| format!("creating logs dir {}", paths.logs_dir().display()))?;

    let _log_guard = logging::init(&paths.log_file())?;

    // Load agent definition (remains the fallback base for all loop config).
    let agent_cfg = config::load(&paths.root)?;
    agent_cfg.validate().context("invalid agent.yaml")?;
    let hitl = BurstHitlNotifier::from_config(&agent_cfg.hitl.notifier)?;

    // Load WORKFLOW.md; parse frontmatter to derive effective loop config.
    let prompt = match prompt::PromptRenderer::load(&paths.workflow_md()) {
        Ok(prompt) => prompt,
        Err(e) => {
            notify_startup_error(&hitl, &format!("loading WORKFLOW.md failed: {e:#}"));
            return Err(e);
        }
    };
    let effective_cfg = EffectiveLoopConfig::merge(&agent_cfg, &prompt.snapshot().frontmatter);

    // Build tracker using effective config (WORKFLOW.md wins, falls back to agent.yaml).
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

    // Open SQLite store under <root>/data/store.db; mark any crashed runs from
    // a previous invocation, then seed the in-memory history ring from SQLite.
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
            // Wait for stale workers to die before marking their runs crashed,
            // so a slow-dying process cannot still be alive when we resume slots
            // (TOCTOU). Bounded at 8s: 5s grace + 3s extra.
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

    // Dashboard displays the agent identity from agent.yaml (stable, display-only).
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

    wait_for_signal().await?;
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

/// Resolves when the process receives SIGINT (ctrl-c) or SIGTERM.
async fn wait_for_signal() -> Result<()> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).context("installing SIGTERM handler")?;
    tokio::select! {
        r = tokio::signal::ctrl_c() => { r.context("installing SIGINT handler")?; }
        _ = term.recv() => {}
    }
    Ok(())
}
