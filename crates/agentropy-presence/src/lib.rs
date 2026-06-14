//! Presence registry for agent dashboards.
//!
//! A pure read/write module over a registry *directory*. Each live agent
//! dashboard writes exactly one JSON file describing how to reach it; the
//! `agentropy dash` aggregator reads the directory, prunes entries whose
//! process is gone, and presents the survivors.
//!
//! The module performs no I/O beyond the registry directory and holds no
//! background state, so it is fully testable without a running agent: seed a
//! `tempdir`, write/read, assert.
//!
//! ## File layout
//!
//! One file per agent, named `<id-slug>-<folder-hash>.json`. Keying on both id
//! and a hash of the folder means one operator can run the same agent id from
//! two different folders without the presence files colliding.
//!
//! ## Liveness
//!
//! Entries carry the writing process's `pid`. On read, an entry is kept only if
//! its pid is still alive (`kill(pid, 0)` on Unix). Crashed agents therefore
//! drop off the list automatically; cleanly shut-down agents additionally
//! `unlink` their own file so nothing lingers even briefly.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Default registry directory: `~/.agentropy/dashboards/`.
///
/// Resolves `~` from `$HOME`. Falls back to a relative `.agentropy/dashboards`
/// only when `$HOME` is unset (e.g. odd CI), which keeps the function
/// infallible for callers that just want a sensible default.
pub fn default_registry_dir() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => {
            Path::new(&home).join(".agentropy").join("dashboards")
        }
        _ => PathBuf::from(".agentropy").join("dashboards"),
    }
}

/// One agent dashboard's presence record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceEntry {
    /// Stable agent id (from `agent.yaml`).
    pub id: String,
    /// Absolute agent folder the dashboard is serving.
    pub folder: String,
    /// Host:port the dashboard's HTTP server bound, as seen on the LAN/tailnet
    /// (e.g. `0.0.0.0:53124`). The aggregator substitutes the host portion with
    /// the browser's request host so the iframe resolves from the client side.
    pub addr: String,
    /// PID of the agent process owning this dashboard. Used for liveness.
    pub pid: u32,
    /// Unix-epoch seconds the dashboard booted.
    pub started_at: i64,
}

impl PresenceEntry {
    /// Filename (no directory) this entry is stored under: `<id>-<hash>.json`.
    /// `id` is slugified to keep the name filesystem-safe; the folder hash
    /// disambiguates same-id agents in different folders.
    pub fn file_name(&self) -> String {
        file_name_for(&self.id, &self.folder)
    }

    /// The port portion of [`PresenceEntry::addr`], if parseable.
    pub fn port(&self) -> Option<u16> {
        self.addr.rsplit(':').next().and_then(|p| p.parse().ok())
    }
}

/// Compute the storage filename for an id + folder pair.
pub fn file_name_for(id: &str, folder: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(folder.as_bytes());
    let digest = hasher.finalize();
    let hash = hex::encode(&digest[..6]);
    format!("{}-{}.json", slugify(id), hash)
}

fn slugify(id: &str) -> String {
    let slug: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if slug.is_empty() {
        "agent".to_string()
    } else {
        slug
    }
}

/// A handle over a registry directory. Cheap to construct; holds no state.
#[derive(Debug, Clone)]
pub struct Registry {
    dir: PathBuf,
}

impl Registry {
    /// Open (do not create) a registry at `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Open the default registry directory.
    pub fn default_dir() -> Self {
        Self::new(default_registry_dir())
    }

    /// The registry directory path.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Write (create or overwrite) this entry's presence file, creating the
    /// registry directory if needed. Write is atomic via a temp file + rename
    /// so a concurrent reader never observes a half-written file.
    pub fn write(&self, entry: &PresenceEntry) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating registry dir {}", self.dir.display()))?;
        let path = self.dir.join(entry.file_name());
        let tmp = self.dir.join(format!(".{}.tmp", entry.file_name()));
        let json = serde_json::to_vec_pretty(entry).context("serializing presence entry")?;
        std::fs::write(&tmp, &json)
            .with_context(|| format!("writing presence temp {}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("renaming presence file into {}", path.display()))?;
        Ok(path)
    }

    /// Remove this entry's presence file. Missing file is not an error
    /// (clean-shutdown unlink is idempotent across restarts).
    pub fn remove(&self, id: &str, folder: &str) -> Result<()> {
        let path = self.dir.join(file_name_for(id, folder));
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing presence file {}", path.display())),
        }
    }

    /// Read all *live* entries: every well-formed presence file whose pid is
    /// still alive. Dead-pid files are deleted as a side effect (self-healing
    /// the directory); malformed/partial files are skipped without error.
    pub fn read_live(&self) -> Vec<PresenceEntry> {
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            // Missing directory => no live agents.
            Err(_) => return out,
        };
        for dirent in entries.flatten() {
            let path = dirent.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let entry: PresenceEntry = match serde_json::from_slice(&bytes) {
                Ok(e) => e,
                // Malformed / partial file: skip, leave for the writer to fix.
                Err(_) => continue,
            };
            if pid_alive(entry.pid) {
                out.push(entry);
            } else {
                // Crashed without unlinking: prune the stale file.
                let _ = std::fs::remove_file(&path);
            }
        }
        // Order of appearance: oldest started_at first; id/folder break ties
        // (started_at is second-granular, so same-second starts stay stable).
        out.sort_by(|a, b| {
            a.started_at
                .cmp(&b.started_at)
                .then(a.id.cmp(&b.id))
                .then(a.folder.cmp(&b.folder))
        });
        out
    }
}

/// Whether a process with `pid` currently exists.
///
/// On Unix this is `kill(pid, 0)`: returns true when the signal *could* be
/// sent (process exists), including when it exists but we lack permission
/// (`EPERM`). On non-Unix targets we conservatively report `true` so we never
/// drop a live agent on platforms without a cheap liveness probe.
pub fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        if pid == 0 {
            return false;
        }
        let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if ret == 0 {
            return true;
        }
        // EPERM => process exists but not signalable by us.
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, folder: &str, pid: u32) -> PresenceEntry {
        PresenceEntry {
            id: id.to_string(),
            folder: folder.to_string(),
            addr: "0.0.0.0:50000".to_string(),
            pid,
            started_at: 1_700_000_000,
        }
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let reg = Registry::new(dir.path());
        let e = entry("ALG-1", "/agents/a", std::process::id());
        reg.write(&e).unwrap();
        let live = reg.read_live();
        assert_eq!(live, vec![e]);
    }

    #[test]
    fn dead_pid_is_pruned_on_read() {
        let dir = tempfile::tempdir().unwrap();
        let reg = Registry::new(dir.path());
        // PID 999999 is exceedingly unlikely to exist.
        let dead = entry("ALG-2", "/agents/b", 999_999);
        let path = reg.write(&dead).unwrap();
        assert!(path.exists());
        let live = reg.read_live();
        assert!(live.is_empty());
        // Side effect: the stale file is removed.
        assert!(!path.exists());
    }

    #[test]
    fn clean_shutdown_unlink_removes_entry() {
        let dir = tempfile::tempdir().unwrap();
        let reg = Registry::new(dir.path());
        let e = entry("ALG-3", "/agents/c", std::process::id());
        reg.write(&e).unwrap();
        reg.remove(&e.id, &e.folder).unwrap();
        assert!(reg.read_live().is_empty());
        // Idempotent: removing again is not an error.
        reg.remove(&e.id, &e.folder).unwrap();
    }

    #[test]
    fn same_id_different_folder_does_not_collide() {
        let dir = tempfile::tempdir().unwrap();
        let reg = Registry::new(dir.path());
        let pid = std::process::id();
        let a = entry("ALG-9", "/agents/one", pid);
        let b = entry("ALG-9", "/agents/two", pid);
        assert_ne!(a.file_name(), b.file_name());
        reg.write(&a).unwrap();
        reg.write(&b).unwrap();
        let live = reg.read_live();
        assert_eq!(live.len(), 2);
        assert!(live.contains(&a));
        assert!(live.contains(&b));
    }

    #[test]
    fn malformed_file_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let reg = Registry::new(dir.path());
        std::fs::write(dir.path().join("garbage.json"), b"{not json").unwrap();
        std::fs::write(dir.path().join("partial.json"), b"{\"id\":\"x\"").unwrap();
        let good = entry("ALG-4", "/agents/d", std::process::id());
        reg.write(&good).unwrap();
        let live = reg.read_live();
        assert_eq!(live, vec![good]);
    }

    #[test]
    fn non_json_files_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let reg = Registry::new(dir.path());
        std::fs::write(dir.path().join("README.txt"), b"hello").unwrap();
        assert!(reg.read_live().is_empty());
    }

    #[test]
    fn missing_dir_reads_empty() {
        let reg = Registry::new("/nonexistent/agentropy/registry/xyz");
        assert!(reg.read_live().is_empty());
    }

    #[test]
    fn current_process_is_alive() {
        assert!(pid_alive(std::process::id()));
    }

    #[test]
    fn pid_zero_is_not_alive() {
        assert!(!pid_alive(0));
    }

    #[test]
    fn port_parsed_from_addr() {
        let e = entry("ALG-5", "/agents/e", 1);
        assert_eq!(e.port(), Some(50000));
    }

    #[test]
    fn overwrite_keeps_single_file() {
        let dir = tempfile::tempdir().unwrap();
        let reg = Registry::new(dir.path());
        let pid = std::process::id();
        let mut e = entry("ALG-6", "/agents/f", pid);
        reg.write(&e).unwrap();
        e.addr = "0.0.0.0:60001".to_string();
        reg.write(&e).unwrap();
        let live = reg.read_live();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].addr, "0.0.0.0:60001");
    }
}
