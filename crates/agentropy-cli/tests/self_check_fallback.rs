//! End-to-end coverage for the boot-time crashloop safety net (ALG-244).
//!
//! Uses the `self_check_probe` helper binary as a stand-in for a freshly
//! self-updated `bin/agentropy`. The probe runs the real
//! `agentropy_cli::self_check::guard_boot`; we control whether its own
//! `--self-check` passes or fails via an env var, and assert on the resulting
//! boot behaviour.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn probe_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_self_check_probe"))
}

/// Copy the probe binary to `dest` so `current_exe()` resolves there and the
/// guard looks for `<dest>.prev` next to it.
fn install_current(dest: &Path) {
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    fs::copy(probe_path(), dest).unwrap();
    fs::set_permissions(dest, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Write a `.prev` shell script that records that it ran, then exits 0.
fn install_prev_marker(prev: &Path, marker: &Path) {
    let script = format!(
        "#!/bin/sh\necho rolled-back > {}\nexit 0\n",
        marker.display()
    );
    fs::write(prev, script).unwrap();
    fs::set_permissions(prev, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn failing_self_check_execs_into_prev() {
    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("bin");
    let current = bin.join("agentropy");
    let prev = bin.join("agentropy.prev");
    let marker = temp.path().join("rolled-back.txt");

    install_current(&current);
    install_prev_marker(&prev, &marker);

    // The current binary fails its own --self-check, so guard_boot must execv
    // into bin/agentropy.prev rather than panic or exit non-zero.
    let status = Command::new(&current)
        .env("AGENTROPY_PROBE_SELF_CHECK_EXIT", "1")
        .status()
        .unwrap();

    assert!(status.success(), "expected rollback exec to exit 0");
    assert_eq!(
        fs::read_to_string(&marker).unwrap().trim(),
        "rolled-back",
        ".prev binary should have run via execv"
    );
}

#[test]
fn failing_self_check_without_prev_errors_clearly() {
    let temp = tempfile::tempdir().unwrap();
    let current = temp.path().join("bin").join("agentropy");
    install_current(&current);

    // No .prev present: must fail with an actionable error, not crashloop.
    let output = Command::new(&current)
        .env("AGENTROPY_PROBE_SELF_CHECK_EXIT", "1")
        .output()
        .unwrap();

    assert!(!output.status.success(), "expected non-zero exit");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no rollback binary"),
        "stderr should explain the missing rollback binary: {stderr}"
    );
}

#[test]
fn healthy_self_check_boots_without_rollback() {
    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("bin");
    let current = bin.join("agentropy");
    let prev = bin.join("agentropy.prev");
    let marker = temp.path().join("rolled-back.txt");

    install_current(&current);
    install_prev_marker(&prev, &marker);

    // Healthy self-check: boot proceeds, no execv into .prev.
    let output = Command::new(&current)
        .env("AGENTROPY_PROBE_SELF_CHECK_EXIT", "0")
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("probe: booted"), "stdout: {stdout}");
    assert!(
        !marker.exists(),
        ".prev must not run when self-check passes"
    );
}
