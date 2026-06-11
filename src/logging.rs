//! Tracing setup and the structured event helper.
//!
//! `init` wires a non-blocking file appender at `./logs/agent.log`.
//! `ev` emits the structured-ish event line the PRD describes:
//! `time level issue=ID event=... msg=...`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use host_api::{EventBus, LogEvent, LOG_EVENTS_TOPIC};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

static EVENT_BUS: Mutex<Option<Arc<EventBus>>> = Mutex::new(None);

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

    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .init();

    Ok(guard)
}

pub fn set_event_bus(bus: Arc<EventBus>) {
    *EVENT_BUS.lock().expect("event bus mutex poisoned") = Some(bus);
}

/// Emit one structured lifecycle/runner event. `issue` is the issue identifier
/// (or `"-"` for process-level events), `event` is the event kind, `msg` is the
/// free-form message.
pub fn ev(issue: &str, event: &str, msg: &str) {
    tracing::info!(issue = %issue, event = %event, "{msg}");
    if let Some(bus) = EVENT_BUS.lock().expect("event bus mutex poisoned").as_ref() {
        let _ = bus.publish(
            LOG_EVENTS_TOPIC,
            LogEvent {
                level: "INFO".to_string(),
                target: format!("issue={issue} event={event}"),
                message: msg.to_string(),
            },
        );
    }
}
