//! Integration tests link `cap-chat` as an external crate: passing here is
//! the proof that the `#[non_exhaustive]` `ChatSessionParams` (no struct-literal
//! allowed outside the crate) is constructible through the builder alone.

use std::path::Path;

use cap_chat::{ChatRole, ChatSessionParams, CHAT_FALLBACK_BACKEND};

#[test]
fn chat_session_params_builder_round_trips_all_fields() {
    let params = ChatSessionParams::builder(
        "pi-custom",
        Path::new("/agent"),
        Path::new("/agent/data/tui/sessions"),
    )
    .model(Some("gpt-5".into()))
    .build();

    assert_eq!(params.command, "pi-custom");
    assert_eq!(params.agent_root, Path::new("/agent"));
    assert_eq!(params.session_dir, Path::new("/agent/data/tui/sessions"));
    assert_eq!(params.model.as_deref(), Some("gpt-5"));
}

#[test]
fn chat_session_params_builder_defaults_optional_fields() {
    let params = ChatSessionParams::builder(
        "",
        Path::new("/agent"),
        Path::new("/agent/data/tui/sessions"),
    )
    .build();

    assert_eq!(params.command, "");
    assert_eq!(params.model, None);
}

#[test]
fn chat_fallback_backend_matches_contract() {
    assert_eq!(CHAT_FALLBACK_BACKEND, "pi");
}

#[test]
fn chat_role_variants_exist() {
    assert_ne!(ChatRole::Assistant, ChatRole::Thinking);
    // Exhaustive match: a new variant is a compile error here.
    for role in [ChatRole::Assistant, ChatRole::Thinking] {
        match role {
            ChatRole::Assistant | ChatRole::Thinking => {}
        }
    }
}
