use std::sync::Arc;

use anyhow::{bail, Result};
use cap_dashboard_tab::{DashboardTab, DashboardTabs};
use host_api::{Extension, RegisterCtx, StartCtx};
use serde::{Deserialize, Serialize};

pub const EXAMPLE_COMMANDS_TOPIC: &str = "example.commands";
pub const EXAMPLE_EVENTS_TOPIC: &str = "example.events";
pub const EXAMPLE_STATE_TOPIC: &str = "example.state";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExampleCommand {
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExampleEvent {
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExampleState {
    pub started: bool,
    pub data_dir: String,
    pub handled: usize,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ExampleConfig {
    pub greeting: Option<String>,
}

pub struct ExampleExtension;

pub fn extension() -> Box<dyn Extension> {
    Box::new(ExampleExtension)
}

impl Extension for ExampleExtension {
    fn id(&self) -> &'static str {
        "example"
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let cfg = match ctx.config.get(self.id()) {
                Some(value) => serde_json::from_value::<ExampleConfig>(value.clone())?,
                None => ExampleConfig::default(),
            };
            if matches!(cfg.greeting.as_deref(), Some("")) {
                bail!("example.greeting must not be empty when configured");
            }

            std::fs::create_dir_all(ctx.paths.root().join("data"))?;
            let data_dir = ctx.data_dir(self.id())?;
            ctx.bus
                .register_broadcast::<ExampleCommand>(EXAMPLE_COMMANDS_TOPIC, 16)?;
            ctx.bus
                .register_broadcast::<ExampleEvent>(EXAMPLE_EVENTS_TOPIC, 16)?;
            ctx.bus.register_retained(
                EXAMPLE_STATE_TOPIC,
                ExampleState {
                    started: false,
                    data_dir: data_dir.display().to_string(),
                    handled: 0,
                },
            )?;

            // Demo: contribute a tab to the web dashboard via the cap-style
            // dashboard-tab contract. Depends only on `cap-dashboard-tab` +
            // `host-api` — no dashboard internals, no cross-extension imports.
            DashboardTabs::shared(&mut ctx.services)?.add(Arc::new(ExampleTab));
            Ok(())
        })
    }

    fn start<'a>(&'a self, ctx: StartCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            std::fs::create_dir_all(ctx.paths.root().join("data"))?;
            let data_dir = ctx.paths.data_dir(self.id())?;
            std::fs::create_dir_all(&data_dir)?;
            let marker = data_dir.join("started.txt");
            std::fs::write(&marker, b"example extension started\n")?;

            let mut shutdown = ctx.shutdown.clone();
            let mut commands = ctx
                .host
                .bus
                .subscribe::<ExampleCommand>(EXAMPLE_COMMANDS_TOPIC)?;
            let bus = ctx.host.bus.clone();
            tokio::spawn(async move {
                let mut handled = 0_usize;
                let _ = bus.publish(
                    EXAMPLE_STATE_TOPIC,
                    ExampleState {
                        started: true,
                        data_dir: data_dir.display().to_string(),
                        handled,
                    },
                );

                loop {
                    tokio::select! {
                        _ = shutdown.cancelled() => break,
                        command = commands.recv() => {
                            match command {
                                Ok(command) => {
                                    handled += 1;
                                    let _ = bus.publish(EXAMPLE_EVENTS_TOPIC, ExampleEvent {
                                        text: command.text,
                                    });
                                    let _ = bus.publish(EXAMPLE_STATE_TOPIC, ExampleState {
                                        started: true,
                                        data_dir: data_dir.display().to_string(),
                                        handled,
                                    });
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                                    tracing::warn!(skipped, "example command subscriber lagged");
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                            }
                        }
                    }
                }
            });
            Ok(())
        })
    }
}

/// Demo dashboard tab contributed by the example extension. Returns a static
/// HTML fragment that the dashboard splices into its `#content` shell. A real
/// tab can declare its own htmx polling inside the fragment.
pub struct ExampleTab;

impl DashboardTab for ExampleTab {
    fn id(&self) -> &str {
        "example"
    }

    fn title(&self) -> &str {
        "Example"
    }

    fn render(&self) -> Result<String> {
        Ok(concat!(
            "<main style=\"padding:1rem 1.25rem\">",
            "<section class=\"panel\">",
            "<h2>Example tab</h2>",
            "<p>This fragment is contributed by the <code>example</code> extension ",
            "through the dashboard-tab contract. It is composed into the ",
            "<code>#content</code> element via an innerHTML swap.</p>",
            "</section>",
            "</main>"
        )
        .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_tab_registers_and_renders_fragment() {
        let mut services = host_api::ServiceRegistry::default();
        DashboardTabs::shared(&mut services)
            .unwrap()
            .add(Arc::new(ExampleTab));
        let registry = DashboardTabs::from_services(&services);
        let tab = registry.find("example").expect("example tab registered");
        assert_eq!(tab.title(), "Example");
        let html = tab.render().unwrap();
        assert!(html.contains("Example tab"), "fragment body present: {html}");
        assert!(!html.contains("<body"), "fragment is not a full page");
    }

    #[tokio::test]
    async fn smoke_register_start_publish_subscribe_and_shutdown() {
        let temp = tempfile::tempdir().unwrap();
        let paths = host_api::HostPaths::new(temp.path()).unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let mut ctx = RegisterCtx {
            bus: host_api::EventBus::new(),
            http: host_api::HttpRegistry::disabled(),
            foreground: host_api::ForegroundRegistry::default(),
            services: host_api::ServiceRegistry::default(),
            paths: paths.clone(),
            config: host_api::ConfigStore::default(),
            shutdown: host_api::ShutdownToken::new(shutdown_rx),
        };

        ExampleExtension.register(&mut ctx).await.unwrap();
        let mut events = ctx
            .bus
            .subscribe::<ExampleEvent>(EXAMPLE_EVENTS_TOPIC)
            .unwrap();
        let host = ctx.into_start_services().unwrap();
        ExampleExtension
            .start(StartCtx {
                shutdown: host_api::ShutdownToken::new(shutdown_tx.subscribe()),
                paths,
                config: host_api::ConfigStore::default(),
                host: host.clone(),
            })
            .await
            .unwrap();

        host.bus
            .publish(
                EXAMPLE_COMMANDS_TOPIC,
                ExampleCommand {
                    text: "ping".to_string(),
                },
            )
            .unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.text, "ping");
        let state = host
            .bus
            .read_retained::<ExampleState>(EXAMPLE_STATE_TOPIC)
            .unwrap();
        assert!(state.started);
        assert_eq!(state.handled, 1);
        assert!(std::path::Path::new(&state.data_dir)
            .join("started.txt")
            .exists());

        let _ = shutdown_tx.send(true);
    }
}
