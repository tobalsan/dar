use anyhow::Result;
use std::sync::Arc;

macro_rules! plugins {
    ($($extension:expr),* $(,)?) => {
        vec![$(Arc::new($extension) as Arc<dyn host_api::Extension>),*]
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
    let root = agentropy::host_root_from_args(std::env::args_os())?;
    let options = agentropy_host::HostOptions::new(root).without_dotenv();
    agentropy_host::boot(plugins![agentropy::MonolithExtension], options).await
}
