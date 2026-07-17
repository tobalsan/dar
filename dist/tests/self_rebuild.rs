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
    let deadline = Instant::now() + Duration::from_secs(90);
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
