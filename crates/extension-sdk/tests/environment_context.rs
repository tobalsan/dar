use std::path::Path;
use std::process::Command;

use dar_extension_sdk::chat::{
    agent_session_params, SystemContext, SystemContextFile, SYSTEM_CONTEXT_TOPIC,
};
use dar_extension_sdk::{
    ConfigStore, EventBus, HostPaths, RegisterCtx, ServiceRegistry, ShutdownToken, StartCtx,
};
use tokio::sync::watch;

fn start_ctx(root: &Path, identity: SystemContext) -> StartCtx {
    let paths = HostPaths::new(root).unwrap();
    let (_, shutdown) = watch::channel(false);
    let mut bus = EventBus::new();
    bus.register_retained(SYSTEM_CONTEXT_TOPIC, identity)
        .unwrap();
    let register = RegisterCtx {
        bus,
        http: Default::default(),
        foreground: Default::default(),
        services: ServiceRegistry::default(),
        paths: paths.clone(),
        config: ConfigStore::default(),
        shutdown: ShutdownToken::new(shutdown),
    };
    StartCtx {
        shutdown: register.shutdown.clone(),
        paths,
        config: register.config.clone(),
        host: register.into_start_services().unwrap(),
    }
}

fn identity() -> SystemContext {
    SystemContext {
        text: "[AGENTS.md]\nYou are Iris.\n[/AGENTS.md]".into(),
        files: vec![SystemContextFile {
            path: "AGENTS.md".into(),
        }],
        reflect: false,
    }
}

fn command_output(command: &str, args: &[&str]) -> String {
    let output = Command::new(command).args(args).output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

#[test]
fn default_chat_context_identifies_the_host_without_changing_retained_identity() {
    let root = tempfile::tempdir().unwrap();
    let original = identity();
    let ctx = start_ctx(root.path(), original.clone());
    let params = agent_session_params(&ctx, root.path()).build();
    let prompt = params.system_prompt.unwrap();
    assert!(prompt.starts_with(
        "[AGENTS.md]\nYou are Iris.\n[/AGENTS.md]\n\n[ENVIRONMENT]\ninterface: chat\n"
    ));
    assert!(prompt.contains(&format!("\nhost: {}\n", command_output("hostname", &[]))));
    assert!(prompt.contains(&format!(
        "\nos_user: {}\n[END ENVIRONMENT]",
        command_output("id", &["-un"])
    )));
    assert_eq!(prompt.matches("[ENVIRONMENT]").count(), 1);
    assert_eq!(
        ctx.host
            .bus
            .read_retained::<SystemContext>(SYSTEM_CONTEXT_TOPIC)
            .unwrap(),
        original
    );
}

#[test]
fn out_of_tree_surface_can_select_its_interface_without_replacing_identity() {
    let root = tempfile::tempdir().unwrap();
    let ctx = start_ctx(root.path(), identity());
    let params =
        dar_extension_sdk::chat::agent_session_params_with_interface(&ctx, root.path(), "telegram")
            .model(Some("custom-model".into()))
            .build();
    let prompt = params.system_prompt.unwrap();
    assert!(prompt.starts_with(
        "[AGENTS.md]\nYou are Iris.\n[/AGENTS.md]\n\n[ENVIRONMENT]\ninterface: telegram\n"
    ));
    assert_eq!(prompt.matches("[ENVIRONMENT]").count(), 1);
    assert_eq!(params.model.as_deref(), Some("custom-model"));
}

#[test]
fn agent_without_identity_keeps_the_backend_default_for_default_and_custom_interfaces() {
    let root = tempfile::tempdir().unwrap();
    for identity in [
        SystemContext::default(),
        SystemContext {
            text: "Available skills: example".into(),
            ..SystemContext::default()
        },
    ] {
        let ctx = start_ctx(root.path(), identity);
        assert!(agent_session_params(&ctx, root.path())
            .build()
            .system_prompt
            .is_none());
        assert!(
            dar_extension_sdk::chat::agent_session_params_with_interface(
                &ctx,
                root.path(),
                "telegram",
            )
            .build()
            .system_prompt
            .is_none()
        );
    }
}
