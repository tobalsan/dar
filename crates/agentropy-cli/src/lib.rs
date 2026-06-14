//! Reusable agentropy CLI boot wiring.

mod cli;
pub mod composer;
pub mod dash;
mod doctor;
pub mod self_check;
pub mod self_update;

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
    // `--self-check` is a boot-validation flag, handled before clap so it can be
    // passed alongside `--dir` without a dedicated subcommand. It parses the
    // config and instantiates every extension, then exits 0/non-zero.
    if let Some(root) = self_check::extract_flag(std::env::args_os())? {
        let code = self_check::run(&root, plugins).await?;
        std::process::exit(code);
    }
    let cli = Cli::parse();
    match cli.command {
        Command::Run(args) => {
            let root = args.resolve_root()?;
            self_check::guard_boot(&root)?;
            run_host(root, plugins).await
        }
        Command::Dash(args) => {
            dash::serve(dash::DashOptions::resolve(
                args.bind,
                args.port,
                args.registry_dir,
            ))
            .await
        }
        other => run_non_run_command(other, plugins).await,
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

async fn run_non_run_command(command: Command, plugins: Vec<Arc<dyn Extension>>) -> Result<()> {
    match command {
        Command::Run(_) => unreachable!("run is handled by run_host()"),
        Command::Dash(_) => unreachable!("dash is handled in run_inner()"),
        Command::Doctor(args) => {
            let root = args.resolve_root()?;
            if args.static_ {
                let target = args.target.clone().unwrap_or_else(|| {
                    if cfg!(target_arch = "aarch64") {
                        "aarch64-unknown-linux-musl".to_string()
                    } else {
                        "x86_64-unknown-linux-musl".to_string()
                    }
                });
                doctor::check_static_build_prereqs(&target)?;
            }
            let dotenv_report = dotenv::load_agent_env(&root)?;
            let services = plugin_services(&root, plugins).await?;
            let code = doctor::run(&root, &dotenv_report, services)?;
            std::process::exit(code);
        }
        Command::InitBuild(args) => composer::init_build_with_options(
            &args.resolve_root()?,
            composer::BuildOptions {
                vendor: args.vendor,
                offline: args.offline,
                ..composer::BuildOptions::default()
            },
        ),
        Command::Build(args) => composer::build_with_options(
            &args.resolve_root()?,
            composer::BuildOptions {
                vendor: args.vendor,
                offline: args.offline,
                target: args.target,
                static_: args.static_,
                universal: args.universal,
            },
        ),
        Command::LockRefresh(args) => composer::lock_refresh(&args.resolve_root()?),
        Command::Self_(args) => match args.command {
            cli::SelfCommand::Rebuild(args) => self_update::rebuild_with_options(
                &args.resolve_root()?,
                composer::BuildOptions {
                    vendor: args.vendor,
                    offline: args.offline,
                    target: args.target,
                    static_: args.static_,
                    universal: args.universal,
                },
                self_update::RestartMode::Execv,
            ),
        },
        Command::InitWorkflow(args) => {
            let root = args.resolve_root()?;
            dotenv::load_agent_env(&root)?;
            let services = plugin_services(&root, plugins).await?;
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
            let services = plugin_services(&root, plugins).await?;
            services
                .get_named::<dyn HostCommand>("export")?
                .run(serde_json::json!({ "dir": root }))
        }
    }
}

async fn plugin_services(
    root: &std::path::Path,
    plugins: Vec<Arc<dyn Extension>>,
) -> Result<ServiceRegistry> {
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
    for plugin in plugins {
        plugin.register(&mut ctx).await?;
    }
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
                "--vendor".into(),
                "--offline".into(),
            ])
            .unwrap();
            match cli.command {
                Command::InitBuild(args) => {
                    assert_eq!(
                        args.resolve_root().unwrap(),
                        temp.path().canonicalize().unwrap()
                    );
                    assert!(args.vendor);
                    assert!(args.offline);
                }
                Command::Build(args) => {
                    assert_eq!(
                        args.resolve_root().unwrap(),
                        temp.path().canonicalize().unwrap()
                    );
                    assert!(args.vendor);
                    assert!(args.offline);
                }
                _ => panic!("expected build command"),
            }
        }
    }

    #[test]
    fn build_command_parses_portable_binary_flags() {
        let temp = tempfile::tempdir().unwrap();
        let cli = Cli::try_parse_from([
            "agentropy".into(),
            "build".into(),
            "--dir".into(),
            temp.path().as_os_str().to_os_string(),
            "--target".into(),
            "x86_64-unknown-linux-musl".into(),
            "--static".into(),
        ])
        .unwrap();
        match cli.command {
            Command::Build(args) => {
                assert_eq!(args.target.as_deref(), Some("x86_64-unknown-linux-musl"));
                assert!(args.static_);
                assert!(!args.universal);
            }
            _ => panic!("expected build command"),
        }
    }

    #[test]
    fn self_rebuild_command_parses_portable_binary_flags() {
        let temp = tempfile::tempdir().unwrap();
        let cli = Cli::try_parse_from([
            "agentropy".into(),
            "self".into(),
            "rebuild".into(),
            "--dir".into(),
            temp.path().as_os_str().to_os_string(),
            "--target".into(),
            "aarch64-unknown-linux-musl".into(),
            "--static".into(),
        ])
        .unwrap();
        match cli.command {
            Command::Self_(args) => match args.command {
                cli::SelfCommand::Rebuild(args) => {
                    assert_eq!(args.target.as_deref(), Some("aarch64-unknown-linux-musl"));
                    assert!(args.static_);
                }
            },
            _ => panic!("expected self rebuild command"),
        }
    }

    #[test]
    fn build_command_parses_universal_flag() {
        let cli = Cli::try_parse_from(["agentropy", "build", "--universal"]).unwrap();
        match cli.command {
            Command::Build(args) => assert!(args.universal),
            _ => panic!("expected build command"),
        }
    }

    #[test]
    fn lock_refresh_command_parses_dir() {
        let temp = tempfile::tempdir().unwrap();
        let cli = Cli::try_parse_from([
            "agentropy".into(),
            "lock-refresh".into(),
            "--dir".into(),
            temp.path().as_os_str().to_os_string(),
        ])
        .unwrap();
        match cli.command {
            Command::LockRefresh(args) => {
                assert_eq!(
                    args.resolve_root().unwrap(),
                    temp.path().canonicalize().unwrap()
                );
            }
            _ => panic!("expected lock-refresh command"),
        }
    }

    #[test]
    fn self_rebuild_command_parses_dir() {
        let temp = tempfile::tempdir().unwrap();
        let cli = Cli::try_parse_from([
            "agentropy".into(),
            "self".into(),
            "rebuild".into(),
            "--dir".into(),
            temp.path().as_os_str().to_os_string(),
        ])
        .unwrap();
        match cli.command {
            Command::Self_(args) => match args.command {
                cli::SelfCommand::Rebuild(args) => {
                    assert_eq!(
                        args.resolve_root().unwrap(),
                        temp.path().canonicalize().unwrap()
                    );
                }
            },
            _ => panic!("expected self rebuild command"),
        }
    }
}
