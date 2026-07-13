use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use anyhow::{bail, Context, Result};
use fs2::FileExt;

use crate::composer;

pub enum RestartMode {
    Execv,
    Skip,
}

pub fn rebuild(_agent: &Path, _restart: RestartMode) -> Result<()> {
    rebuild_with_options(_agent, composer::BuildOptions::default(), _restart)
}

pub fn rebuild_with_options(
    _agent: &Path,
    options: composer::BuildOptions,
    _restart: RestartMode,
) -> Result<()> {
    let agent = _agent
        .canonicalize()
        .with_context(|| format!("resolving agent folder {}", _agent.display()))?;
    ensure_self_rebuild_output_runnable(&options)?;
    with_lock(&agent, || {
        composer::compose(&agent)?;
        let new_binary = agent.join("bin/dar.new");
        if new_binary.exists() {
            fs::remove_file(&new_binary)
                .with_context(|| format!("removing stale {}", new_binary.display()))?;
        }
        composer::build_to_with_options(&agent, &new_binary, options)?;
        doctor_gate(&agent, &ProcessDoctor)?;
        atomic_swap(&agent)?;
        match _restart {
            RestartMode::Execv => exec_current_process(),
            RestartMode::Skip => Ok(()),
        }
    })
}

fn ensure_self_rebuild_output_runnable(options: &composer::BuildOptions) -> Result<()> {
    if options.universal {
        return Ok(());
    }
    let Some(target) = options.target.as_deref() else {
        return Ok(());
    };
    if target_is_host_runnable(target) {
        return Ok(());
    }
    bail!("self rebuild target `{target}` is not runnable on this host; use a host target or `build --target` for cross-built artifacts")
}

fn target_is_host_runnable(target: &str) -> bool {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => {
            matches!(
                target,
                "x86_64-unknown-linux-gnu" | "x86_64-unknown-linux-musl"
            )
        }
        ("linux", "aarch64") => {
            matches!(
                target,
                "aarch64-unknown-linux-gnu" | "aarch64-unknown-linux-musl"
            )
        }
        ("macos", "x86_64") => target == "x86_64-apple-darwin",
        ("macos", "aarch64") => target == "aarch64-apple-darwin",
        _ => false,
    }
}

trait DoctorRunner {
    fn run(&self, new_binary: &Path, agent: &Path) -> Result<()>;
}

struct ProcessDoctor;

impl DoctorRunner for ProcessDoctor {
    fn run(&self, new_binary: &Path, agent: &Path) -> Result<()> {
        let output = Command::new(new_binary)
            .args(["doctor", "--dir"])
            .arg("..")
            .current_dir(agent.join("bin"))
            .output()
            .with_context(|| format!("running doctor gate {}", new_binary.display()))?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("doctor gate failed with {}:\n{stderr}", output.status);
    }
}

fn doctor_gate(agent: &Path, doctor: &dyn DoctorRunner) -> Result<()> {
    let new_binary = agent.join("bin/dar.new");
    match doctor.run(&new_binary, agent) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&new_binary);
            Err(e).context("doctor gate failed")
        }
    }
}

fn atomic_swap(agent: &Path) -> Result<()> {
    let bin = agent.join("bin/dar");
    let new = agent.join("bin/dar.new");
    let prev = agent.join("bin/dar.prev");
    fs::rename(&bin, &prev)
        .with_context(|| format!("renaming {} to {}", bin.display(), prev.display()))?;
    if let Err(e) = fs::rename(&new, &bin) {
        let rollback = fs::rename(&prev, &bin);
        if let Err(rollback_err) = rollback {
            return Err(e).with_context(|| {
                format!(
                    "renaming {} to {} failed; rollback {} to {} also failed: {}",
                    new.display(),
                    bin.display(),
                    prev.display(),
                    bin.display(),
                    rollback_err
                )
            });
        }
        return Err(e).with_context(|| format!("renaming {} to {}", new.display(), bin.display()));
    }
    Ok(())
}

fn with_lock<T>(agent: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let _thread_guard = process_lock()
        .lock()
        .map_err(|_| anyhow::anyhow!("self-update in-process lock poisoned"))?;
    fs::create_dir_all(agent.join("data"))
        .with_context(|| format!("creating {}", agent.join("data").display()))?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(agent.join("data/self-update.lock"))
        .with_context(|| format!("opening {}", agent.join("data/self-update.lock").display()))?;
    LockGuard::lock(lock)?;
    f()
}

fn process_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

struct LockGuard(File);

impl LockGuard {
    fn lock(file: File) -> Result<Self> {
        file.lock_exclusive().context("locking self-update lock")?;
        Ok(Self(file))
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

fn exec_current_process() -> Result<()> {
    let argv0 = std::env::args_os()
        .next()
        .context("current process argv[0] is missing")?;
    let args = std::env::args_os().skip(1).collect::<Vec<OsString>>();
    let err = Command::new(&argv0).args(args).exec();
    Err(err).with_context(|| format!("execv failed for {}", Path::new(&argv0).display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn doctor_gate_abort_leaves_current_binary_and_removes_new() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path().to_path_buf();
        write_executable(&agent.join("bin/dar"), "old");
        write_executable(&agent.join("bin/dar.new"), "new");

        let err = doctor_gate(&agent, &FakeDoctor { ok: false }).unwrap_err();

        assert!(err.to_string().contains("doctor gate failed"));
        assert_eq!(fs::read_to_string(agent.join("bin/dar")).unwrap(), "old");
        assert!(!agent.join("bin/dar.new").exists());
        assert!(!agent.join("bin/dar.prev").exists());
    }

    #[test]
    fn atomic_rename_preserves_prev() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path().to_path_buf();
        write_executable(&agent.join("bin/dar"), "old");
        write_executable(&agent.join("bin/dar.new"), "new");

        atomic_swap(&agent).unwrap();

        assert_eq!(fs::read_to_string(agent.join("bin/dar")).unwrap(), "new");
        assert_eq!(
            fs::read_to_string(agent.join("bin/dar.prev")).unwrap(),
            "old"
        );
    }

    #[test]
    fn atomic_rename_restores_current_when_new_binary_is_missing() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path().to_path_buf();
        write_executable(&agent.join("bin/dar"), "old");
        write_executable(&agent.join("bin/dar.prev"), "older");

        let err = atomic_swap(&agent).unwrap_err();

        assert!(err.to_string().contains("renaming"));
        assert_eq!(fs::read_to_string(agent.join("bin/dar")).unwrap(), "old");
    }

    #[test]
    fn lock_serializes_concurrent_updates() {
        let _ = RestartMode::Skip;
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path().to_path_buf();
        let events = Arc::new(Mutex::new(Vec::new()));
        let first_events = Arc::clone(&events);
        let second_events = Arc::clone(&events);

        let first = thread::spawn(move || {
            with_lock(&agent, || {
                first_events
                    .lock()
                    .unwrap()
                    .push(("first-start", Instant::now()));
                thread::sleep(Duration::from_millis(150));
                first_events
                    .lock()
                    .unwrap()
                    .push(("first-end", Instant::now()));
                Ok(())
            })
            .unwrap();
        });
        thread::sleep(Duration::from_millis(25));
        let agent = temp.path().to_path_buf();
        let second = thread::spawn(move || {
            with_lock(&agent, || {
                second_events
                    .lock()
                    .unwrap()
                    .push(("second-start", Instant::now()));
                Ok(())
            })
            .unwrap();
        });

        first.join().unwrap();
        second.join().unwrap();

        let events = events.lock().unwrap();
        let first_end = events
            .iter()
            .find(|(name, _)| *name == "first-end")
            .unwrap()
            .1;
        let second_start = events
            .iter()
            .find(|(name, _)| *name == "second-start")
            .unwrap()
            .1;
        assert!(second_start >= first_end);
    }

    #[test]
    fn self_rebuild_rejects_non_runnable_target() {
        let target = if cfg!(target_arch = "aarch64") {
            "x86_64-unknown-linux-musl"
        } else {
            "aarch64-unknown-linux-musl"
        };
        let err = ensure_self_rebuild_output_runnable(&composer::BuildOptions {
            target: Some(target.to_string()),
            ..composer::BuildOptions::default()
        })
        .unwrap_err();

        assert!(
            err.to_string().contains("is not runnable on this host"),
            "{err:#}"
        );
    }

    #[test]
    fn self_rebuild_allows_host_target() {
        let target = match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") => "x86_64-unknown-linux-musl",
            ("linux", "aarch64") => "aarch64-unknown-linux-musl",
            ("macos", "x86_64") => "x86_64-apple-darwin",
            ("macos", "aarch64") => "aarch64-apple-darwin",
            _ => return,
        };

        ensure_self_rebuild_output_runnable(&composer::BuildOptions {
            target: Some(target.to_string()),
            ..composer::BuildOptions::default()
        })
        .unwrap();
    }

    struct FakeDoctor {
        ok: bool,
    }

    impl DoctorRunner for FakeDoctor {
        fn run(&self, _new_binary: &Path, _agent: &Path) -> Result<()> {
            if self.ok {
                Ok(())
            } else {
                anyhow::bail!("doctor gate failed")
            }
        }
    }

    fn write_executable(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}
