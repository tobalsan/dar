//! Boot-time crashloop safety net.
//!
//! The doctor gate ([`crate::self_update`]) is the primary defense against a
//! self-update producing a binary that cannot boot. This module is the
//! belt-and-suspenders second layer described in the FSC PRD ("The Self-Update
//! Loop" step 7, story 9):
//!
//!   * `--self-check` validates that the binary can boot — it parses the agent
//!     config and instantiates every extension — then exits `0`/non-zero.
//!   * At boot, before the long-running `run` loop starts, the binary spawns
//!     itself with `--self-check`. If that child exits non-zero and a
//!     `bin/agentropy.prev` sits next to the running binary, `main()` `execv`s
//!     into `.prev` so a binary that passed the doctor gate but still fails to
//!     boot rolls back to the last-known-good binary instead of crashlooping.
//!   * With no `.prev` present, boot fails with a clear, actionable error
//!     rather than silently looping.

use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, Result};
use host_api::Extension;

/// Env guard set before `execv`-ing into `bin/agentropy.prev`. The rolled-back
/// binary sees it and skips the boot guard, so a `.prev` that is also broken
/// fails loudly instead of looping back into itself.
const FALLBACK_GUARD_ENV: &str = "AGENTROPY_SELF_CHECK_FALLBACK";

/// Detect the `--self-check` flag in the raw argument list and resolve the agent
/// root from a sibling `--dir <path>` (defaulting to the current directory).
/// Returns `Ok(None)` when `--self-check` is absent, leaving normal CLI dispatch
/// untouched.
pub fn extract_flag<I, S>(args: I) -> Result<Option<PathBuf>>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    if !args.iter().any(|a| a == "--self-check") {
        return Ok(None);
    }
    let mut dir: Option<PathBuf> = None;
    let mut iter = args.iter().skip(1);
    while let Some(arg) = iter.next() {
        if arg == "--dir" {
            dir = iter.next().map(PathBuf::from);
        } else if let Some(value) = arg.to_str().and_then(|s| s.strip_prefix("--dir=")) {
            dir = Some(PathBuf::from(value));
        }
    }
    let raw = match dir {
        Some(p) => p,
        None => std::env::current_dir().context("resolving current directory")?,
    };
    let root = raw
        .canonicalize()
        .with_context(|| format!("resolving agent folder {}", raw.display()))?;
    Ok(Some(root))
}

/// Validate that this binary can boot: parse `agent.yaml` and instantiate every
/// extension. Returns `Ok(0)` when healthy, `Ok(1)` when a check fails.
pub async fn run(root: &Path, plugins: Vec<Arc<dyn Extension>>) -> Result<i32> {
    match check(root, plugins).await {
        Ok(()) => {
            eprintln!("self-check: ok");
            Ok(0)
        }
        Err(e) => {
            eprintln!("self-check: FAIL {e:#}");
            Ok(1)
        }
    }
}

async fn check(root: &Path, plugins: Vec<Arc<dyn Extension>>) -> Result<()> {
    orchestrator::config::load(root)
        .context("loading agent.yaml")?
        .validate()
        .context("validating agent.yaml")?;
    crate::plugin_services(root, plugins)
        .await
        .context("instantiating extensions")?;
    Ok(())
}

/// Boot-time crashloop guard. Spawns `current_exe --self-check --dir <root>`;
/// when that child exits non-zero, `execv`s into `bin/agentropy.prev` if it
/// exists, otherwise returns an actionable error. A healthy self-check returns
/// `Ok(())` and boot continues with no extra `execv` hop.
pub fn guard_boot(root: &Path) -> Result<()> {
    if std::env::var_os(FALLBACK_GUARD_ENV).is_some() {
        // We are the rolled-back `.prev` binary (or an operator forced the
        // guard off). Do not re-run the guard; boot directly so a broken
        // `.prev` surfaces its own failure instead of looping.
        return Ok(());
    }

    let exe = std::env::current_exe().context("resolving current executable")?;
    let status = Command::new(&exe)
        .arg("--self-check")
        .arg("--dir")
        .arg(root)
        .status()
        .with_context(|| format!("spawning self-check {}", exe.display()))?;

    if status.success() {
        return Ok(());
    }

    match prev_binary(&exe) {
        Some(prev) => exec_prev(&prev),
        None => Err(anyhow::anyhow!(
            "self-check failed (exit {}) and no rollback binary exists at {}; \
             refusing to crashloop. Rebuild with `agentropy build --dir {}` \
             or restore a known-good binary.",
            status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string()),
            prev_path(&exe).display(),
            root.display(),
        )),
    }
}

/// Path to the rollback binary that sits next to `exe` (`bin/agentropy.prev`).
fn prev_path(exe: &Path) -> PathBuf {
    let dir = exe.parent().unwrap_or_else(|| Path::new("."));
    let name = exe
        .file_name()
        .map(|n| {
            let mut name = n.to_os_string();
            name.push(".prev");
            name
        })
        .unwrap_or_else(|| OsString::from("agentropy.prev"));
    dir.join(name)
}

fn prev_binary(exe: &Path) -> Option<PathBuf> {
    let prev = prev_path(exe);
    prev.is_file().then_some(prev)
}

/// `execv` into the rollback binary, preserving the original arguments and
/// marking the guard env so the rolled-back binary does not loop.
fn exec_prev(prev: &Path) -> Result<()> {
    eprintln!(
        "self-check failed; rolling back to {} via execv",
        prev.display()
    );
    let args = std::env::args_os().skip(1).collect::<Vec<OsString>>();
    let err = Command::new(prev)
        .args(args)
        .env(FALLBACK_GUARD_ENV, "1")
        .exec();
    Err(err).with_context(|| format!("execv failed for {}", prev.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn write_executable(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn prev_path_appends_prev_suffix() {
        let exe = Path::new("/agent/bin/agentropy");
        assert_eq!(prev_path(exe), Path::new("/agent/bin/agentropy.prev"));
    }

    #[test]
    fn prev_binary_found_only_when_file_present() {
        let temp = tempfile::tempdir().unwrap();
        let exe = temp.path().join("bin/agentropy");
        write_executable(&exe, "current");
        assert!(prev_binary(&exe).is_none());
        write_executable(&temp.path().join("bin/agentropy.prev"), "prev");
        assert_eq!(
            prev_binary(&exe),
            Some(temp.path().join("bin/agentropy.prev"))
        );
    }
}
