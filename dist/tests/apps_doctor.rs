use std::process::Command;

#[test]
fn doctor_warns_when_apps_sandbox_uses_the_dashboard_origin() {
    let agent = tempfile::tempdir().unwrap();
    std::fs::write(
        agent.path().join("agent.yaml"),
        "id: apps-doctor\nname: Apps doctor\nrunner:\n  use: fake\ndashboard:\n  bind: 127.0.0.1\n  port: 47831\nextensions:\n  chat-web:\n    apps:\n      sandbox_origin: http://127.0.0.1:47831\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dar"))
        .args(["doctor", "--dir"])
        .arg(agent.path())
        .output()
        .unwrap();
    let diagnostics = String::from_utf8_lossy(&output.stderr);
    assert!(
        diagnostics.contains("MCP Apps sandbox origin matches the dashboard"),
        "{diagnostics}"
    );
}

// Public CLI seam approved by ALG-530 Sandbox config / latest origin validation.
// Catches operator-facing hostname diagnostics; misses browser runtime URL aliases.
#[test]
fn doctor_warns_when_apps_share_dashboard_cookies_across_ports() {
    for assignment in [
        "      sandbox_origin: http://127.0.0.1:47832\n",
        "      sandbox_origins:\n        canvas: http://127.0.0.1:47832\n",
    ] {
        let agent = tempfile::tempdir().unwrap();
        std::fs::write(
            agent.path().join("agent.yaml"),
            format!("id: apps-doctor\nname: Apps doctor\nrunner:\n  use: fake\ndashboard:\n  bind: 127.0.0.1\n  port: 47831\nextensions:\n  chat-web:\n    apps:\n{assignment}"),
        )
        .unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_dar"))
            .args(["doctor", "--dir"])
            .arg(agent.path())
            .output()
            .unwrap();
        let diagnostics = String::from_utf8_lossy(&output.stderr);
        assert!(
            diagnostics.contains("MCP Apps sandbox hostname matches the dashboard"),
            "{diagnostics}"
        );
    }
}
