//! Approved seam: scheduler lifecycle and HTTP run-now -> public Runner contract.
//! Covers local identity and both delivery channels; vendor encoding is separate.
use std::{collections::HashMap, sync::Arc, time::Duration};

use axum::{body::Body, http::Request};
use cap_runner::{ExitKind, Runner, RunnerHandle, SpawnParams};
use host_api::{ConfigStore, Extension, HostPaths, RegisterCtx, ShutdownToken, StartCtx};
use serde_json::json;
use tokio::sync::{mpsc, oneshot, watch};
use tower::ServiceExt;

struct CaptureRunner {
    system_channel: bool,
    tx: mpsc::UnboundedSender<(String, Option<String>)>,
}

impl Runner for CaptureRunner {
    fn supports_system_prompt(&self) -> bool {
        self.system_channel
    }

    fn spawn<'a>(
        &self,
        params: SpawnParams<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<RunnerHandle>> + Send + 'a>,
    > {
        self.tx.send((params.prompt, params.system_prompt)).unwrap();
        Box::pin(async {
            let (kill, _) = oneshot::channel();
            Ok(RunnerHandle::new(
                0,
                kill,
                tokio::spawn(async { ExitKind::Normal }),
            ))
        })
    }
}

#[tokio::test]
async fn scheduler_adds_environment_to_local_identity_in_each_runner_channel() {
    for system_channel in [true, false] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("agent.yaml"),
            "id: demo\nname: Demo\nrunner:\n  use: capture\n",
        )
        .unwrap();
        std::fs::write(root.path().join("AGENTS.md"), "Identity fixture.").unwrap();
        std::fs::create_dir_all(root.path().join("cron")).unwrap();
        std::fs::write(
            root.path().join("cron/jobs.json"),
            json!({"version":1,"jobs":[{
                "id":"digest", "enabled":true,
                "schedule":{"cron":"0 0 1 1 *","tz":"UTC"},
                "payload":{"message":"Summarize."}
            }]})
            .to_string(),
        )
        .unwrap();
        let paths = HostPaths::new(root.path()).unwrap();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let shutdown = ShutdownToken::new(shutdown_rx);
        let config = ConfigStore::from_values(HashMap::from([("scheduler".into(), json!({}))]));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut register = RegisterCtx {
            bus: host_api::EventBus::new(),
            http: Default::default(),
            foreground: Default::default(),
            services: Default::default(),
            paths: paths.clone(),
            config: config.clone(),
            shutdown: shutdown.clone(),
        };
        register
            .services
            .register::<dyn Runner>("capture", Arc::new(CaptureRunner { system_channel, tx }))
            .unwrap();
        let extension = scheduler::extension();
        extension.register(&mut register).await.unwrap();
        let host = register.into_start_services().unwrap();
        extension
            .start(StartCtx {
                host: host.clone(),
                paths,
                config,
                shutdown,
            })
            .await
            .unwrap();
        let response = host
            .router
            .as_ref()
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/scheduler/jobs/digest/run-now")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(response.status().is_success());
        let (prompt, system) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let identity = if system_channel {
            assert_eq!(prompt, "Summarize.");
            system.unwrap()
        } else {
            assert!(system.is_none());
            assert!(prompt.ends_with("Summarize."));
            prompt
        };
        assert!(identity.contains("Identity fixture."));
        assert!(
            identity.contains("[ENVIRONMENT]\ninterface: scheduler\nhost: "),
            "{identity}"
        );
        assert_eq!(identity.matches("[ENVIRONMENT]").count(), 1);
        shutdown_tx.send(true).unwrap();
        extension.stop().await.unwrap();
    }
}
