//! Tracing setup and the structured event helper.
//!
//! `init` wires a non-blocking file appender at `./logs/agent.log` plus a
//! human-readable stderr layer (PRD: agentropy stderr also prints to terminal).
//! `ev` emits the structured-ish event line the PRD describes:
//! `time level issue=ID event=... msg=...`.

use std::path::Path;

use anyhow::{Context, Result};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// Initialize the global tracing subscriber. Returns the appender's
/// `WorkerGuard`, which the caller MUST keep alive for the whole process or
/// buffered log lines are dropped on exit.
pub fn init(log_file: &Path) -> Result<WorkerGuard> {
    let dir = log_file
        .parent()
        .context("log file path has no parent directory")?;
    let file_name = log_file
        .file_name()
        .context("log file path has no file name")?;

    let appender = tracing_appender::rolling::never(dir, file_name);
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,agentropy=info"));

    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_target(false)
        .with_writer(non_blocking);

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_writer(std::io::stderr);

    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stderr_layer)
        .init();

    Ok(guard)
}

/// Emit one structured lifecycle/runner event. `issue` is the issue identifier
/// (or `"-"` for process-level events), `event` is the event kind, `msg` is the
/// free-form message.
pub fn ev(issue: &str, event: &str, msg: &str) {
    tracing::info!(issue = %issue, event = %event, "{msg}");
}
