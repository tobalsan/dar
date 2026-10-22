//! Test-only probe binary for the boot-time crashloop safety net.
//!
//! Behaviour is driven entirely by env vars so the integration test in
//! `tests/self_check_fallback.rs` can simulate a binary that passes the doctor
//! gate but fails its own boot `--self-check`:
//!
//!   * `DAR_PROBE_SELF_CHECK_EXIT` — exit code returned when invoked with
//!     `--self-check` (defaults to `0`, i.e. healthy).
//!   * The probe always runs [`dar_cli_core::self_check::guard_boot`] against
//!     the directory of its own executable on a normal (non `--self-check`)
//!     invocation, then prints `probe: booted` and exits `0` if the guard lets
//!     it through.

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--self-check") {
        if std::env::var_os("DAR_PROBE_CHECK_ENV").is_some() {
            assert_eq!(std::env::var("DAR_PROBE_FILE").as_deref(), Ok("file"));
            println!("probe: self-check inherited file value");
        }
        let code: i32 = std::env::var("DAR_PROBE_SELF_CHECK_EXIT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        std::process::exit(code);
    }

    let exe = std::env::current_exe().expect("current exe");
    let root = exe.parent().expect("exe parent").to_path_buf();

    let loaded = agent_env::load_agent_env(&root).expect("load probe env");
    if loaded
        .loaded
        .iter()
        .any(|key| key == "DAR_SELF_CHECK_FALLBACK")
    {
        // Exercise rollback when the guard itself is file-owned: allow the
        // boot check to run, but retain its registration for command scrubbing.
        std::env::remove_var("DAR_SELF_CHECK_FALLBACK");
    }

    match dar_cli_core::self_check::guard_boot(&root) {
        Ok(()) => {
            println!("probe: booted");
        }
        Err(e) => {
            eprintln!("probe: guard error: {e:#}");
            std::process::exit(7);
        }
    }
}
