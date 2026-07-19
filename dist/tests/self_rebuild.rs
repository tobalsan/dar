use std::fs;
use std::io::{Read, Seek};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn standalone_self_rebuild_swaps_once_and_exits() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let agent = tempfile::Builder::new()
        .prefix(".dar-self-rebuild-")
        .tempdir_in(repo)
        .unwrap();
    copy_dir(&repo.join("example-agent"), agent.path());
    let agent_yaml = agent.path().join("agent.yaml");
    let config = fs::read_to_string(&agent_yaml).unwrap();
    fs::write(&agent_yaml, format!("{config}\ntracker:\n  use: files\n")).unwrap();

    run_dar(&["init-build", "--dir", agent.path().to_str().unwrap()]);
    run_dar(&["build", "--dir", agent.path().to_str().unwrap()]);
    let old = fs::read(agent.path().join("bin/dar")).unwrap();
    let canonical_agent = agent.path().canonicalize().unwrap();

    let mut stderr = tempfile::tempfile().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_dar"))
        .env("DAR_SRC", repo)
        .args(["self", "rebuild", "--dir", agent.path().to_str().unwrap()])
        .stderr(Stdio::from(stderr.try_clone().unwrap()))
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(180);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("self rebuild did not exit");
        }
        thread::sleep(Duration::from_millis(100));
    };

    stderr.rewind().unwrap();
    let mut output = String::new();
    stderr.read_to_string(&mut output).unwrap();
    assert!(status.success(), "self rebuild failed: {status}\n{output}");
    assert!(
        output.contains(&format!("dar: rebuilding {}...", canonical_agent.display())),
        "missing rebuild progress: {output}"
    );
    assert!(
        output.contains(&format!(
            "dar: rebuild complete; wrote {}",
            canonical_agent.join("bin/dar").display()
        )),
        "missing rebuild success: {output}"
    );
    assert!(agent.path().join("bin/dar").is_file());
    assert_eq!(fs::read(agent.path().join("bin/dar.prev")).unwrap(), old);
}

/// Proves the bootstrap phase: a composition change made to `agent.yaml`
/// after the initial `init-build`+`build` is picked up by a single `self
/// rebuild --dir` pass. Uses `chat-web` rather than the `scheduler` extension
/// named in the original bug report, since `example-agent/agent.yaml`
/// already opts into `scheduler` by default; `chat-web` is not selected by
/// default and its `register()` only does local bookkeeping (no process
/// spawn), so it's safe to link and doctor-gate without a `pi`/`codex`
/// binary on the host.
///
/// This exercises compose-change detection (the main rebuild's `compose()`
/// call picks up the new `chat-web` selection), the resulting lock self-heal
/// (`update_lockfile`, since the new dependency isn't in the lockfile yet),
/// and the bootstrap child invocation (`<new_binary> compose --dir <agent>`)
/// running cleanly to completion. It does NOT exercise the "second build
/// after child recompose" branch in `bootstrap_through_new_binary`: the `dar`
/// binary invoking `self rebuild` here is built from the same source tree as
/// `bin/dar.new`, so it already knows about `chat-web` and picks it up in its
/// own `compose()` call before the child ever runs — the child's recompose
/// then finds nothing new to diff. Exercising that branch needs an older
/// initiating binary (one built before the extension existed) bootstrapping
/// through a newer one, which this test does not set up.
#[test]
fn standalone_self_rebuild_bootstraps_composition_change() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let agent = tempfile::Builder::new()
        .prefix(".dar-self-rebuild-bootstrap-")
        .tempdir_in(repo)
        .unwrap();
    copy_dir(&repo.join("example-agent"), agent.path());
    let agent_yaml = agent.path().join("agent.yaml");
    let config = fs::read_to_string(&agent_yaml).unwrap();
    fs::write(&agent_yaml, format!("{config}\ntracker:\n  use: files\n")).unwrap();

    run_dar(&["init-build", "--dir", agent.path().to_str().unwrap()]);
    run_dar(&["build", "--dir", agent.path().to_str().unwrap()]);
    let old = fs::read(agent.path().join("bin/dar")).unwrap();
    let main_rs_before = fs::read_to_string(agent.path().join(".dar/src/main.rs")).unwrap();
    assert!(
        !main_rs_before.contains("chat_web::ChatWebExtension"),
        "chat-web must not be linked before the composition change: {main_rs_before}"
    );

    // Select an extra stock extension after the initial build so the next
    // rebuild has a composition change to pick up.
    let config = fs::read_to_string(&agent_yaml).unwrap();
    let config = config.replacen("  scheduler: {}\n", "  scheduler: {}\n  chat-web: {}\n", 1);
    fs::write(&agent_yaml, config).unwrap();

    let mut stderr = tempfile::tempfile().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_dar"))
        .env("DAR_SRC", repo)
        .args(["self", "rebuild", "--dir", agent.path().to_str().unwrap()])
        .stderr(Stdio::from(stderr.try_clone().unwrap()))
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(180);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("self rebuild did not exit");
        }
        thread::sleep(Duration::from_millis(100));
    };

    stderr.rewind().unwrap();
    let mut output = String::new();
    stderr.read_to_string(&mut output).unwrap();
    assert!(status.success(), "self rebuild failed: {status}\n{output}");

    let main_rs_after = fs::read_to_string(agent.path().join(".dar/src/main.rs")).unwrap();
    assert!(
        main_rs_after.contains("chat_web::ChatWebExtension"),
        "composition did not pick up chat-web: {main_rs_after}"
    );
    assert!(agent.path().join("bin/dar").is_file());
    assert_ne!(
        fs::read(agent.path().join("bin/dar")).unwrap(),
        old,
        "bin/dar was not swapped for the newly composed binary"
    );
}

fn run_dar(args: &[&str]) {
    let status = Command::new(env!("CARGO_BIN_EXE_dar"))
        .env(
            "DAR_SRC",
            Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap(),
        )
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "dar build failed: {status}");
}

fn copy_dir(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() == ".dar" || entry.file_name() == "bin" {
            continue;
        }
        let destination = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            fs::create_dir(&destination).unwrap();
            copy_dir(&entry.path(), &destination);
        } else {
            fs::copy(entry.path(), destination).unwrap();
        }
    }
}
