//! Mtime-based reload detection for `<root>/agent.yaml`.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;

use crate::config::{self, AgentConfig};

pub struct AgentConfigReloader {
    root: PathBuf,
    last_mtime: Option<SystemTime>,
}

impl AgentConfigReloader {
    pub fn new(root: &Path) -> Self {
        let path = root.join("agent.yaml");
        let last_mtime = std::fs::metadata(path)
            .ok()
            .and_then(|meta| meta.modified().ok());
        Self {
            root: root.to_path_buf(),
            last_mtime,
        }
    }

    pub fn maybe_reload(&mut self) -> Result<Option<AgentConfig>> {
        let path = self.root.join("agent.yaml");
        let meta = match std::fs::metadata(&path) {
            Ok(meta) => meta,
            Err(e) => {
                crate::logging::ev(
                    "-",
                    "agent_config_reload",
                    &format!("metadata error (stale config kept): {e:#}"),
                );
                return Ok(None);
            }
        };
        let mtime = meta.modified().ok();
        if mtime == self.last_mtime {
            return Ok(None);
        }

        match config::load(&self.root).and_then(|cfg| {
            cfg.validate()?;
            Ok(cfg)
        }) {
            Ok(cfg) => {
                self.last_mtime = mtime;
                crate::logging::ev(
                    "-",
                    "agent_config_reload",
                    "agent.yaml reloaded successfully",
                );
                Ok(Some(cfg))
            }
            Err(e) => {
                self.last_mtime = mtime;
                crate::logging::ev(
                    "-",
                    "agent_config_reload",
                    &format!("reload error (stale config kept): {e:#}"),
                );
                Ok(None)
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn mark_stale_for_test(&mut self) {
        self.last_mtime = Some(SystemTime::UNIX_EPOCH);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn write_valid_agent_yaml(root: &Path, max_run_timeout_ms: u64) {
        std::fs::write(
            root.join("agent.yaml"),
            format!(
                "\
id: test-agent
name: Test Agent
runner:
  use: fake
  command: fake
  max_run_timeout_ms: {max_run_timeout_ms}
  stall_timeout_ms: 300000
  max_turns: 20
"
            ),
        )
        .unwrap();
    }

    #[test]
    fn unchanged_agent_yaml_is_noop() {
        let temp = tempfile::tempdir().unwrap();
        write_valid_agent_yaml(temp.path(), 1000);
        let mut reloader = AgentConfigReloader::new(temp.path());

        let cfg = reloader.maybe_reload().unwrap();

        assert!(cfg.is_none());
    }

    #[test]
    fn changed_agent_yaml_is_reloaded() {
        let temp = tempfile::tempdir().unwrap();
        write_valid_agent_yaml(temp.path(), 1000);
        let mut reloader = AgentConfigReloader::new(temp.path());
        write_valid_agent_yaml(temp.path(), 2500);
        reloader.last_mtime = Some(SystemTime::UNIX_EPOCH);

        let cfg = reloader.maybe_reload().unwrap().unwrap();

        assert_eq!(cfg.runner.max_run_timeout_ms, 2500);
    }

    #[test]
    fn malformed_agent_yaml_returns_none() {
        let temp = tempfile::tempdir().unwrap();
        write_valid_agent_yaml(temp.path(), 1000);
        let mut reloader = AgentConfigReloader::new(temp.path());
        std::fs::write(temp.path().join("agent.yaml"), "runner: [").unwrap();
        reloader.last_mtime = Some(SystemTime::UNIX_EPOCH);

        let cfg = reloader.maybe_reload().unwrap();

        assert!(cfg.is_none());
    }
}
