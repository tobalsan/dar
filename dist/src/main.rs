//! agentropy composition root.
//!
//! Routes the `run` subcommand to the extension host (the explicit `plugins!`
//! list below is the only place that names the shipped extension mix); all other
//! subcommands resolve a minimal service registry and run synchronously.

mod cli;
mod doctor;

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use host_api::{
    plugins, ConfigStore, EventBus, Extension, ForegroundRegistry, HostCommand, HostPaths,
    HttpRegistry, RegisterCtx, ServiceRegistry, ShutdownToken,
};
use tokio::sync::watch;

use cli::{Cli, Command};
use orchestrator::config;
use orchestrator::dotenv;
use orchestrator::hitl::{BurstHitlNotifier, HitlNotification, HitlNotify};
use orchestrator::paths::AgentPaths;
use orchestrator::prompt::PromptRenderer;
use orchestrator::workflow_config::EffectiveLoopConfig;

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
        Command::Run(args) => run(args.resolve_root()?).await,
        other => run_non_run_command(other).await,
    }
}

/// Boot the extension host for the long-running `run` loop.
async fn run(root: std::path::PathBuf) -> Result<()> {
    let (bind, port) = dashboard_addr_for_root(&root)?;
    // Per-extension config from agent.yaml `extensions:`, so extensions can read
    // their own settings via the host ConfigStore (PRD-EXTENSIONS story 11).
    let agent_config = config::load(&root)?;
    let foreground = agent_config.foreground.clone();
    let config = ConfigStore::from_values(agent_config.extension_configs()?);
    // Surface any extension startup failure via the configured HITL notifier so
    // a misconfigured extension still pages the operator (PRD story 57).
    let hitl = startup_hitl(&root);
    let hitl_for_hook = Arc::clone(&hitl);
    let options = agentropy_host::HostOptions::new(root)
        .without_dotenv()
        .http_addr(bind, port)
        .config(config)
        .foreground(foreground)
        .on_startup_error(move |id, message| {
            hitl_for_hook.notify(HitlNotification::new(
                "startup-error",
                id,
                message.to_string(),
            ));
        });
    let result = agentropy_host::boot(
        plugins![
            frontend_log::FrontendLogExtension,
            tracker_files::TrackerFilesExtension,
            tracker_linear::TrackerLinearExtension,
            orchestrator::OrchestratorExtension::default(),
            dashboard::DashboardExtension::default(),
            runner_pi::RunnerPiExtension,
            runner_claude::RunnerClaudeExtension,
            runner_codex::RunnerCodexExtension,
            runner_opencode::RunnerOpenCodeExtension,
            runner_cli::RunnerCliExtension,
            runner_fake::RunnerFakeExtension,
            chat_opencode::ChatOpenCodeExtension,
            chat_pi::ChatPiExtension,
            tui::TuiExtension,
        ],
        options,
    )
    .await;
    hitl.stop();
    result
}

/// Build the HITL notifier the host startup-error hook reports through. Falls
/// back to a no-op notifier if the agent config can't be loaded yet (the boot
/// itself will then surface the underlying config error).
fn startup_hitl(root: &std::path::Path) -> Arc<dyn HitlNotify> {
    config::load(root)
        .ok()
        .and_then(|cfg| BurstHitlNotifier::from_config(&cfg.hitl.notifier).ok())
        .unwrap_or_else(|| Arc::new(orchestrator::hitl::NoopHitlNotifier))
}

fn dashboard_addr_for_root(root: &std::path::Path) -> Result<(std::net::IpAddr, u16)> {
    let paths = AgentPaths::new(root.to_path_buf());
    let agent_cfg = config::load(&paths.root)?;
    let prompt = PromptRenderer::load(&paths.workflow_md())?;
    let effective_cfg = EffectiveLoopConfig::merge(&agent_cfg, &prompt.snapshot().frontmatter);
    Ok((effective_cfg.dashboard_bind, effective_cfg.dashboard_port))
}

/// Resolve the minimal service registry the non-run subcommands need (trackers +
/// runners) and dispatch them synchronously.
async fn run_non_run_command(command: Command) -> Result<()> {
    match command {
        Command::Run(_) => unreachable!("run is handled by run()"),
        Command::Doctor(args) => {
            let root = args.resolve_root()?;
            let dotenv_report = dotenv::load_agent_env(&root)?;
            let services = default_services(&root).await?;
            let code = doctor::run(&root, &dotenv_report, services)?;
            std::process::exit(code);
        }
        Command::InitWorkflow(args) => {
            let root = args.resolve_root()?;
            dotenv::load_agent_env(&root)?;
            let services = default_services(&root).await?;
            services
                .get_named::<dyn HostCommand>("init-workflow")?
                .run(serde_json::json!({
                    "dir": root,
                    "force": args.force,
                    "linear_project_slug": args.linear_project_slug,
                    "linear_project": args.linear_project,
                    "expose_graphql_tool": args.expose_graphql_tool,
                }))
        }
        Command::Export(args) => {
            let root = args.resolve_root()?;
            dotenv::load_agent_env(&root)?;
            let services = default_services(&root).await?;
            services
                .get_named::<dyn HostCommand>("export")?
                .run(serde_json::json!({ "dir": root }))
        }
    }
}

/// Register the tracker + runner capability extensions into a throwaway context
/// and return the resulting service registry. Used by the synchronous
/// subcommands (doctor/init-workflow/export) that resolve services without
/// running the full host boot.
async fn default_services(root: &std::path::Path) -> Result<ServiceRegistry> {
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut ctx = RegisterCtx {
        bus: EventBus::new(),
        http: HttpRegistry::disabled(),
        foreground: ForegroundRegistry::default(),
        services: ServiceRegistry::default(),
        paths: HostPaths::new(root)?,
        config: ConfigStore::default(),
        shutdown: ShutdownToken::new(shutdown_rx),
    };
    tracker_files::TrackerFilesExtension
        .register(&mut ctx)
        .await?;
    tracker_linear::TrackerLinearExtension
        .register(&mut ctx)
        .await?;
    runner_pi::RunnerPiExtension.register(&mut ctx).await?;
    runner_claude::RunnerClaudeExtension.register(&mut ctx).await?;
    runner_codex::RunnerCodexExtension.register(&mut ctx).await?;
    runner_opencode::RunnerOpenCodeExtension.register(&mut ctx).await?;
    runner_cli::RunnerCliExtension.register(&mut ctx).await?;
    runner_fake::RunnerFakeExtension.register(&mut ctx).await?;
    Ok(ctx.services)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clap_display_errors_remain_successful_exits() {
        let err = Cli::try_parse_from(["agentropy", "--help"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
        let err = Cli::try_parse_from(["agentropy", "run", "--help"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
    }

    #[test]
    fn run_command_parses_dir() {
        let temp = tempfile::tempdir().unwrap();
        let cli = Cli::try_parse_from([
            "agentropy".into(),
            "run".into(),
            "--dir".into(),
            temp.path().as_os_str().to_os_string(),
        ])
        .unwrap();
        match cli.command {
            Command::Run(args) => {
                assert_eq!(args.resolve_root().unwrap(), temp.path().canonicalize().unwrap());
            }
            _ => panic!("expected run command"),
        }
    }
}
