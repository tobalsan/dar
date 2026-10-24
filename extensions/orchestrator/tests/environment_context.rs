//! Approved seam: public Orchestrator::run -> public Runner contract.
//! Covers fresh runs and retry identity; vendor encoding is exercised separately.
use std::{sync::Arc, time::Duration};

use cap_runner::{ExitKind, Runner, RunnerHandle, SpawnParams};
use orchestrator::{
    config::AgentConfig,
    hitl::NoopHitlNotifier,
    paths::AgentPaths,
    prompt::PromptRenderer,
    state::{AgentInfo, AppState},
    store::Store,
    workflow_config::{parse_workflow_md, EffectiveLoopConfig},
    Orchestrator,
};
use system_files::bus::{SystemContext, SystemContextFile};
use tokio::sync::{mpsc, oneshot, watch};
use tracker_files::FileTracker;

struct CaptureRunner(mpsc::UnboundedSender<(String, Option<String>)>);

impl Runner for CaptureRunner {
    fn supports_system_prompt(&self) -> bool {
        true
    }

    fn spawn<'a>(
        &self,
        params: SpawnParams<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<RunnerHandle>> + Send + 'a>,
    > {
        self.0.send((params.prompt, params.system_prompt)).unwrap();
        Box::pin(async {
            let (kill, _) = oneshot::channel();
            Ok(RunnerHandle::new(
                0,
                kill,
                tokio::spawn(async { ExitKind::Abnormal(Some(1)) }),
            ))
        })
    }
}

#[tokio::test]
async fn first_run_and_retry_receive_environment_with_existing_identity_channel() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("issues")).unwrap();
    std::fs::write(
        root.path().join("issues/ENV-1.md"),
        "---\nid: ENV-1\nidentifier: ENV-1\ntitle: Environment\nstate: todo\n---\nFixture issue",
    )
    .unwrap();
    let yaml = "id: fixture\nname: Fixture\nrunner:\n  use: fake\n";
    std::fs::write(root.path().join("agent.yaml"), yaml).unwrap();
    let config: AgentConfig = serde_yaml::from_str(yaml).unwrap();
    let workflow = "---\ntracker:\n  kind: files\n  path: issues\n  active_states: [todo]\n  terminal_states: [done]\npolling:\n  interval_ms: 10\n  max_concurrent: 1\n  max_retries: 1\n  retry_backoff_ms: 10\nworkspace:\n  root: workspaces\n---\nWork on {{ issue.identifier }}.";
    std::fs::write(root.path().join("WORKFLOW.md"), workflow).unwrap();
    let effective =
        EffectiveLoopConfig::resolve(&config, &parse_workflow_md(workflow).unwrap().frontmatter);
    let store = Arc::new(Store::open(&root.path().join("store.db")).unwrap());
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let state = AppState::new(
        AgentInfo {
            id: "fixture".into(),
            folder: root.path().display().to_string(),
            tracker: "files".into(),
            runner: "fake".into(),
            workflow: None,
        },
        control_tx,
        store,
        vec![],
    );
    let (tx, mut rx) = mpsc::unbounded_channel();
    let identity = SystemContext {
        text: "Identity fixture.\n".into(),
        files: vec![SystemContextFile {
            path: "AGENTS.md".into(),
        }],
        reflect: false,
    };
    let runner = Orchestrator::with_hitl_notifier(
        config,
        AgentPaths::new(root.path().to_path_buf()),
        Arc::new(FileTracker::new(
            root.path().join("issues"),
            vec!["todo".into()],
            vec!["done".into()],
        )),
        Arc::new(CaptureRunner(tx)),
        Default::default(),
        PromptRenderer::load(&root.path().join("WORKFLOW.md")).unwrap(),
        effective,
        state,
        control_rx,
        Arc::new(NoopHitlNotifier),
    )
    .with_system_context(identity);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(runner.run(shutdown_rx));
    let first = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .unwrap()
        .unwrap();
    let retry = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .unwrap()
        .unwrap();
    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    for (prompt, system) in [first, retry] {
        // Orchestration keeps its established prompt fallback even for runners
        // which support a separate system channel.
        assert!(system.is_none());
        assert!(prompt.starts_with("Identity fixture.\n"));
        assert!(
            prompt.contains("[ENVIRONMENT]\ninterface: orchestrator\nhost: "),
            "{prompt}"
        );
        assert_eq!(prompt.matches("[ENVIRONMENT]").count(), 1);
        assert!(prompt.contains("[END ENVIRONMENT]\nWork on ENV-1."));
    }
}
