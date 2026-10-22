//! Separate processes discard ownership registries, just as an in-place exec does.
use std::path::PathBuf;
use std::process::Command;

use dar_agent_env::{load_agent_env, reload_agent_env, scrub_loaded_env};

fn subprocess(stage: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "reexec_restores_env_ownership", "--nocapture"])
        .env("DAR_TEST_REEXEC_STAGE", stage);
    command
}

fn assert_success(command: &mut Command) {
    let output = command.output().unwrap();
    assert!(output.status.success(), "subprocess failed: {output:?}");
}

#[test]
fn reexec_restores_env_ownership() {
    let stage = std::env::var("DAR_TEST_REEXEC_STAGE").unwrap_or_default();
    if stage.is_empty() {
        for scrub in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let prefix = format!("DAR_TEST_REEXEC_{}_", std::process::id());
            // Only the initial producer setup supplies/removes data keys.
            assert_success(
                subprocess("producer")
                    .env("DAR_TEST_REEXEC_ROOT", root.path())
                    .env("DAR_TEST_REEXEC_PREFIX", &prefix)
                    .env("DAR_TEST_REEXEC_SCRUB", if scrub { "yes" } else { "no" })
                    .env(format!("{prefix}EXTERNAL"), "shell")
                    .env_remove(format!("{prefix}FILE"))
                    .env_remove(format!("{prefix}ADDED")),
            );
        }
        return;
    }

    let root = PathBuf::from(std::env::var_os("DAR_TEST_REEXEC_ROOT").unwrap());
    let prefix = std::env::var("DAR_TEST_REEXEC_PREFIX").unwrap();
    let file = format!("{prefix}FILE");
    let added = format!("{prefix}ADDED");
    let external = format!("{prefix}EXTERNAL");
    let scrub = std::env::var("DAR_TEST_REEXEC_SCRUB").unwrap() == "yes";
    assert_eq!(std::env::var(&external).as_deref(), Ok("shell"));

    if stage == "grandchild" {
        assert!(std::env::var_os(&file).is_none());
        assert!(std::env::var_os(&added).is_none());
        return;
    }

    let path = root.join(".env");
    if stage == "producer" {
        assert!(std::env::var_os(&file).is_none());
        assert!(std::env::var_os(&added).is_none());
        std::fs::write(&path, format!("{file}=old\n{external}=file\n")).unwrap();
        assert_eq!(load_agent_env(&root).unwrap().loaded, vec![file.clone()]);
        std::fs::write(&path, format!("{file}=old\n{external}=file\n{added}=old\n")).unwrap();
        assert_eq!(
            reload_agent_env(&root).unwrap().reloaded,
            vec![added.clone(), file.clone()]
        );
        let mut successor = subprocess("successor");
        if scrub {
            scrub_loaded_env(&mut successor);
        }
        assert_success(&mut successor);
        // Preparing the successor must not mutate the producer's environment.
        assert_eq!(std::env::var(&file).as_deref(), Ok("old"));
        assert_eq!(std::env::var(&added).as_deref(), Ok("old"));
        return;
    }

    assert_eq!(stage, "successor");
    for key in [&file, &added] {
        if scrub {
            assert!(std::env::var_os(key).is_none());
        } else {
            assert_eq!(std::env::var(key).as_deref(), Ok("old"));
        }
    }
    let load = load_agent_env(&root).unwrap();
    if scrub {
        assert_eq!(load.loaded, vec![file.clone(), added.clone()]);
        assert_eq!(load.skipped_existing, vec![external.clone()]);
    } else {
        assert!(load.loaded.is_empty());
        assert_eq!(
            load.skipped_existing,
            vec![file.clone(), external.clone(), added.clone()]
        );
    }
    std::fs::write(&path, format!("{file}=new\n{external}=file\n{added}=new\n")).unwrap();
    let reload = reload_agent_env(&root).unwrap();
    if scrub {
        assert_eq!(reload.reloaded, vec![added.clone(), file.clone()]);
        assert_eq!(reload.skipped_external, vec![external.clone()]);
        assert_eq!(std::env::var(&file).as_deref(), Ok("new"));
        assert_eq!(std::env::var(&added).as_deref(), Ok("new"));
        let mut grandchild = subprocess("grandchild");
        scrub_loaded_env(&mut grandchild);
        assert_success(&mut grandchild);
    } else {
        // The unsanitized control reproduces stale values classified as external.
        assert!(reload.reloaded.is_empty());
        assert_eq!(
            reload.skipped_external,
            vec![added.clone(), external.clone(), file.clone()]
        );
        assert_eq!(std::env::var(&file).as_deref(), Ok("old"));
        assert_eq!(std::env::var(&added).as_deref(), Ok("old"));
    }
    assert_eq!(std::env::var(&external).as_deref(), Ok("shell"));
}
