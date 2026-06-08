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

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::{mpsc, watch};

use cli::{Cli, Command};
use paths::AgentPaths;
use state::{AgentInfo, AppState};

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
            // doctor sets up no file logging; findings go to stderr.
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

    // Ensure logs/ exists before the appender opens the file.
    std::fs::create_dir_all(paths.logs_dir())
        .with_context(|| format!("creating logs dir {}", paths.logs_dir().display()))?;

    // WorkerGuard must live for the whole process or buffered logs are dropped.
    let _log_guard = logging::init(&paths.log_file())?;

    let cfg = config::load(&paths.root)?;
    cfg.validate().context("invalid agent.yaml")?;

    let tracker = tracker::build(&cfg.tracker, &paths)?;
    let prompt = prompt::PromptRenderer::load(&paths.workflow_md())?;

    // Control channel: dashboard -> orchestrator.
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

    let agent_info = AgentInfo {
        id: cfg.id.clone(),
        folder: paths.root.display().to_string(),
        tracker: cfg.tracker.use_.clone(),
        runner: cfg.runner.use_.clone(),
    };
    let app_state = AppState::new(agent_info, control_tx, Arc::clone(&store), history_seed);

    // Shutdown signal observed by both tasks.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let orchestrator = orchestrator::Orchestrator::new(
        cfg.clone(),
        paths.clone(),
        Arc::clone(&tracker),
        prompt,
        app_state.clone(),
        control_rx,
    );

    let orch_shutdown = shutdown_rx.clone();
    let orch_task = tokio::spawn(async move { orchestrator.run(orch_shutdown).await });

    let bind = cfg.dashboard.bind;
    let port = cfg.dashboard.port;
    let dash_state = app_state.clone();
    let dash_shutdown = shutdown_rx.clone();
    let dash_task =
        tokio::spawn(async move { dashboard::serve(dash_state, bind, port, dash_shutdown).await });

    logging::ev(
        "-",
        "startup",
        &format!("agentropy running; dashboard on http://{bind}:{port}/"),
    );

    // Wait for ctrl_c or SIGTERM, then signal graceful shutdown.
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

    // Let both tasks wind down. Orchestrator kills the active child first.
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
