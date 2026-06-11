use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use host_api::{
    ConfigStore, Extension, HostPaths, HttpRegistry, RegisterCtx, ServiceRegistry, ShutdownToken,
};
use tokio::sync::watch;

pub struct HostOptions {
    pub root: PathBuf,
    pub http_enabled: bool,
    pub load_dotenv: bool,
}

impl HostOptions {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            http_enabled: true,
            load_dotenv: true,
        }
    }

    pub fn without_dotenv(mut self) -> Self {
        self.load_dotenv = false;
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
        services: ServiceRegistry::default(),
        paths: paths.clone(),
        config: ConfigStore::default(),
        shutdown: shutdown.clone(),
    };

    for extension in &extensions {
        extension.register(&mut register_ctx).await?;
    }
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

    let _ = shutdown_tx.send(true);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use anyhow::Result;
    use axum::{routing::get, Router};
    use host_api::{assert_contained, Extension, HostPaths, HttpMount, RegisterCtx, StartCtx};

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
