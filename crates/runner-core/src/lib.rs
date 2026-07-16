pub mod bridge;
pub use bridge::{
    codex_mcp_bridge_args, host_tool_bridge, host_tool_bridge_with_identity, make_initialize,
    make_initialized, make_thread_start, make_turn_start, opencode_config, opencode_mcp_block,
    pi_mcp_config_args, write_opencode_config, BridgeInvocation, BRIDGE_SERVER_NAME,
};

/// Classified output from one protocol line.
pub struct ProtocolLine {
    pub row_type: &'static str,
    pub text: String,
    pub detail: String,
}

pub mod classify;
pub use classify::{
    classify_opencode_event, classify_protocol_line, extract_display_text, map_event_type,
    normalize_log_row, strip_ansi,
};

pub mod supervision;
pub use supervision::{
    common_env, effective_command, env_with_session_dir, log_ev, register_scrubbed_env_key,
    scrub_loaded_env, set_log_hook, setup_process_group, spawn_backend, spawn_line_pump, supervise,
    term_then_kill, wait_for_pids_dead, BackendSpec, EnvRemove, LogHook,
};
