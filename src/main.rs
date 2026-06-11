use anyhow::Result;
use clap::Parser;
use std::sync::Arc;

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
    let options = agentropy_host::HostOptions::new(root)
        .without_dotenv()
        .foreground("logs");
    agentropy_host::boot(
        vec![
            Arc::new(frontend_log::FrontendLogExtension),
            Arc::new(agentropy::MonolithExtension),
        ],
        options,
    )
    .await
}
