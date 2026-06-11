use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use host_api::{
    ConfigStore, ExclusiveTerminal, Extension, HostPaths, HttpRegistry, RegisterCtx,
    ServiceRegistry, ShutdownToken,
};
use tokio::sync::watch;

pub struct HostOptions {
    pub root: PathBuf,
    pub http_enabled: bool,
    pub load_dotenv: bool,
    pub foreground: Option<String>,
    pub interactive: Option<bool>,
}

impl HostOptions {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            http_enabled: true,
            load_dotenv: true,
            foreground: None,
            interactive: None,
        }
    }

    pub fn without_dotenv(mut self) -> Self {
        self.load_dotenv = false;
        self
    }

    pub fn foreground(mut self, id: impl Into<String>) -> Self {
        self.foreground = Some(id.into());
        self
    }

    pub fn interactive(mut self, interactive: bool) -> Self {
        self.interactive = Some(interactive);
        self
    }
}

pub async fn run(root: impl AsRef<Path>) -> Result<()> {
    run_with_extensions(root, Vec::new()).await
}

pub async fn run_with_extensions(
    root: impl AsRef<Path>,
    extensions: Vec<Arc<dyn Extension>>,
) -> Result<()> {
    boot(extensions, HostOptions::new(root.as_ref())).await
}

fn load_dotenv(root: &Path) -> Result<()> {
    let path = root.join(".env");
    if path.exists() {
        for line in std::fs::read_to_string(path)?.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                if std::env::var_os(key).is_none() {
                    std::env::set_var(key.trim(), value.trim().trim_matches('"'));
                }
            }
        }
    }
    Ok(())
}

pub async fn boot(extensions: Vec<Arc<dyn Extension>>, options: HostOptions) -> Result<()> {
    if options.load_dotenv {
        load_dotenv(&options.root)?;
    }
    let paths = HostPaths::new(&options.root)?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let shutdown = ShutdownToken::new(shutdown_rx);
    let mut register_ctx = RegisterCtx {
        bus: host_api::EventBus::new(),
        http: if options.http_enabled {
            HttpRegistry::default()
        } else {
            HttpRegistry::disabled()
        },
        foreground: host_api::ForegroundRegistry::default(),
        services: ServiceRegistry::default(),
        paths: paths.clone(),
        config: ConfigStore::default(),
        shutdown: shutdown.clone(),
    };

    for extension in &extensions {
        extension.register(&mut register_ctx).await?;
    }
    let foreground = register_ctx
        .foreground
        .select(options.foreground.as_deref())?;
    let config = register_ctx.config.clone();
    let host = register_ctx.into_start_services()?;

    for extension in extensions {
        let ctx = host_api::StartCtx {
            shutdown: shutdown.clone(),
            paths: paths.clone(),
            config: config.clone(),
            host: host.clone(),
        };
        extension.start(ctx).await?;
    }

    if let Some(provider) = foreground {
        let mut foreground = (provider.factory)();
        let ctx = host_api::StartCtx {
            shutdown: shutdown.clone(),
            paths: paths.clone(),
            config: config.clone(),
            host: host.clone(),
        };
        foreground
            .run(
                ctx,
                acquire_terminal(options.interactive.unwrap_or_else(stdout_is_terminal))?,
            )
            .await?;
    }

    let _ = shutdown_tx.send(true);
    Ok(())
}

fn stdout_is_terminal() -> bool {
    std::io::stdout().is_terminal()
}

fn acquire_terminal(interactive: bool) -> Result<ExclusiveTerminal> {
    if !interactive {
        return Ok(ExclusiveTerminal::non_interactive(Box::new(
            std::io::stdout(),
        )));
    }

    let mut stdout = std::io::stdout();
    enable_raw_mode().context("enabling terminal raw mode")?;
    if let Err(e) = stdout.execute(EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(e).context("entering alternate screen");
    }

    let previous_hook = Arc::new(Mutex::new(Some(std::panic::take_hook())));
    let hook_for_panic = Arc::clone(&previous_hook);
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        if let Some(hook) = hook_for_panic
            .lock()
            .expect("panic hook mutex poisoned")
            .as_ref()
        {
            hook(info);
        }
    }));
    let mut terminal = ExclusiveTerminal::new(true, Box::new(stdout), restore_terminal);
    terminal.set_previous_panic_hook(previous_hook);
    Ok(terminal)
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = std::io::stdout().execute(LeaveAlternateScreen);
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use anyhow::Result;
    use axum::{routing::get, Router};
    use host_api::{
        assert_contained, ExclusiveTerminal, Extension, Foreground, HostPaths, HttpMount,
        RegisterCtx, StartCtx,
    };

    use super::*;

    struct RecordingExt {
        id: &'static str,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl Extension for RecordingExt {
        fn id(&self) -> &'static str {
            self.id
        }

        fn register<'a>(
            &'a self,
            _ctx: &'a mut RegisterCtx,
        ) -> host_api::BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                self.log
                    .lock()
                    .unwrap()
                    .push(format!("register:{}", self.id));
                Ok(())
            })
        }

        fn start<'a>(&'a self, _ctx: StartCtx) -> host_api::BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                self.log.lock().unwrap().push(format!("start:{}", self.id));
                Ok(())
            })
        }
    }

    struct ForegroundExt {
        id: &'static str,
        foreground_id: &'static str,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl Extension for ForegroundExt {
        fn id(&self) -> &'static str {
            self.id
        }

        fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                let id = self.foreground_id;
                let log = Arc::clone(&self.log);
                ctx.foreground.foreground(
                    id,
                    Arc::new(move || {
                        Box::new(RecordingForeground {
                            id,
                            log: Arc::clone(&log),
                        })
                    }),
                )?;
                Ok(())
            })
        }
    }

    struct RecordingForeground {
        id: &'static str,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl Foreground for RecordingForeground {
        fn run<'a>(
            &'a mut self,
            _ctx: StartCtx,
            terminal: ExclusiveTerminal,
        ) -> host_api::BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                self.log.lock().unwrap().push(format!(
                    "foreground:{} interactive={}",
                    self.id,
                    terminal.is_interactive()
                ));
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn register_completes_for_all_extensions_before_start() {
        let temp = tempfile::tempdir().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        boot(
            vec![
                Arc::new(RecordingExt {
                    id: "a",
                    log: Arc::clone(&log),
                }),
                Arc::new(RecordingExt {
                    id: "b",
                    log: Arc::clone(&log),
                }),
            ],
            HostOptions::new(temp.path()).without_dotenv(),
        )
        .await
        .unwrap();

        assert_eq!(
            *log.lock().unwrap(),
            vec!["register:a", "register:b", "start:a", "start:b"]
        );
    }

    #[tokio::test]
    async fn typed_bus_supports_broadcast_and_retained_topics() {
        let mut bus = host_api::EventBus::new();
        bus.register_broadcast::<String>("events", 8).unwrap();
        let mut rx = bus.subscribe::<String>("events").unwrap();
        bus.publish("events", "hello".to_string()).unwrap();
        assert_eq!(rx.recv().await.unwrap(), "hello");

        bus.register_retained::<u64>("state", 1).unwrap();
        assert_eq!(bus.read_retained::<u64>("state").unwrap(), 1);
        bus.publish("state", 2_u64).unwrap();
        assert_eq!(bus.read_retained::<u64>("state").unwrap(), 2);
    }

    #[tokio::test]
    async fn registered_services_are_available_during_start() {
        trait Service: Send + Sync {
            fn value(&self) -> &'static str;
        }

        #[derive(Debug)]
        struct ServiceImpl;

        impl Service for ServiceImpl {
            fn value(&self) -> &'static str {
                "ok"
            }
        }

        struct ServiceExt;

        impl Extension for ServiceExt {
            fn id(&self) -> &'static str {
                "service-ext"
            }

            fn register<'a>(
                &'a self,
                ctx: &'a mut RegisterCtx,
            ) -> host_api::BoxFuture<'a, Result<()>> {
                Box::pin(async move {
                    let service: Arc<dyn Service> = Arc::new(ServiceImpl);
                    ctx.services.register::<dyn Service>("svc", service)?;
                    Ok(())
                })
            }

            fn start<'a>(&'a self, ctx: StartCtx) -> host_api::BoxFuture<'a, Result<()>> {
                Box::pin(async move {
                    assert_eq!(ctx.host.services.get::<dyn Service>("svc")?.value(), "ok");
                    Ok(())
                })
            }
        }

        let temp = tempfile::tempdir().unwrap();
        boot(
            vec![Arc::new(ServiceExt)],
            HostOptions::new(temp.path()).without_dotenv(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn uniquely_registered_foreground_runs_after_start() {
        let temp = tempfile::tempdir().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        boot(
            vec![
                Arc::new(RecordingExt {
                    id: "background",
                    log: Arc::clone(&log),
                }),
                Arc::new(ForegroundExt {
                    id: "frontend",
                    foreground_id: "logs",
                    log: Arc::clone(&log),
                }),
            ],
            HostOptions::new(temp.path())
                .without_dotenv()
                .interactive(false),
        )
        .await
        .unwrap();

        assert_eq!(
            *log.lock().unwrap(),
            vec![
                "register:background",
                "start:background",
                "foreground:logs interactive=false"
            ]
        );
    }

    #[tokio::test]
    async fn configured_foreground_selects_one_provider() {
        let temp = tempfile::tempdir().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        boot(
            vec![
                Arc::new(ForegroundExt {
                    id: "frontend-a",
                    foreground_id: "logs",
                    log: Arc::clone(&log),
                }),
                Arc::new(ForegroundExt {
                    id: "frontend-b",
                    foreground_id: "tui",
                    log: Arc::clone(&log),
                }),
            ],
            HostOptions::new(temp.path())
                .without_dotenv()
                .interactive(false)
                .foreground("tui"),
        )
        .await
        .unwrap();

        assert_eq!(
            *log.lock().unwrap(),
            vec!["foreground:tui interactive=false"]
        );
    }

    #[tokio::test]
    async fn multiple_foregrounds_without_selection_fail_boot() {
        let temp = tempfile::tempdir().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let err = boot(
            vec![
                Arc::new(ForegroundExt {
                    id: "frontend-a",
                    foreground_id: "logs",
                    log: Arc::clone(&log),
                }),
                Arc::new(ForegroundExt {
                    id: "frontend-b",
                    foreground_id: "tui",
                    log,
                }),
            ],
            HostOptions::new(temp.path()).without_dotenv(),
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("multiple foreground providers"));
    }

    #[test]
    fn exclusive_terminal_restores_on_drop() {
        let restored = Arc::new(Mutex::new(false));
        {
            let restored = Arc::clone(&restored);
            let _terminal = ExclusiveTerminal::new(true, Box::new(Vec::<u8>::new()), move || {
                *restored.lock().unwrap() = true;
            });
        }
        assert!(*restored.lock().unwrap());
    }

    #[test]
    fn http_mount_composes_and_detects_collisions() {
        let mut http = host_api::HttpRegistry::default();
        http.mount(HttpMount {
            namespace: "/a".to_string(),
            router: Router::new().route("/status", get(|| async { "a" })),
            routes: vec!["/status".to_string()],
            claim_root: false,
        })
        .unwrap();
        http.mount(HttpMount {
            namespace: "/b".to_string(),
            router: Router::new().route("/status", get(|| async { "b" })),
            routes: vec!["/status".to_string()],
            claim_root: false,
        })
        .unwrap();
        let _ = http.into_router();

        let mut http = host_api::HttpRegistry::default();
        http.mount(HttpMount {
            namespace: "/a".to_string(),
            router: Router::new().route("/status", get(|| async { "a" })),
            routes: vec!["/status".to_string()],
            claim_root: false,
        })
        .unwrap();
        assert!(http
            .mount(HttpMount {
                namespace: "/a".to_string(),
                router: Router::new().route("/status", get(|| async { "b" })),
                routes: vec!["/status".to_string()],
                claim_root: false,
            })
            .is_err());
    }

    #[test]
    fn data_dir_is_contained_and_assert_contained_rejects_escapes() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("data")).unwrap();
        let paths = HostPaths::new(temp.path()).unwrap();
        let dir = paths.data_dir("ext").unwrap();
        assert!(dir.starts_with(paths.root()));

        assert!(assert_contained(temp.path(), temp.path().join("../escape")).is_err());

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let outside = tempfile::tempdir().unwrap();
            symlink(outside.path(), temp.path().join("data/link")).unwrap();
            assert!(assert_contained(temp.path(), temp.path().join("data/link")).is_err());
        }
    }
}
