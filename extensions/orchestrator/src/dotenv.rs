//! Minimal `.env` loader for agent-folder scoped runtime configuration.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{bail, Context, Result};

/// Children spawned via `runner-core` must never inherit `.env`-loaded keys;
/// the scrub registry lives there so backend extension crates share it.
pub use runner_core::scrub_loaded_env;

/// Keys this process has loaded from `.env`. A key lands here the first time it
/// is copied from the file into the process env (initial load). Reloads then
/// override *only* these keys, never genuine process-env values that merely
/// happen to share a name with a `.env` entry.
fn file_loaded_keys() -> &'static Mutex<std::collections::HashMap<PathBuf, HashSet<String>>> {
    static KEYS: OnceLock<Mutex<std::collections::HashMap<PathBuf, HashSet<String>>>> =
        OnceLock::new();
    KEYS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn record_file_loaded_key(root: &Path, key: &str) {
    file_loaded_keys()
        .lock()
        .expect("file-loaded env key registry poisoned")
        .entry(root.to_path_buf())
        .or_default()
        .insert(key.to_string());
}

fn is_file_loaded_key(root: &Path, key: &str) -> bool {
    file_loaded_keys()
        .lock()
        .expect("file-loaded env key registry poisoned")
        .get(root)
        .is_some_and(|keys| keys.contains(key))
}

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

/// Outcome of an on-demand secret reload (`reload_agent_env`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReloadReport {
    pub path: PathBuf,
    pub found: bool,
    /// Keys re-read from `.env` and written back into the process env because
    /// they were originally loaded from the file (values may be unchanged).
    pub reloaded: Vec<String>,
    /// Keys present in `.env` that were left untouched because a genuine
    /// process-env value (not from `.env`) owns them.
    pub skipped_external: Vec<String>,
    /// Formerly file-owned keys removed from the process because they are absent
    /// from the replacement (including a deleted `.env`).
    pub removed: Vec<String>,
}

impl ReloadReport {
    pub fn absent(path: PathBuf) -> Self {
        Self {
            path,
            found: false,
            reloaded: Vec::new(),
            skipped_external: Vec::new(),
            removed: Vec::new(),
        }
    }
}

/// Parsed-content watcher for the agent-root `.env`. It deliberately compares
/// parsed content, not mtimes, and samples twice before accepting a replacement.
pub struct EnvReloader {
    root: PathBuf,
    fingerprint: u64,
}

impl EnvReloader {
    pub fn new(root: &Path) -> Self {
        let fingerprint = read_stable(root)
            .map(|v| fingerprint(&v))
            .unwrap_or_else(|_| fingerprint(&BTreeMap::new()));
        Self {
            root: root.to_path_buf(),
            fingerprint,
        }
    }
    pub fn maybe_reload(&mut self) -> Result<Option<ReloadReport>> {
        let values = read_stable(&self.root)?;
        let next = fingerprint(&values);
        if next == self.fingerprint {
            return Ok(None);
        }
        let report = reload_agent_env_values(&self.root, values)?;
        self.fingerprint = next;
        Ok(Some(report))
    }
}

fn fingerprint(values: &BTreeMap<String, String>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    values.hash(&mut h);
    h.finish()
}

fn read_stable(root: &Path) -> Result<BTreeMap<String, String>> {
    let path = root.join(".env");
    let read = || -> Result<BTreeMap<String, String>> {
        let Ok(contents) = std::fs::read_to_string(&path) else {
            if !path.exists() {
                return Ok(BTreeMap::new());
            }
            bail!("unable to read agent environment file");
        };
        parse_contents(&contents)
    };
    let first = read()?;
    let second = read()?;
    if first != second {
        bail!("agent environment file changed while being read");
    }
    Ok(first)
}

fn parse_contents(contents: &str) -> Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    for line in contents.lines() {
        if let Some((key, value)) = parse_line(line)? {
            values.insert(key, value);
        }
    }
    Ok(values)
}

/// Current valid agent-file values for bridge redaction. Callers must retain
/// prior values themselves when a cache could still expose them.
pub fn loaded_agent_env_values(root: &Path) -> Vec<String> {
    file_loaded_keys()
        .lock()
        .expect("file-loaded env key registry poisoned")
        .get(root)
        .into_iter()
        .flatten()
        .filter_map(|key| std::env::var(key).ok())
        .collect()
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
            runner_core::register_scrubbed_env_key(key.clone());
            record_file_loaded_key(root, &key);
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

/// Re-read `<root>/.env` on demand and override **only** the keys this process
/// originally loaded from `.env`. Keys that a genuine process-env value owns
/// (never loaded from the file) are left untouched, mirroring the
/// "don't clobber real env" rule of [`load_agent_env`] in reverse.
///
/// New keys that appear in `.env` since the initial load and are not already
/// set in the process env are also loaded (and tracked), so a freshly-added
/// secret is picked up too. Newly-tracked keys are registered for child
/// scrubbing exactly like the initial load.
pub fn reload_agent_env(root: &Path) -> Result<ReloadReport> {
    let values = read_stable(root)?;
    reload_agent_env_values(root, values)
}

fn reload_agent_env_values(root: &Path, values: BTreeMap<String, String>) -> Result<ReloadReport> {
    let path = root.join(".env");
    let mut reloaded = Vec::new();
    let mut skipped_external = Vec::new();
    let mut removed = Vec::new();
    let owned: Vec<_> = file_loaded_keys()
        .lock()
        .expect("file-loaded env key registry poisoned")
        .get(root)
        .into_iter()
        .flatten()
        .cloned()
        .collect();
    for key in owned {
        if !values.contains_key(&key) {
            std::env::remove_var(&key);
            removed.push(key);
        }
    }
    for (key, value) in values {
        if is_file_loaded_key(root, &key) {
            // We own this key — always refresh it from the file.
            std::env::set_var(&key, value);
            reloaded.push(key);
        } else if std::env::var_os(&key).is_some() {
            // A genuine process-env value owns this key; never clobber it.
            skipped_external.push(key);
        } else {
            // A key added to `.env` after initial load and not set elsewhere.
            std::env::set_var(&key, value);
            runner_core::register_scrubbed_env_key(key.clone());
            record_file_loaded_key(root, &key);
            reloaded.push(key);
        }
    }

    Ok(ReloadReport {
        path,
        found: root.join(".env").exists(),
        reloaded,
        skipped_external,
        removed,
    })
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
        bail!("invalid environment key");
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

    use super::{load_agent_env, parse_line, reload_agent_env, scrub_loaded_env, EnvReloader};

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
    fn reload_overrides_only_file_loaded_keys() {
        let _guard = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let env_path = dir.path().join(".env");

        // Genuine process env owns RELOAD_EXTERNAL; RELOAD_FILE comes from .env.
        std::env::set_var("RELOAD_EXTERNAL", "real");
        std::env::remove_var("RELOAD_FILE");
        std::fs::write(&env_path, "RELOAD_EXTERNAL=fromfile\nRELOAD_FILE=old\n").unwrap();

        let load = load_agent_env(dir.path()).unwrap();
        // Initial load respects the existing external value, loads the new one.
        assert_eq!(load.loaded, vec!["RELOAD_FILE"]);
        assert_eq!(load.skipped_existing, vec!["RELOAD_EXTERNAL"]);
        assert_eq!(std::env::var("RELOAD_EXTERNAL").unwrap(), "real");
        assert_eq!(std::env::var("RELOAD_FILE").unwrap(), "old");

        // Rotate both values in the file, then reload.
        std::fs::write(&env_path, "RELOAD_EXTERNAL=fromfile2\nRELOAD_FILE=new\n").unwrap();
        let report = reload_agent_env(dir.path()).unwrap();

        assert!(report.found);
        // Only the file-loaded key is overridden; the external one is skipped.
        assert_eq!(report.reloaded, vec!["RELOAD_FILE"]);
        assert_eq!(report.skipped_external, vec!["RELOAD_EXTERNAL"]);
        assert_eq!(std::env::var("RELOAD_FILE").unwrap(), "new");
        assert_eq!(std::env::var("RELOAD_EXTERNAL").unwrap(), "real");

        std::env::remove_var("RELOAD_EXTERNAL");
        std::env::remove_var("RELOAD_FILE");
    }

    #[test]
    fn reload_picks_up_newly_added_key() {
        let _guard = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let env_path = dir.path().join(".env");
        std::env::remove_var("RELOAD_ADDED");
        std::fs::write(&env_path, "# empty\n").unwrap();
        load_agent_env(dir.path()).unwrap();

        std::fs::write(&env_path, "RELOAD_ADDED=fresh\n").unwrap();
        let report = reload_agent_env(dir.path()).unwrap();
        assert_eq!(report.reloaded, vec!["RELOAD_ADDED"]);
        assert_eq!(std::env::var("RELOAD_ADDED").unwrap(), "fresh");

        // A newly-tracked key must also be scrubbed from child spawns.
        let mut command = std::process::Command::new("env");
        scrub_loaded_env(&mut command);
        let removed = command
            .get_envs()
            .any(|(key, value)| key == "RELOAD_ADDED" && value.is_none());
        assert!(removed);

        std::env::remove_var("RELOAD_ADDED");
    }

    #[test]
    fn reload_absent_env_is_noop() {
        let _guard = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let report = reload_agent_env(dir.path()).unwrap();
        assert!(!report.found);
        assert!(report.reloaded.is_empty());
        assert!(report.skipped_external.is_empty());
    }

    #[test]
    fn reload_removes_file_owned_keys_when_file_is_deleted() {
        let _guard = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::env::remove_var("DOTENV_REMOVED");
        std::fs::write(&path, "DOTENV_REMOVED=present\n").unwrap();
        load_agent_env(dir.path()).unwrap();
        std::fs::remove_file(path).unwrap();

        let report = reload_agent_env(dir.path()).unwrap();
        assert!(!report.found);
        assert!(report.removed.contains(&"DOTENV_REMOVED".to_string()));
        assert!(std::env::var_os("DOTENV_REMOVED").is_none());
    }

    #[test]
    fn watcher_applies_each_parsed_change_once() {
        let _guard = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::env::remove_var("DOTENV_WATCHED");
        std::fs::write(&path, "DOTENV_WATCHED=one\n").unwrap();
        load_agent_env(dir.path()).unwrap();
        let mut watcher = EnvReloader::new(dir.path());
        assert!(watcher.maybe_reload().unwrap().is_none());
        std::fs::write(&path, "DOTENV_WATCHED=two\n").unwrap();
        assert!(watcher.maybe_reload().unwrap().is_some());
        assert!(watcher.maybe_reload().unwrap().is_none());
        assert_eq!(std::env::var("DOTENV_WATCHED").unwrap(), "two");
        std::env::remove_var("DOTENV_WATCHED");
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
