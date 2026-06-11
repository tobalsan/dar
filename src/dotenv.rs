//! Minimal `.env` loader for agent-folder scoped runtime configuration.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{bail, Context, Result};

static FILE_LOADED_KEYS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadReport {
    pub path: PathBuf,
    pub found: bool,
    pub loaded: Vec<String>,
    pub skipped_existing: Vec<String>,
}

impl LoadReport {
    pub fn absent(path: PathBuf) -> Self {
        Self {
            path,
            found: false,
            loaded: Vec::new(),
            skipped_existing: Vec::new(),
        }
    }
}

/// Load `<root>/.env` into the current process without overriding existing env.
pub fn load_agent_env(root: &Path) -> Result<LoadReport> {
    let path = root.join(".env");
    if !path.exists() {
        return Ok(LoadReport::absent(path));
    }

    let contents =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let mut loaded = Vec::new();
    let mut skipped_existing = Vec::new();

    for (idx, line) in contents.lines().enumerate() {
        let Some((key, value)) = parse_line(line)
            .with_context(|| format!("parsing {} line {}", path.display(), idx + 1))?
        else {
            continue;
        };
        if std::env::var_os(&key).is_some() {
            skipped_existing.push(key);
        } else {
            std::env::set_var(&key, value);
            loaded_key_set().lock().unwrap().insert(key.clone());
            runner_core::record_loaded_env_key(key.clone());
            loaded.push(key);
        }
    }

    Ok(LoadReport {
        path,
        found: true,
        loaded,
        skipped_existing,
    })
}

/// Remove env vars that came from `.env` from a child command environment.
pub fn scrub_loaded_env<C>(cmd: &mut C)
where
    C: EnvRemove,
{
    let Some(keys) = FILE_LOADED_KEYS.get() else {
        return;
    };
    for key in keys.lock().unwrap().iter() {
        cmd.env_remove(key);
    }
}

pub trait EnvRemove {
    fn env_remove<K: AsRef<std::ffi::OsStr>>(&mut self, key: K) -> &mut Self;
}

impl EnvRemove for std::process::Command {
    fn env_remove<K: AsRef<std::ffi::OsStr>>(&mut self, key: K) -> &mut Self {
        std::process::Command::env_remove(self, key)
    }
}

impl EnvRemove for tokio::process::Command {
    fn env_remove<K: AsRef<std::ffi::OsStr>>(&mut self, key: K) -> &mut Self {
        tokio::process::Command::env_remove(self, key)
    }
}

fn loaded_key_set() -> &'static Mutex<HashSet<String>> {
    FILE_LOADED_KEYS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn parse_line(line: &str) -> Result<Option<(String, String)>> {
    let mut s = line.trim();
    if s.is_empty() || s.starts_with('#') {
        return Ok(None);
    }
    if let Some(rest) = s.strip_prefix("export ") {
        s = rest.trim_start();
    }

    let Some(eq) = s.find('=') else {
        bail!("expected KEY=VALUE");
    };
    let key = s[..eq].trim();
    if key.is_empty()
        || !key.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
        || key.as_bytes()[0].is_ascii_digit()
    {
        bail!("invalid env key {key:?}");
    }

    let value = parse_value(s[eq + 1..].trim_start())?;
    Ok(Some((key.to_string(), value)))
}

fn parse_value(raw: &str) -> Result<String> {
    if let Some(rest) = raw.strip_prefix('"') {
        let mut out = String::new();
        let mut escaped = false;
        for (idx, c) in rest.char_indices() {
            if escaped {
                match c {
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    '\\' => out.push('\\'),
                    '"' => out.push('"'),
                    other => out.push(other),
                }
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                return Ok(out);
            } else {
                out.push(c);
            }
            if idx == rest.len() - 1 && escaped {
                out.push('\\');
            }
        }
        bail!("unterminated double-quoted value");
    }

    if let Some(rest) = raw.strip_prefix('\'') {
        if let Some(end) = rest.find('\'') {
            return Ok(rest[..end].to_string());
        }
        bail!("unterminated single-quoted value");
    }

    let value = match raw.find('#') {
        Some(idx) => &raw[..idx],
        None => raw,
    };
    Ok(value.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, OnceLock};

    use super::{load_agent_env, parse_line, scrub_loaded_env};

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    #[test]
    fn parses_common_dotenv_lines() {
        assert_eq!(
            parse_line("export FOO=\"bar baz\"").unwrap(),
            Some(("FOO".into(), "bar baz".into()))
        );
        assert_eq!(
            parse_line("A=one # comment").unwrap(),
            Some(("A".into(), "one".into()))
        );
        assert_eq!(
            parse_line("B='two # kept'").unwrap(),
            Some(("B".into(), "two # kept".into()))
        );
        assert_eq!(parse_line("# ignored").unwrap(), None);
    }

    #[test]
    fn load_agent_env_does_not_override_existing_env() {
        let _guard = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".env"),
            "DOTENV_EXISTING=file\nDOTENV_NEW=fresh\n",
        )
        .unwrap();
        std::env::set_var("DOTENV_EXISTING", "real");
        std::env::remove_var("DOTENV_NEW");

        let report = load_agent_env(dir.path()).unwrap();

        assert!(report.found);
        assert_eq!(std::env::var("DOTENV_EXISTING").unwrap(), "real");
        assert_eq!(std::env::var("DOTENV_NEW").unwrap(), "fresh");
        assert_eq!(report.loaded, vec!["DOTENV_NEW"]);
        assert_eq!(report.skipped_existing, vec!["DOTENV_EXISTING"]);

        std::env::remove_var("DOTENV_EXISTING");
        std::env::remove_var("DOTENV_NEW");
    }

    #[test]
    fn scrub_loaded_env_removes_only_file_loaded_keys_from_child() {
        let _guard = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "DOTENV_SCRUB=file\n").unwrap();
        std::env::remove_var("DOTENV_SCRUB");

        load_agent_env(dir.path()).unwrap();
        let mut command = std::process::Command::new("env");
        scrub_loaded_env(&mut command);

        let removed = command
            .get_envs()
            .any(|(key, value)| key == "DOTENV_SCRUB" && value.is_none());
        assert!(removed);

        std::env::remove_var("DOTENV_SCRUB");
    }
}
