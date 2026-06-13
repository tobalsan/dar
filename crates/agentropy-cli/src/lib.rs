//! Reusable agentropy CLI boot wiring.

mod cli;
pub mod composer;
mod doctor;

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use host_api::{
    ConfigStore, EventBus, Extension, ForegroundRegistry, HostCommand, HostPaths, HttpRegistry,
    RegisterCtx, ServiceRegistry, ShutdownToken,
};
use tokio::sync::watch;

use cli::{Cli, Command};
use orchestrator::config;
use orchestrator::dotenv;
use orchestrator::hitl::{BurstHitlNotifier, HitlNotification, HitlNotify};
use orchestrator::paths::AgentPaths;
use orchestrator::prompt::PromptRenderer;
use orchestrator::workflow_config::EffectiveLoopConfig;

pub async fn run(plugins: Vec<Arc<dyn Extension>>) {
    if let Err(e) = run_inner(plugins).await {
        eprintln!("agentropy: {e:#}");
        std::process::exit(1);
    }
}

async fn run_inner(plugins: Vec<Arc<dyn Extension>>) -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Run(args) => run_host(args.resolve_root()?, plugins).await,
        other => run_non_run_command(other).await,
    }
}

/// Boot the extension host for the long-running `run` loop.
async fn run_host(root: std::path::PathBuf, plugins: Vec<Arc<dyn Extension>>) -> Result<()> {
    let (bind, port) = dashboard_addr_for_root(&root)?;
    let agent_config = config::load(&root)?;
    let foreground = agent_config.foreground.clone();
    let config = ConfigStore::from_values(agent_config.extension_configs()?);
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
    let result = agentropy_host::boot(plugins, options).await;
    hitl.stop();
    result
}

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

async fn run_non_run_command(command: Command) -> Result<()> {
    match command {
        Command::Run(_) => unreachable!("run is handled by run_host()"),
        Command::Doctor(args) => {
            let root = args.resolve_root()?;
            let dotenv_report = dotenv::load_agent_env(&root)?;
            let services = default_services(&root).await?;
            let code = doctor::run(&root, &dotenv_report, services)?;
            std::process::exit(code);
        }
        Command::InitBuild(args) => composer::init_build(&args.resolve_root()?),
        Command::Build(args) => composer::build(&args.resolve_root()?),
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
    runner_claude::RunnerClaudeExtension
        .register(&mut ctx)
        .await?;
    runner_codex::RunnerCodexExtension
        .register(&mut ctx)
        .await?;
    runner_opencode::RunnerOpenCodeExtension
        .register(&mut ctx)
        .await?;
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
                assert_eq!(
                    args.resolve_root().unwrap(),
                    temp.path().canonicalize().unwrap()
                );
            }
            _ => panic!("expected run command"),
        }
    }

    #[test]
    fn build_commands_parse_dir() {
        let temp = tempfile::tempdir().unwrap();
        for command in ["init-build", "build"] {
            let cli = Cli::try_parse_from([
                "agentropy".into(),
                command.into(),
                "--dir".into(),
                temp.path().as_os_str().to_os_string(),
            ])
            .unwrap();
            match cli.command {
                Command::InitBuild(args) | Command::Build(args) => {
                    assert_eq!(
                        args.resolve_root().unwrap(),
                        temp.path().canonicalize().unwrap()
                    );
                }
                _ => panic!("expected build command"),
            }
        }
    }
}
