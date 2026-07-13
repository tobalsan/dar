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

/// Callback invoked when an extension fails to start (or boot fails before the
/// foreground runs). Lets the composition root surface the failure (e.g. via the
/// HITL notifier) before the host exits. Receives the failing extension id (or
/// `"-"` for boot-phase failures with no single owner) and the error message.
pub type StartupErrorHook = Arc<dyn Fn(&str, &str) + Send + Sync>;

pub struct HostOptions {
    pub root: PathBuf,
    pub http_enabled: bool,
    pub http_bind: std::net::IpAddr,
    pub http_port: u16,
    pub load_dotenv: bool,
    pub foreground: Option<String>,
    pub interactive: Option<bool>,
    pub on_startup_error: Option<StartupErrorHook>,
    pub config: ConfigStore,
    /// The resolved WORKFLOW.md's directory, if it differs from `root`. Fed
    /// to `HostPaths::with_workflow_root`; `None` keeps `workflow_root ==
    /// root` (today's default-workflow layout).
    pub workflow_root: Option<PathBuf>,
    /// Skip extensions whose `Extension::agent_singleton()` returns `true`
    /// (schedulers, chat surfaces) — set for non-default `--workflow`
    /// processes so an agent identity's singleton external connections are
    /// only opened by its default-workflow process.
    pub skip_agent_singletons: bool,
    /// Host-owned root for immutable artifact vaults. Never defaults to agent root.
    pub artifact_root: Option<PathBuf>,
}

impl HostOptions {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            http_enabled: true,
            http_bind: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            http_port: 8080,
            load_dotenv: true,
            foreground: None,
            interactive: None,
            on_startup_error: None,
            config: ConfigStore::default(),
            workflow_root: None,
            skip_agent_singletons: false,
            artifact_root: None,
        }
    }

    /// Override the workflow root (see [`HostOptions::workflow_root`]).
    pub fn workflow_root(mut self, workflow_root: impl Into<PathBuf>) -> Self {
        self.workflow_root = Some(workflow_root.into());
        self
    }

    /// Skip agent-singleton extensions at boot (see
    /// [`HostOptions::skip_agent_singletons`]).
    pub fn skip_agent_singletons(mut self, skip: bool) -> Self {
        self.skip_agent_singletons = skip;
        self
    }

    /// Provide the per-extension config store extensions read at register/start.
    pub fn config(mut self, config: ConfigStore) -> Self {
        self.config = config;
        self
    }

    /// Register a callback fired when any extension's `register`/`start` or the
    /// foreground handoff returns an error during boot.
    pub fn on_startup_error(mut self, hook: impl Fn(&str, &str) + Send + Sync + 'static) -> Self {
        self.on_startup_error = Some(Arc::new(hook));
        self
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

    /// Configure host-owned immutable artifact storage.
    pub fn artifact_root(mut self, artifact_root: impl Into<PathBuf>) -> Self {
        self.artifact_root = Some(artifact_root.into());
        self
    }

    pub fn http_addr(mut self, bind: std::net::IpAddr, port: u16) -> Self {
        self.http_bind = bind;
        self.http_port = port;
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
    let on_error = options.on_startup_error.clone();
    let report = |id: &str, err: &anyhow::Error| {
        if let Some(hook) = &on_error {
            hook(id, &format!("{err:#}"));
        }
    };
    boot_inner(extensions, options, &report).await
}

async fn boot_inner(
    extensions: Vec<Arc<dyn Extension>>,
    options: HostOptions,
    report: &impl Fn(&str, &anyhow::Error),
) -> Result<()> {
    if options.load_dotenv {
        load_dotenv(&options.root).inspect_err(|e| report("-", e))?;
    }
    let paths = HostPaths::new(&options.root).inspect_err(|e| report("-", e))?;
    let paths = if let Some(artifact_root) = &options.artifact_root {
        paths
            .with_artifact_root(artifact_root)
            .inspect_err(|e| report("-", e))?
    } else {
        paths
    };
    let paths = if let Some(workflow_root) = &options.workflow_root {
        paths
            .with_workflow_root(workflow_root)
            .inspect_err(|e| report("-", e))?
    } else {
        paths
    };
    // Filter once, before register, so register/start/foreground-select all
    // see the same set of extensions.
    let extensions: Vec<Arc<dyn Extension>> = if options.skip_agent_singletons {
        extensions
            .into_iter()
            .filter(|extension| {
                if extension.agent_singleton() {
                    tracing::info!(
                        extension = extension.id(),
                        "skipping agent-singleton extension"
                    );
                    false
                } else {
                    true
                }
            })
            .collect()
    } else {
        extensions
    };
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let shutdown_on_signal = shutdown_tx.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = shutdown_on_signal.send(true);
        }
    });
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
        config: options.config.clone(),
        shutdown: shutdown.clone(),
    };

    for extension in &extensions {
        extension
            .register(&mut register_ctx)
            .await
            .inspect_err(|e| report(extension.id(), e))?;
    }
    let foreground = register_ctx
        .foreground
        .select(options.foreground.as_deref())
        .inspect_err(|e| report("-", e))?;
    let config = register_ctx.config.clone();
    let host = register_ctx.into_start_services()?;
    let http_router = host.router.clone();
    let http_shutdown = shutdown.clone();
    let http_task = if options.http_enabled {
        let bind = options.http_bind;
        let port = options.http_port;
        // Bind synchronously so an occupied port fails the boot before any
        // extension starts (and before anything can announce the HTTP URL).
        let listener = tokio::net::TcpListener::bind((bind, port))
            .await
            .with_context(|| format!("binding host HTTP on {bind}:{port}"))
            .inspect_err(|e| report("-", e))?;
        // Surface the *actual* bound address so extensions can learn the
        // OS-assigned port when booting on `:0` (ephemeral). Set before any
        // extension `start` runs.
        if let Ok(addr) = listener.local_addr() {
            host.set_http_addr(addr);
        }
        Some(tokio::spawn(async move {
            axum::serve(listener, http_router.as_ref().clone().into_make_service())
                .with_graceful_shutdown(async move {
                    let mut shutdown = http_shutdown;
                    shutdown.cancelled().await;
                })
                .await
                .context("host HTTP server")
        }))
    } else {
        None
    };

    for extension in extensions {
        let ctx = host_api::StartCtx {
            shutdown: shutdown.clone(),
            paths: paths.clone(),
            config: config.clone(),
            host: host.clone(),
        };
        extension
            .start(ctx)
            .await
            .inspect_err(|e| report(extension.id(), e))?;
    }

    if let Some(provider) = foreground {
        let mut foreground = (provider.factory)();
        let ctx = host_api::StartCtx {
            shutdown: shutdown.clone(),
            paths: paths.clone(),
            config: config.clone(),
            host: host.clone(),
        };
        let interactive = options.interactive.unwrap_or_else(stdout_is_terminal);
        // Only foregrounds that asked for raw mode (e.g. the TUI) get a
        // raw-mode/alt-screen terminal. A cooked foreground (the logs line
        // stream) stays in cooked mode even on a tty so the terminal driver
        // keeps turning Ctrl-C into SIGINT for the host's shutdown path.
        let terminal = acquire_terminal(interactive && provider.raw_mode)
            .inspect_err(|e| report(&provider.id, e))?;
        foreground
            .run(ctx, terminal)
            .await
            .inspect_err(|e| report(&provider.id, e))?;
    }

    let _ = shutdown_tx.send(true);
    if let Some(task) = http_task {
        let _ = task.await?;
    }
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

    /// Boot options for tests: HTTP on an ephemeral port so the synchronous
    /// listener bind never collides across concurrently running tests.
    fn test_options(root: &std::path::Path) -> HostOptions {
        HostOptions::new(root)
            .without_dotenv()
            .http_addr(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 0)
    }

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
        raw_mode: bool,
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
                let factory = Arc::new(move || {
                    Box::new(RecordingForeground {
                        id,
                        log: Arc::clone(&log),
                    }) as Box<dyn host_api::Foreground>
                });
                if self.raw_mode {
                    ctx.foreground.foreground_raw_mode(id, factory)?;
                } else {
                    ctx.foreground.foreground(id, factory)?;
                }
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
            test_options(temp.path()),
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
        boot(vec![Arc::new(ServiceExt)], test_options(temp.path()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn host_options_config_reaches_extension_at_start() {
        struct ConfigReadingExt {
            seen: Arc<Mutex<Option<serde_json::Value>>>,
        }

        impl Extension for ConfigReadingExt {
            fn id(&self) -> &'static str {
                "cfg-ext"
            }

            fn start<'a>(&'a self, ctx: StartCtx) -> host_api::BoxFuture<'a, Result<()>> {
                Box::pin(async move {
                    *self.seen.lock().unwrap() = ctx.config.get("cfg-ext").cloned();
                    Ok(())
                })
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let seen = Arc::new(Mutex::new(None));
        let mut values = std::collections::HashMap::new();
        values.insert("cfg-ext".to_string(), serde_json::json!({ "port": 9000 }));
        boot(
            vec![Arc::new(ConfigReadingExt {
                seen: Arc::clone(&seen),
            })],
            test_options(temp.path()).config(ConfigStore::from_values(values)),
        )
        .await
        .unwrap();

        assert_eq!(
            seen.lock().unwrap().clone(),
            Some(serde_json::json!({ "port": 9000 }))
        );
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
                    raw_mode: false,
                    log: Arc::clone(&log),
                }),
            ],
            test_options(temp.path()).interactive(false),
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
                    raw_mode: false,
                    log: Arc::clone(&log),
                }),
                Arc::new(ForegroundExt {
                    id: "frontend-b",
                    foreground_id: "tui",
                    raw_mode: false,
                    log: Arc::clone(&log),
                }),
            ],
            test_options(temp.path())
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
                    raw_mode: false,
                    log: Arc::clone(&log),
                }),
                Arc::new(ForegroundExt {
                    id: "frontend-b",
                    foreground_id: "tui",
                    raw_mode: false,
                    log,
                }),
            ],
            test_options(temp.path()),
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("multiple foreground providers"));
    }

    #[tokio::test]
    async fn cooked_foreground_stays_non_interactive_even_when_tty() {
        let temp = tempfile::tempdir().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        boot(
            vec![Arc::new(ForegroundExt {
                id: "frontend",
                foreground_id: "logs",
                raw_mode: false,
                log: Arc::clone(&log),
            })],
            test_options(temp.path()).interactive(true),
        )
        .await
        .unwrap();

        // raw_mode=false => host must NOT enable raw mode even though the boot
        // is interactive, so the foreground receives a cooked terminal.
        assert_eq!(
            *log.lock().unwrap(),
            vec!["foreground:logs interactive=false"]
        );
    }

    struct SingletonExt {
        id: &'static str,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl Extension for SingletonExt {
        fn id(&self) -> &'static str {
            self.id
        }

        fn agent_singleton(&self) -> bool {
            true
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

    #[tokio::test]
    async fn skip_agent_singletons_omits_singleton_extensions_from_register_and_start() {
        let temp = tempfile::tempdir().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        boot(
            vec![
                Arc::new(RecordingExt {
                    id: "regular",
                    log: Arc::clone(&log),
                }),
                Arc::new(SingletonExt {
                    id: "singleton",
                    log: Arc::clone(&log),
                }),
            ],
            test_options(temp.path()).skip_agent_singletons(true),
        )
        .await
        .unwrap();

        assert_eq!(
            *log.lock().unwrap(),
            vec!["register:regular", "start:regular"]
        );
    }

    #[tokio::test]
    async fn agent_singletons_run_by_default() {
        let temp = tempfile::tempdir().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        boot(
            vec![
                Arc::new(RecordingExt {
                    id: "regular",
                    log: Arc::clone(&log),
                }),
                Arc::new(SingletonExt {
                    id: "singleton",
                    log: Arc::clone(&log),
                }),
            ],
            test_options(temp.path()),
        )
        .await
        .unwrap();

        assert_eq!(
            *log.lock().unwrap(),
            vec![
                "register:regular",
                "register:singleton",
                "start:regular",
                "start:singleton"
            ]
        );
    }

    struct FailingStartExt;

    impl Extension for FailingStartExt {
        fn id(&self) -> &'static str {
            "failing"
        }

        fn start<'a>(&'a self, _ctx: StartCtx) -> host_api::BoxFuture<'a, Result<()>> {
            Box::pin(async move { Err(anyhow::anyhow!("boom on start")) })
        }
    }

    #[tokio::test]
    async fn startup_error_hook_fires_on_failing_start() {
        let temp = tempfile::tempdir().unwrap();
        let captured: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&captured);
        let err = boot(
            vec![Arc::new(FailingStartExt)],
            test_options(temp.path()).on_startup_error(move |id, message| {
                sink.lock()
                    .unwrap()
                    .push((id.to_string(), message.to_string()));
            }),
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("boom on start"));
        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].0, "failing");
        assert!(captured[0].1.contains("boom on start"));
    }

    #[tokio::test]
    async fn occupied_http_port_fails_boot_before_extensions_start() {
        let temp = tempfile::tempdir().unwrap();
        let blocker = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = blocker.local_addr().unwrap().port();
        let log = Arc::new(Mutex::new(Vec::new()));
        let err = boot(
            vec![Arc::new(RecordingExt {
                id: "a",
                log: Arc::clone(&log),
            })],
            HostOptions::new(temp.path())
                .without_dotenv()
                .http_addr(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port),
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("binding host HTTP"));
        // Fail-fast: extensions registered but never started.
        assert_eq!(*log.lock().unwrap(), vec!["register:a"]);
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
    fn http_mount_at_root_does_not_panic() {
        let mut http = host_api::HttpRegistry::default();
        http.mount(HttpMount {
            namespace: "/".to_string(),
            router: Router::new().route("/content", get(|| async { "root" })),
            routes: vec!["/content".to_string()],
            claim_root: true,
        })
        .unwrap();
        let _ = http.into_router();
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
