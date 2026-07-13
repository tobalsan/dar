use std::sync::Arc;

use anyhow::Result;
use dar_extension_sdk::chat::{
    ArtifactReady, ChatBackend, ChatEvent, ChatRole, ChatSession, ChatSessionParams,
    CHAT_FALLBACK_BACKEND,
};
use dar_extension_sdk::orchestrator::{RunSnapshot, RUN_SNAPSHOT_TOPIC};
use dar_extension_sdk::{BoxFuture, Extension, RegisterCtx, ServiceRegistry, StartCtx};
use tokio::sync::mpsc;

struct TestExtension;

impl Extension for TestExtension {
    fn id(&self) -> &'static str {
        "test-extension"
    }

    fn register<'a>(&'a self, _ctx: &'a mut RegisterCtx) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn start<'a>(&'a self, _ctx: StartCtx) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

struct TestBackend;

impl ChatBackend for TestBackend {
    fn open<'a>(
        &'a self,
        _params: ChatSessionParams,
        _tx: mpsc::Sender<ChatEvent>,
    ) -> dar_extension_sdk::chat::BoxFuture<'a, Result<Box<dyn ChatSession>>> {
        Box::pin(async { anyhow::bail!("test backend is compile-only") })
    }
}

#[test]
fn sdk_reexports_extension_contracts() {
    let extension = TestExtension;
    assert_eq!(extension.id(), "test-extension");

    let snapshot = RunSnapshot::empty();
    assert!(snapshot.active.is_none());
    assert!(snapshot.active_runs.is_empty());
    assert_eq!(RUN_SNAPSHOT_TOPIC, "orchestrator.run-snapshot");
    assert_eq!(CHAT_FALLBACK_BACKEND, "pi");

    let _role = ChatRole::Assistant;
    let _ready: Option<ArtifactReady> = None;
    let _artifact_id = dar_extension_sdk::artifacts::ArtifactId::new();
}

#[test]
fn sdk_reexports_chat_backend_contract() {
    let mut services = ServiceRegistry::default();
    services
        .service::<dyn ChatBackend>("test", Arc::new(TestBackend))
        .unwrap();
    assert!(services.get_named::<dyn ChatBackend>("test").is_ok());
}
