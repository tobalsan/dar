use std::sync::Arc;

use anyhow::Result;
use host_api::{
    ExclusiveTerminal, Extension, Foreground, LogEvent, RegisterCtx, StartCtx, APP_DONE_TOPIC,
    LOG_EVENTS_TOPIC, STARTUP_BANNER_TOPIC,
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
            ctx.bus
                .register_retained::<Option<LogEvent>>(STARTUP_BANNER_TOPIC, None)?;
            ctx.foreground
                .foreground("logs", Arc::new(|| Box::new(FrontendLogForeground)))?;
            Ok(())
        })
    }
}

struct FrontendLogForeground;

fn write_event(terminal: &mut ExclusiveTerminal, event: &LogEvent) -> std::io::Result<()> {
    writeln!(
        terminal.writer(),
        "{} {} {}",
        event.level,
        event.target,
        event.message
    )
}

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
            // The banner topic is retained, so a banner published before this
            // foreground started running (e.g. during another extension's
            // start) is observed here instead of being dropped.
            let mut banner = ctx
                .host
                .bus
                .subscribe_retained::<Option<LogEvent>>(STARTUP_BANNER_TOPIC)?;
            let mut banner_pending = match banner.borrow_and_update().clone() {
                Some(event) => {
                    write_event(&mut terminal, &event)?;
                    false
                }
                None => true,
            };
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    changed = app_done.changed() => {
                        if changed.is_err() || *app_done.borrow() {
                            break;
                        }
                    }
                    changed = banner.changed(), if banner_pending => {
                        match changed {
                            Ok(()) => {
                                if let Some(event) = banner.borrow_and_update().clone() {
                                    write_event(&mut terminal, &event)?;
                                    banner_pending = false;
                                }
                            }
                            Err(_) => banner_pending = false,
                        }
                    }
                    event = events.recv() => {
                        match event {
                            Ok(event) => write_event(&mut terminal, &event)?,
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
    use std::io::Write;
    use std::sync::Mutex;

    use super::*;

    fn register_ctx(paths: host_api::HostPaths) -> RegisterCtx {
        let (_tx, rx) = tokio::sync::watch::channel(false);
        RegisterCtx {
            bus: host_api::EventBus::new(),
            http: host_api::HttpRegistry::disabled(),
            foreground: host_api::ForegroundRegistry::default(),
            services: host_api::ServiceRegistry::default(),
            paths,
            config: host_api::ConfigStore::default(),
            shutdown: host_api::ShutdownToken::new(rx),
        }
    }

    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl SharedBuf {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn registers_logs_foreground_and_topic() {
        let temp = tempfile::tempdir().unwrap();
        let paths = host_api::HostPaths::new(temp.path()).unwrap();
        let mut ctx = register_ctx(paths);

        FrontendLogExtension.register(&mut ctx).await.unwrap();
        assert!(ctx.bus.subscribe::<LogEvent>(LOG_EVENTS_TOPIC).is_ok());
        assert!(!ctx.bus.read_retained::<bool>(APP_DONE_TOPIC).unwrap());
        assert!(ctx
            .bus
            .read_retained::<Option<LogEvent>>(STARTUP_BANNER_TOPIC)
            .unwrap()
            .is_none());
        assert!(ctx.foreground.select(Some("logs")).unwrap().is_some());
        assert!(ctx.foreground.select(Some("missing")).is_err());
    }

    /// Boot-ordering regression test: a banner published BEFORE the foreground
    /// runs (the host runs every extension's `start` first) must still reach
    /// the terminal. With a plain broadcast topic it was silently dropped.
    #[tokio::test]
    async fn banner_published_before_run_reaches_the_foreground() {
        let temp = tempfile::tempdir().unwrap();
        let paths = host_api::HostPaths::new(temp.path()).unwrap();
        let mut ctx = register_ctx(paths.clone());
        FrontendLogExtension.register(&mut ctx).await.unwrap();
        let config = ctx.config.clone();
        let host = ctx.into_start_services().unwrap();

        // Stub publisher acting like an extension's start(): no subscriber yet.
        host.bus
            .publish(
                STARTUP_BANNER_TOPIC,
                Some(LogEvent {
                    level: "INFO".to_string(),
                    target: "issue=- event=startup".to_string(),
                    message: "dar running; dashboard on http://127.0.0.1:7878/".to_string(),
                }),
            )
            .unwrap();

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        shutdown_tx.send(true).unwrap();
        let start_ctx = StartCtx {
            shutdown: host_api::ShutdownToken::new(shutdown_rx),
            paths,
            config,
            host,
        };
        let buf = SharedBuf::default();
        let terminal = ExclusiveTerminal::non_interactive(Box::new(buf.clone()));
        FrontendLogForeground
            .run(start_ctx, terminal)
            .await
            .unwrap();

        assert_eq!(
            buf.contents(),
            "INFO issue=- event=startup dar running; dashboard on http://127.0.0.1:7878/\n"
        );
    }

    /// A banner published while the foreground is already running prints too
    /// (the orchestrator emits it after its first tick, which can land on
    /// either side of the foreground's subscription).
    #[tokio::test]
    async fn banner_published_while_running_reaches_the_foreground() {
        let temp = tempfile::tempdir().unwrap();
        let paths = host_api::HostPaths::new(temp.path()).unwrap();
        let mut ctx = register_ctx(paths.clone());
        FrontendLogExtension.register(&mut ctx).await.unwrap();
        let config = ctx.config.clone();
        let host = ctx.into_start_services().unwrap();

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let start_ctx = StartCtx {
            shutdown: host_api::ShutdownToken::new(shutdown_rx),
            paths,
            config,
            host: host.clone(),
        };
        let buf = SharedBuf::default();
        let terminal = ExclusiveTerminal::non_interactive(Box::new(buf.clone()));
        let task =
            tokio::spawn(async move { FrontendLogForeground.run(start_ctx, terminal).await });

        host.bus
            .publish(
                STARTUP_BANNER_TOPIC,
                Some(LogEvent {
                    level: "INFO".to_string(),
                    target: "issue=- event=startup".to_string(),
                    message: "late banner".to_string(),
                }),
            )
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !buf.contents().contains("late banner") {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("banner never reached the foreground");

        shutdown_tx.send(true).unwrap();
        task.await.unwrap().unwrap();
        assert_eq!(buf.contents(), "INFO issue=- event=startup late banner\n");
    }
}
