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

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::{mpsc, watch};

use cli::{Cli, Command};
use paths::AgentPaths;
use state::{AgentInfo, AppState};
use workflow_config::EffectiveLoopConfig;

fn main() {
    if let Err(e) = main_inner() {
        eprintln!("agentropy: {e:#}");
        std::process::exit(1);
    }
}

#[tokio::main]
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
    }
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

    // Load WORKFLOW.md; parse frontmatter to derive effective loop config.
    let prompt = prompt::PromptRenderer::load(&paths.workflow_md())?;
    let effective_cfg = EffectiveLoopConfig::merge(&agent_cfg, &prompt.snapshot().frontmatter);

    // Build tracker using effective active/terminal states (WORKFLOW.md wins,
    // falls back to agent.yaml when absent).
    let mut tracker_cfg = agent_cfg.tracker.clone();
    tracker_cfg.active_states = effective_cfg.active_states.clone();
    tracker_cfg.terminal_states = effective_cfg.terminal_states.clone();
    let tracker = tracker::build(&tracker_cfg, &paths)?;

    let (control_tx, control_rx) = mpsc::unbounded_channel();

    // Open SQLite store under <root>/data/store.db; mark any crashed runs from
    // a previous invocation, then seed the in-memory history ring from SQLite.
    let store = Arc::new(
        store::Store::open(&paths.store_db()).context("opening SQLite persistence store")?,
    );
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
        tracker: agent_cfg.tracker.use_.clone(),
        runner: effective_cfg.runner_kind.clone(),
    };
    let app_state = AppState::new(agent_info, control_tx, Arc::clone(&store), history_seed);

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let orchestrator = orchestrator::Orchestrator::new(
        agent_cfg.clone(),
        paths.clone(),
        Arc::clone(&tracker),
        prompt,
        effective_cfg.clone(),
        app_state.clone(),
        control_rx,
    );

    let orch_shutdown = shutdown_rx.clone();
    let orch_task = tokio::spawn(async move { orchestrator.run(orch_shutdown).await });

    let bind = effective_cfg.dashboard_bind;
    let port = effective_cfg.dashboard_port;
    let dash_state = app_state.clone();
    let dash_shutdown = shutdown_rx.clone();
    let dash_task =
        tokio::spawn(async move { dashboard::serve(dash_state, bind, port, dash_shutdown).await });

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

    Ok(())
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
