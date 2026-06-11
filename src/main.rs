use anyhow::Result;
use clap::Parser;
macro_rules! plugins {
    ($($extension:expr),* $(,)?) => {
        vec![$(std::sync::Arc::new($extension) as std::sync::Arc<dyn host_api::Extension>),*]
    };
}

fn main() {
    if let Err(e) = main_inner() {
        eprintln!("agentropy: {e:#}");
        std::process::exit(1);
    }
}

#[tokio::main(worker_threads = 2)]
async fn main_inner() -> Result<()> {
    let cli = agentropy::Cli::parse();
    if !matches!(cli.command, agentropy::Command::Run(_)) {
        return agentropy::run_parsed_cli(cli).await;
    }

    let root = agentropy::host_root_from_args(std::env::args_os())?;
    let (bind, port) = agentropy::dashboard_addr_for_root(&root)?;
    let options = agentropy_host::HostOptions::new(root)
        .without_dotenv()
        .http_addr(bind, port)
        .foreground("logs");
    agentropy_host::boot(
        plugins![
            frontend_log::FrontendLogExtension,
            tracker_files::TrackerFilesExtension,
            tracker_linear::TrackerLinearExtension,
            orchestrator::OrchestratorExtension,
            dashboard::DashboardExtension::default(),
            agentropy::BuiltinRunnerExtension,
        ],
        options,
    )
    .await
}
