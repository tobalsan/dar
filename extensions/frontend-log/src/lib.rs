use std::sync::Arc;

use anyhow::Result;
use host_api::{
    ExclusiveTerminal, Extension, Foreground, LogEvent, RegisterCtx, StartCtx, APP_DONE_TOPIC,
    LOG_EVENTS_TOPIC,
};

pub struct FrontendLogExtension;

impl Extension for FrontendLogExtension {
    fn id(&self) -> &'static str {
        "frontend-log"
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            ctx.bus
                .register_broadcast::<LogEvent>(LOG_EVENTS_TOPIC, 1024)?;
            ctx.bus.register_retained::<bool>(APP_DONE_TOPIC, false)?;
            ctx.foreground
                .foreground("logs", Arc::new(|| Box::new(FrontendLogForeground)))?;
            Ok(())
        })
    }
}

struct FrontendLogForeground;

impl Foreground for FrontendLogForeground {
    fn run<'a>(
        &'a mut self,
        ctx: StartCtx,
        mut terminal: ExclusiveTerminal,
    ) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut shutdown = ctx.shutdown.clone();
            let mut app_done = ctx.host.bus.subscribe_retained::<bool>(APP_DONE_TOPIC)?;
            let mut events = ctx.host.bus.subscribe::<LogEvent>(LOG_EVENTS_TOPIC)?;
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    changed = app_done.changed() => {
                        if changed.is_err() || *app_done.borrow() {
                            break;
                        }
                    }
                    event = events.recv() => {
                        match event {
                            Ok(event) => {
                                writeln!(
                                    terminal.writer(),
                                    "{} {} {}",
                                    event.level,
                                    event.target,
                                    event.message
                                )?;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            }
            terminal.restore();
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn registers_logs_foreground_and_topic() {
        let temp = tempfile::tempdir().unwrap();
        let paths = host_api::HostPaths::new(temp.path()).unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let mut ctx = RegisterCtx {
            bus: host_api::EventBus::new(),
            http: host_api::HttpRegistry::disabled(),
            foreground: host_api::ForegroundRegistry::default(),
            services: host_api::ServiceRegistry::default(),
            paths,
            config: host_api::ConfigStore::default(),
            shutdown: host_api::ShutdownToken::new(rx),
        };

        FrontendLogExtension.register(&mut ctx).await.unwrap();
        assert!(ctx.bus.subscribe::<LogEvent>(LOG_EVENTS_TOPIC).is_ok());
        assert!(!ctx.bus.read_retained::<bool>(APP_DONE_TOPIC).unwrap());
        assert!(ctx.foreground.select(Some("logs")).unwrap().is_some());
        assert!(ctx.foreground.select(Some("missing")).is_err());
    }
}
