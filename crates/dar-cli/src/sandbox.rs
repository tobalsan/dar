//! `dar sandbox`: scaffold Docker sandbox files for an agent folder, and build
//! the static Linux binary that the container bind-mounts.
//!
//! Security model: no binary baked into the image, no docker socket, and only
//! granular bind mounts. The agent can write `memory*`, `data/`, `logs/` (and
//! `pi-agent/` for pi) but never `Dockerfile`, `docker-compose.yml`, or `.env`,
//! so it cannot plant code that the host would later execute.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::composer;

/// Pinned image for the in-Docker musl build.
const RUST_IMAGE: &str = "rust:1.96-alpine";
const MEMORY_FILE: &str = "memory.md";
const PI_PACKAGE: &str = "@earendil-works/pi-coding-agent";

/// Files written vs left untouched by one scaffold run.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ScaffoldReport {
    pub written: Vec<String>,
    pub skipped: Vec<String>,
}

impl ScaffoldReport {
    fn note(&mut self, name: &str, wrote: bool) {
        if wrote {
            &mut self.written
        } else {
            &mut self.skipped
        }
        .push(name.to_string());
    }
}

/// The slice of `agent.yaml` the sandbox scaffolder reads.
#[derive(Debug, Deserialize)]
struct SandboxConfig {
    #[serde(default)]
    sandboxed: bool,
    #[serde(default)]
    runner: SandboxRunner,
    #[serde(default)]
    providers: std::collections::HashMap<String, SandboxProvider>,
    #[serde(default)]
    system_files: Vec<SandboxSystemFile>,
    #[serde(default)]
    dashboard: Option<SandboxDashboard>,
    #[serde(default)]
    extensions: std::collections::HashMap<String, serde_yaml::Value>,
}

#[derive(Debug, Default, Deserialize)]
struct SandboxDashboard {
    #[serde(default)]
    bind: Option<std::net::IpAddr>,
    #[serde(default)]
    port: Option<u16>,
}

/// Fixed dashboard port written when chat-web needs a publishable port.
const CHAT_PORT: u16 = 7878;

#[derive(Debug, Default, Deserialize)]
struct SandboxRunner {
    #[serde(rename = "use", alias = "sdk", default)]
    use_: String,
}

#[derive(Debug, Deserialize)]
struct SandboxProvider {
    #[serde(default)]
    api_key: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum SandboxSystemFile {
    Bare(String),
    Detailed { path: String },
}

impl SandboxSystemFile {
    fn path(&self) -> &str {
        match self {
            Self::Bare(path) | Self::Detailed { path } => path,
        }
    }
}

fn load_config(root: &Path) -> Result<SandboxConfig> {
    let path = root.join("agent.yaml");
    let raw = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_yaml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

/// Whether `agent.yaml` opts into sandbox mode. Missing/unparsable ⇒ false.
pub fn is_sandboxed(root: &Path) -> bool {
    load_config(root).map(|c| c.sandboxed).unwrap_or(false)
}

impl SandboxConfig {
    fn runner_kind(&self) -> &str {
        let kind = self.runner.use_.trim();
        if kind.is_empty() {
            "pi"
        } else {
            kind
        }
    }
}

/// Only runners that work inside the minimal container are supported.
fn check_runner(kind: &str) -> Result<()> {
    match kind {
        "builtin" | "pi" => Ok(()),
        other => {
            bail!("runner {other:?} is not supported in sandbox mode (supported: builtin, pi)")
        }
    }
}

/// Append `sandboxed: true` as a top-level line without re-serializing, so
/// comments survive. `Ok(None)` when the parsed mapping already has the key.
/// Errors when the file cannot be safely appended to.
fn with_sandboxed_line(text: &str) -> Result<Option<String>> {
    with_top_level_block(text, "sandboxed", "sandboxed: true\n")
}

/// Append `block` when top-level `key` is absent; text-only (keeps comments).
fn with_top_level_block(text: &str, key: &str, block: &str) -> Result<Option<String>> {
    let manual = format!("add `{}` to agent.yaml manually", block.trim());
    let value: serde_yaml::Value = serde_yaml::from_str(text).context("parsing agent.yaml")?;
    let Some(map) = value.as_mapping() else {
        bail!("agent.yaml is not a mapping; {manual}");
    };
    if map.contains_key(key) {
        return Ok(None);
    }
    let first = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'));
    let last = text.lines().map(str::trim).rfind(|l| !l.is_empty());
    if first.is_some_and(|l| l.starts_with('{')) || last == Some("...") {
        bail!(
            "agent.yaml is not a plain block mapping (flow style or `...` document end); {manual}"
        );
    }
    let mut out = text.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(block);
    Ok(Some(out))
}

impl SandboxConfig {
    /// chat-web is linked when its section is present and not `enabled: false`.
    fn chat_web(&self) -> bool {
        self.extensions
            .get("chat-web")
            .is_some_and(|v| v.get("enabled").and_then(serde_yaml::Value::as_bool) != Some(false))
    }
}

/// Port to publish for chat-web (served by the dashboard server). Returns the
/// port and, when agent.yaml has no `dashboard:` section, the block to append.
/// The default port 0 (ephemeral) can't be published, so a fixed one is needed.
fn chat_port(cfg: &SandboxConfig) -> Result<Option<(u16, Option<String>)>> {
    if !cfg.chat_web() {
        return Ok(None);
    }
    let Some(dash) = &cfg.dashboard else {
        let block = format!("dashboard:\n  port: {CHAT_PORT}\n");
        return Ok(Some((CHAT_PORT, Some(block))));
    };
    if dash.bind.is_some_and(|b| b.is_loopback()) {
        bail!("chat-web: dashboard.bind is loopback, unreachable from outside the container; remove it (default 0.0.0.0)");
    }
    match dash.port {
        Some(p) if p != 0 => Ok(Some((p, None))),
        _ => bail!("chat-web: set a fixed `dashboard.port` (e.g. {CHAT_PORT}) in agent.yaml so the sandbox can publish it"),
    }
}

/// Host paths the container may write. Each must stay inside the agent folder.
const WRITABLE_DIRS: [&str; 5] = ["memory", "data", "logs", "workspaces", "cron"];

/// Bail when a writable mount resolves (via symlink) outside `root`.
fn check_writable_contained(root: &Path, pi: bool) -> Result<()> {
    let base = root
        .canonicalize()
        .with_context(|| format!("resolving {}", root.display()))?;
    let mut names: Vec<&str> = WRITABLE_DIRS.to_vec();
    names.push(MEMORY_FILE);
    if pi {
        names.push("pi-agent");
    }
    for name in names {
        let resolved = root
            .join(name)
            .canonicalize()
            .with_context(|| format!("resolving writable mount {name}"))?;
        if !resolved.starts_with(&base) {
            bail!(
                "writable mount {name} resolves outside the agent folder ({}); refusing",
                resolved.display()
            );
        }
    }
    Ok(())
}

/// Invoking host user, so bind-mounted dirs are writable by the container.
fn host_ids() -> (u32, u32) {
    // SAFETY: getuid/getgid take no arguments and cannot fail.
    unsafe { (libc::getuid(), libc::getgid()) }
}

/// Env var names referenced as `$env:NAME` in `providers.*.api_key`, sorted.
fn env_placeholders(cfg: &SandboxConfig) -> Vec<String> {
    let names: BTreeSet<String> = cfg
        .providers
        .values()
        .filter_map(|p| p.api_key.as_deref())
        .filter_map(|v| v.trim().strip_prefix("$env:"))
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .collect();
    names.into_iter().collect()
}

/// True when a system_files path is absolute or climbs out of the agent folder.
fn escapes_agent(path: &str) -> bool {
    let p = Path::new(path);
    p.is_absolute()
        || p.components()
            .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
}

fn render_dockerfile(runner: &str) -> String {
    let mut out = String::from(
        "# Sandbox image for this dar agent. The dar binary is NOT baked in: it is\n\
         # bind-mounted at /agent/bin/dar (see docker-compose.yml). Rebuild it with\n\
         # `dar build`, then `docker compose restart`.\n",
    );
    if runner == "pi" {
        out.push_str("FROM node:22-bookworm-slim\n");
        out.push_str(
            "RUN apt-get update -qq \\\n && apt-get install -y -qq --no-install-recommends ca-certificates git \\\n && rm -rf /var/lib/apt/lists/* \\\n",
        );
        out.push_str(&format!(" && npm install -g {PI_PACKAGE}\n"));
    } else {
        out.push_str("FROM debian:bookworm-slim\n");
        out.push_str(
            "RUN apt-get update -qq \\\n && apt-get install -y -qq --no-install-recommends ca-certificates git \\\n && rm -rf /var/lib/apt/lists/*\n",
        );
    }
    out.push_str("WORKDIR /agent\nCMD [\"/agent/bin/dar\",\"run\"]\n");
    out
}

/// `root` supplies which optional files exist; `system_files` are the already
/// filtered (contained, non-memory) entries to mount read-only.
fn render_compose(root: &Path, cfg: &SandboxConfig, port: Option<u16>) -> String {
    let pi = cfg.runner_kind() == "pi";
    let mut out = String::from(
        "# Generated by `dar sandbox`. Granular mounts: the agent cannot edit this\n\
         # file, the Dockerfile, or .env. Host paths must exist before `up`.\n\
         services:\n  agent:\n    build: .\n    working_dir: /agent\n    env_file: .env\n",
    );
    out.push_str("    user: \"${UID:-1000}:${GID:-1000}\"\n");
    out.push_str("    environment:\n      HOME: /home/agent\n");
    out.push_str("    cap_drop: [ALL]\n    security_opt:\n      - no-new-privileges:true\n");
    out.push_str("    cpus: 2.0\n    mem_limit: 4g\n    pids_limit: 512\n");
    out.push_str("    read_only: true\n    restart: unless-stopped\n");
    if let Some(p) = port {
        // chat-web + dashboard. Loopback on the host; widen deliberately.
        out.push_str(&format!(
            "    ports:\n      - \"127.0.0.1:${{CHAT_PORT:-{p}}}:{p}\"\n"
        ));
    }
    out.push_str(
        "    tmpfs:\n      - /tmp\n      - /home/agent:uid=${UID:-1000},gid=${GID:-1000}\n",
    );
    if pi {
        // pi chat writes /agent/.pi/mcp.json; root fs is read-only.
        out.push_str("      - /agent/.pi:uid=${UID:-1000},gid=${GID:-1000}\n");
    }
    out.push_str("    volumes:\n");
    out.push_str("      - ./bin/dar-sandbox:/agent/bin/dar:ro\n");
    out.push_str("      - ./agent.yaml:/agent/agent.yaml:ro\n");
    let mut ro: Vec<String> = vec!["AGENTS.md".into(), "TOOLS.md".into(), "WORKFLOW.md".into()];
    for f in &cfg.system_files {
        let path = f.path().trim_start_matches("./").to_string();
        if !escapes_agent(&path) && path != MEMORY_FILE && !ro.contains(&path) {
            ro.push(path);
        }
    }
    for path in ro {
        if root.join(&path).is_file() {
            out.push_str(&format!("      - ./{path}:/agent/{path}:ro\n"));
        }
    }
    out.push_str("      - ./memory.md:/agent/memory.md\n");
    out.push_str("      - ./memory:/agent/memory\n");
    out.push_str("      - ./skills:/agent/skills:ro\n");
    if pi {
        out.push_str("      - ./pi-agent:/home/agent/.pi/agent\n");
        out.push_str("      - ./skills:/home/agent/.pi/agent/skills:ro\n");
    }
    out.push_str("      - ./data:/agent/data\n      - ./logs:/agent/logs\n");
    out.push_str("      - ./workspaces:/agent/workspaces\n      - ./cron:/agent/cron\n");
    out.push_str("      # - ${WORKSPACE_SRC}:/code\n");
    out
}

fn render_env_example(names: &[String], uid: u32, gid: u32) -> String {
    let mut out = String::from(
        "# Copy to .env (git-ignored, mode 600). Never commit secrets.\n\
         # Host user the container runs as (bind-mounted dirs must be writable by it).\n\
         # Written by `dar sandbox` from the invoking user.\n",
    );
    out.push_str(&format!("UID={uid}\nGID={gid}\n\n"));
    for name in names {
        out.push_str(&format!("{name}=\n"));
    }
    out.push_str("\n# Host directory mounted at /code (also uncomment it in docker-compose.yml).\n# WORKSPACE_SRC=/absolute/path/to/repo\n");
    out
}

fn render_readme(runner: &str) -> String {
    let auth = if runner == "pi" {
        "\n## Pi auth\n\nRun `docker compose run --rm agent pi` once and `/login`, or put provider keys in `.env`. Credentials persist in `./pi-agent` (git-ignored, writable by the container).\n"
    } else {
        ""
    };
    format!(
        "# Sandbox\n\n\
Runs this agent in Docker so a misbehaving agent can only touch what is mounted.\n\n\
## Security model\n\n\
- The static `dar` binary (`bin/dar-sandbox`) is bind-mounted read-only; it is not in the image.\n\
- Granular mounts: `agent.yaml`, `AGENTS.md`, `TOOLS.md`, other system files and `skills/` are read-only. `WORKFLOW.md` is read-only. Only `memory.md`, `memory/`, `data/`, `logs/`, `workspaces/`, `cron/` (and `pi-agent/` for pi) are writable.\n\
- The agent cannot edit `Dockerfile`, `docker-compose.yml` or `.env`, so it cannot plant code the host later runs.\n\
- All capabilities dropped, `no-new-privileges`, read-only root fs, CPU/memory/pids limits.\n\
- The Docker socket is never mounted.\n\n\
## Build / run / update\n\n\
```bash\ndar build                 # also builds bin/dar-sandbox (static musl, inside Docker)\ndocker compose up -d      # start\ndocker compose restart    # pick up a rebuilt binary\ndocker compose logs -f\n```\n\
{auth}\n\
## Host user

`.env` holds `UID`/`GID` of the user who ran `dar sandbox`; the container runs as them. Edit `.env` if you run Docker as someone else.

## Chat web\n\n\
When `chat-web` is enabled the dashboard runs on a fixed `dashboard.port` and is published on the host's loopback only: open `http://127.0.0.1:<port>/chat`. Override the host port with `CHAT_PORT` in `.env`. To reach it remotely, put a proxy (e.g. `tailscale serve`) in front rather than binding 0.0.0.0. Use `foreground: logs`; there is no TTY for the TUI.\n\n\
## Workspace mounts\n\n\
Uncomment `- ${{WORKSPACE_SRC}}:/code` in `docker-compose.yml`, set `WORKSPACE_SRC` in `.env`, and add more lines the same way.\n\n\
## Gotcha\n\n\
Host paths must exist before `docker compose up`; Docker creates missing ones as root-owned directories. New optional files (e.g. a later `TOOLS.md`) need a compose edit to be mounted. Edits to a single-file mount (`memory.md`) that replace the file (rename) fail with EBUSY; append in place.\n"
    )
}

/// Append `KEY=value` lines for keys not already set in the env file. Returns
/// the keys added. Existing values are never changed.
fn ensure_env_keys(path: &Path, keys: &[(&str, String)]) -> Result<Vec<String>> {
    let mut text =
        fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let has = |text: &str, k: &str| {
        text.lines().any(|l| {
            l.trim_start()
                .strip_prefix(k)
                .is_some_and(|r| r.starts_with('='))
        })
    };
    let mut added = Vec::new();
    for (k, v) in keys {
        if !has(&text, k) {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&format!("{k}={v}\n"));
            added.push((*k).to_string());
        }
    }
    if !added.is_empty() {
        fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(added)
}

/// First host loopback port from `start` that is free right now (`start` if
/// none of the next 100 are). Only consulted when `CHAT_PORT` is unset.
fn free_host_port(start: u16) -> u16 {
    (start..start.saturating_add(100))
        .find(|p| std::net::TcpListener::bind(("127.0.0.1", *p)).is_ok())
        .unwrap_or(start)
}

/// Write `contents` only when `path` is missing. Returns whether it wrote.
fn write_if_missing(
    root: &Path,
    name: &str,
    contents: &str,
    report: &mut ScaffoldReport,
) -> Result<()> {
    let path = root.join(name);
    let wrote = !path.exists();
    if wrote {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
    }
    report.note(name, wrote);
    Ok(())
}

fn ensure_dir(root: &Path, name: &str, report: &mut ScaffoldReport) -> Result<()> {
    let path = root.join(name);
    let wrote = !path.exists();
    if wrote {
        fs::create_dir_all(&path).with_context(|| format!("creating {}", path.display()))?;
    }
    report.note(&format!("{name}/"), wrote);
    Ok(())
}

/// Append `/pi-agent/` to `.gitignore` when absent (after the standard block).
fn ensure_pi_agent_ignored(root: &Path) -> Result<()> {
    composer::ensure_agent_gitignore(root)?;
    let path = root.join(".gitignore");
    let mut text =
        fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    if !text.lines().any(|l| l.trim() == "/pi-agent/") {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("\n# pi credentials/sessions (sandbox)\n/pi-agent/\n");
        fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

/// Scaffold sandbox files in `root` (must contain `agent.yaml`). Idempotent.
pub fn scaffold(root: &Path) -> Result<ScaffoldReport> {
    let yaml_path = root.join("agent.yaml");
    if !yaml_path.is_file() {
        bail!("{} not found; run `dar create` first", yaml_path.display());
    }
    let cfg = load_config(root)?;
    check_runner(cfg.runner_kind())?;
    for f in &cfg.system_files {
        if escapes_agent(f.path()) {
            eprintln!(
                "warning: system_files entry {:?} is absolute or escapes the agent folder; it will not be available in the sandbox",
                f.path()
            );
        }
    }

    let mut report = ScaffoldReport::default();
    let raw = fs::read_to_string(&yaml_path)
        .with_context(|| format!("reading {}", yaml_path.display()))?;
    let port = chat_port(&cfg)?;
    let mut updated_yaml = with_sandboxed_line(&raw)?;
    if let Some((_, Some(block))) = &port {
        let base = updated_yaml.clone().unwrap_or_else(|| raw.clone());
        updated_yaml = with_top_level_block(&base, "dashboard", block)?.or(updated_yaml);
    }

    let pi = cfg.runner_kind() == "pi";
    write_if_missing(root, MEMORY_FILE, "", &mut report)?;
    ensure_dir(root, "memory", &mut report)?;
    ensure_dir(root, "skills", &mut report)?;
    ensure_dir(root, "data", &mut report)?;
    ensure_dir(root, "logs", &mut report)?;
    if pi {
        ensure_dir(root, "pi-agent", &mut report)?;
    }
    ensure_dir(root, "workspaces", &mut report)?;
    ensure_dir(root, "cron", &mut report)?;
    check_writable_contained(root, pi)?;
    match updated_yaml {
        Some(updated) => {
            fs::write(&yaml_path, updated)
                .with_context(|| format!("writing {}", yaml_path.display()))?;
            report.note("agent.yaml (sandboxed / dashboard.port)", true);
        }
        None => report.note("agent.yaml (sandboxed / dashboard.port)", false),
    }
    write_if_missing(
        root,
        "Dockerfile",
        &render_dockerfile(cfg.runner_kind()),
        &mut report,
    )?;
    write_if_missing(
        root,
        "docker-compose.yml",
        &render_compose(root, &cfg, port.as_ref().map(|(p, _)| *p)),
        &mut report,
    )?;
    let env_example = {
        let (uid, gid) = host_ids();
        render_env_example(&env_placeholders(&cfg), uid, gid)
    };
    write_if_missing(root, ".env.example", &env_example, &mut report)?;
    write_if_missing(root, ".env", &env_example, &mut report)?;
    // An existing .env (converted agent) still needs the sandbox keys.
    let (uid, gid) = host_ids();
    let mut keys = vec![("UID", uid.to_string()), ("GID", gid.to_string())];
    if let Some(&(p, _)) = port.as_ref() {
        let host = free_host_port(p);
        if host != p {
            eprintln!("note: host port {p} is busy; publishing chat-web on 127.0.0.1:{host} (CHAT_PORT in .env)");
            keys.push(("CHAT_PORT", host.to_string()));
        }
    }
    let added = ensure_env_keys(&root.join(".env"), &keys)?;
    if !added.is_empty() {
        report.note(&format!(".env (+{})", added.join(", ")), true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root.join(".env"), fs::Permissions::from_mode(0o600))
            .context("chmod 600 .env")?;
    }
    write_if_missing(
        root,
        "SANDBOX.md",
        &render_readme(cfg.runner_kind()),
        &mut report,
    )?;
    ensure_pi_agent_ignored(root)?;
    Ok(report)
}

/// `dar sandbox` entry: scaffold and print written/skipped lists.
pub fn run(root: &Path) -> Result<()> {
    let report = scaffold(root)?;
    println!("written:");
    for f in &report.written {
        println!("  {f}");
    }
    println!("skipped (already present):");
    for f in &report.skipped {
        println!("  {f}");
    }
    Ok(())
}

/// Build the static musl binary inside Docker and install it at
/// `<agent>/bin/dar-sandbox`. `crate_dir` is the already-composed `.dar`.
pub fn build_binary(agent: &Path, crate_dir: &Path) -> Result<()> {
    check_runner(load_config(agent)?.runner_kind())?;
    let src = composer::dar_source_root()?;
    let target_dir = crate_dir.join("target/sandbox");
    // Alpine's toolchain is musl-native, so a plain `cargo build` yields a
    // static binary for the Docker host's arch with no cross-compiler setup.
    let script = "export RUSTUP_TOOLCHAIN=$RUST_VERSION; apk add -q musl-dev cmake make perl gcc g++ && cargo build --release";
    let mount = |p: &Path| format!("{0}:{0}", p.display());
    let mut cmd = Command::new("docker");
    cmd.args(["run", "--rm", "-v", &mount(&src)]);
    if !agent.starts_with(&src) {
        cmd.args(["-v", &mount(agent)]);
    }
    cmd.arg("-w")
        .arg(crate_dir)
        .arg("-e")
        .arg(format!("CARGO_TARGET_DIR={}", target_dir.display()))
        .arg(RUST_IMAGE)
        .args(["sh", "-c", script]);
    let status = cmd.status().context("running docker (is it installed?)")?;
    if !status.success() {
        bail!("docker sandbox build exited with {status}");
    }
    let built = target_dir.join("release/dar");
    let dest = agent.join("bin/dar-sandbox");
    fs::create_dir_all(agent.join("bin")).context("creating bin/")?;
    fs::copy(&built, &dest)
        .with_context(|| format!("copying {} to {}", built.display(), dest.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(yaml: &str) -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("agent.yaml"), yaml).unwrap();
        temp
    }

    const BUILTIN: &str = "id: a\nname: A\n# keep me\nrunner:\n  use: builtin\n  provider: p\nproviders:\n  p:\n    api_url: http://x\n    api_key: $env:P_KEY\nsystem_files:\n  - memory.md\n  - /etc/passwd\n  - ../x.md\n";

    #[test]
    fn scaffold_is_idempotent() {
        let temp = agent(BUILTIN);
        let first = scaffold(temp.path()).unwrap();
        assert!(first.written.contains(&"Dockerfile".to_string()));
        let second = scaffold(temp.path()).unwrap();
        assert!(second.written.is_empty(), "{second:?}");
        assert!(second.skipped.contains(&"Dockerfile".to_string()));
        let yaml = fs::read_to_string(temp.path().join("agent.yaml")).unwrap();
        assert_eq!(yaml.matches("sandboxed: true").count(), 1);
        assert!(yaml.contains("# keep me"));
    }

    #[test]
    fn scaffold_requires_agent_yaml() {
        let temp = tempfile::tempdir().unwrap();
        assert!(scaffold(temp.path()).is_err());
    }

    #[test]
    fn sandboxed_line_appended_once() {
        assert_eq!(
            with_sandboxed_line("id: a").unwrap().as_deref(),
            Some("id: a\nsandboxed: true\n")
        );
        assert_eq!(
            with_sandboxed_line("id: a\nsandboxed: true\n").unwrap(),
            None
        );
        assert_eq!(with_sandboxed_line("sandboxed: false\n").unwrap(), None);
        // quoted key / not a line prefix match
        assert_eq!(with_sandboxed_line("\"sandboxed\": true\n").unwrap(), None);
        assert!(with_sandboxed_line("id: a\nx:\n  sandboxed: true\n")
            .unwrap()
            .is_some());
    }

    #[test]
    fn sandboxed_line_rejects_unsafe_yaml() {
        for bad in ["- a\n- b\n", "{id: a}\n", "id: a\n...\n"] {
            let err = with_sandboxed_line(bad).unwrap_err().to_string();
            assert!(err.contains("manually"), "{bad:?}: {err}");
        }
    }

    #[test]
    fn compose_mounts_loop_dirs_and_pi_tmpfs() {
        let temp = agent(&BUILTIN.replace("builtin", "pi"));
        fs::write(temp.path().join("WORKFLOW.md"), "x").unwrap();
        scaffold(temp.path()).unwrap();
        let c = fs::read_to_string(temp.path().join("docker-compose.yml")).unwrap();
        assert!(c.contains("- /agent/.pi:uid="));
        assert!(c.contains("./WORKFLOW.md:/agent/WORKFLOW.md:ro"));
        assert!(c.contains("./workspaces:/agent/workspaces\n"));
        assert!(c.contains("./cron:/agent/cron\n"));
        assert!(temp.path().join("workspaces").is_dir());
        let b = agent(BUILTIN);
        scaffold(b.path()).unwrap();
        let c = fs::read_to_string(b.path().join("docker-compose.yml")).unwrap();
        assert!(!c.contains("/agent/.pi"));
        assert!(!c.contains("WORKFLOW.md"));
    }

    #[test]
    fn chat_web_publishes_fixed_dashboard_port() {
        // No dashboard section: port block appended, published on loopback.
        let t = agent(&format!(
            "{BUILTIN}extensions:\n  chat-web:\n    enabled: true\n"
        ));
        scaffold(t.path()).unwrap();
        let y = fs::read_to_string(t.path().join("agent.yaml")).unwrap();
        assert!(
            y.ends_with("sandboxed: true\ndashboard:\n  port: 7878\n"),
            "{y}"
        );
        let c = fs::read_to_string(t.path().join("docker-compose.yml")).unwrap();
        assert!(c.contains("- \"127.0.0.1:${CHAT_PORT:-7878}:7878\""), "{c}");
        scaffold(t.path()).unwrap(); // idempotent
        assert_eq!(fs::read_to_string(t.path().join("agent.yaml")).unwrap(), y);

        // Existing fixed port is reused.
        let t = agent(&format!(
            "{BUILTIN}dashboard:\n  port: 9000\nextensions:\n  chat-web: {{}}\n"
        ));
        scaffold(t.path()).unwrap();
        let c = fs::read_to_string(t.path().join("docker-compose.yml")).unwrap();
        assert!(c.contains("${CHAT_PORT:-9000}:9000"), "{c}");

        // Ephemeral port or loopback bind: refuse, nothing written.
        for bad in [
            "dashboard:\n  bind: 1.1.1.1\n",
            "dashboard:\n  bind: 127.0.0.1\n  port: 9000\n",
        ] {
            let t = agent(&format!("{BUILTIN}{bad}extensions:\n  chat-web: {{}}\n"));
            assert!(scaffold(t.path()).is_err(), "{bad}");
            assert!(!t.path().join("docker-compose.yml").exists());
        }

        // Disabled chat-web or none: no ports.
        let t = agent(&format!(
            "{BUILTIN}extensions:\n  chat-web:\n    enabled: false\n"
        ));
        scaffold(t.path()).unwrap();
        let c = fs::read_to_string(t.path().join("docker-compose.yml")).unwrap();
        assert!(!c.contains("ports:"));
    }

    #[cfg(unix)]
    #[test]
    fn env_uses_invoking_user_ids() {
        let temp = agent(BUILTIN);
        scaffold(temp.path()).unwrap();
        let env = fs::read_to_string(temp.path().join(".env")).unwrap();
        let (uid, gid) = host_ids();
        assert!(env.contains(&format!("UID={uid}\nGID={gid}\n")), "{env}");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_writable_mounts_rejected() {
        let outside = tempfile::tempdir().unwrap();
        // dir symlink
        let t = agent(BUILTIN);
        std::os::unix::fs::symlink(outside.path(), t.path().join("data")).unwrap();
        let err = scaffold(t.path()).unwrap_err().to_string();
        assert!(err.contains("outside the agent folder"), "{err}");
        assert!(!fs::read_to_string(t.path().join("agent.yaml"))
            .unwrap()
            .contains("sandboxed: true"));
        // file symlink
        let t = agent(BUILTIN);
        let target = outside.path().join("m.md");
        fs::write(&target, "").unwrap();
        std::os::unix::fs::symlink(&target, t.path().join("memory.md")).unwrap();
        let err = scaffold(t.path()).unwrap_err().to_string();
        assert!(err.contains("memory.md"), "{err}");
    }

    #[test]
    fn runner_gating() {
        assert!(check_runner("builtin").is_ok());
        assert!(check_runner("pi").is_ok());
        let err = check_runner("codex").unwrap_err().to_string();
        assert!(err.contains("not supported in sandbox mode"), "{err}");
        let temp = agent("id: a\nname: A\nrunner:\n  use: codex\n");
        assert!(scaffold(temp.path()).is_err());
        assert!(!temp.path().join("Dockerfile").exists());
    }

    #[test]
    fn env_placeholders_from_api_key_refs_only() {
        let cfg = load_config(agent(BUILTIN).path()).unwrap();
        assert_eq!(env_placeholders(&cfg), vec!["P_KEY".to_string()]);
        let temp = agent(BUILTIN);
        scaffold(temp.path()).unwrap();
        let env = fs::read_to_string(temp.path().join(".env")).unwrap();
        assert!(env.contains("P_KEY=\n"));
    }

    #[test]
    fn compose_has_no_docker_socket_and_hardening() {
        for runner in ["builtin", "pi"] {
            let temp = agent(&BUILTIN.replace("builtin", runner));
            scaffold(temp.path()).unwrap();
            let compose = fs::read_to_string(temp.path().join("docker-compose.yml")).unwrap();
            assert!(!compose.contains("docker.sock"));
            assert!(compose.contains("cap_drop: [ALL]"));
            assert!(compose.contains("no-new-privileges:true"));
            assert_eq!(compose.contains("pi-agent"), runner == "pi");
            assert!(!compose.contains("/etc/passwd"));
            assert!(!compose.contains("../x.md"));
        }
    }

    #[test]
    fn dockerfile_depends_on_runner() {
        assert!(render_dockerfile("pi").contains(PI_PACKAGE));
        assert!(render_dockerfile("builtin").contains("debian:bookworm-slim"));
        assert!(render_dockerfile("builtin").contains("CMD [\"/agent/bin/dar\",\"run\"]"));
    }

    #[test]
    fn escape_detection() {
        assert!(escapes_agent("/abs"));
        assert!(escapes_agent("../x"));
        assert!(!escapes_agent("docs/a.md"));
    }

    #[cfg(unix)]
    #[test]
    fn env_file_is_private_and_gitignored() {
        use std::os::unix::fs::PermissionsExt;
        let temp = agent(BUILTIN);
        scaffold(temp.path()).unwrap();
        let mode = fs::metadata(temp.path().join(".env"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let ignore = fs::read_to_string(temp.path().join(".gitignore")).unwrap();
        for entry in [".env", "/bin/", "/pi-agent/"] {
            assert!(ignore.lines().any(|l| l.trim() == entry), "{entry}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn existing_env_gets_ids_and_keeps_values() {
        use std::os::unix::fs::PermissionsExt;
        let temp = agent(BUILTIN);
        let env = temp.path().join(".env");
        fs::write(&env, "P_KEY=secret\nGID=42").unwrap();
        fs::set_permissions(&env, fs::Permissions::from_mode(0o644)).unwrap();
        scaffold(temp.path()).unwrap();
        let (uid, _) = host_ids();
        assert_eq!(
            fs::read_to_string(&env).unwrap(),
            format!("P_KEY=secret\nGID=42\nUID={uid}\n")
        );
        assert_eq!(
            fs::metadata(&env).unwrap().permissions().mode() & 0o777,
            0o600
        );
        scaffold(temp.path()).unwrap(); // idempotent
        assert_eq!(fs::read_to_string(&env).unwrap().matches("UID=").count(), 1);
    }

    #[test]
    fn busy_chat_port_sets_chat_port_in_env() {
        let busy = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let p = busy.local_addr().unwrap().port();
        assert_ne!(free_host_port(p), p);
        let t = agent(&format!(
            "{BUILTIN}dashboard:\n  port: {p}\nextensions:\n  chat-web: {{}}\n"
        ));
        scaffold(t.path()).unwrap();
        let env = fs::read_to_string(t.path().join(".env")).unwrap();
        let line = env.lines().find(|l| l.starts_with("CHAT_PORT=")).unwrap();
        assert_ne!(line, format!("CHAT_PORT={p}"));
    }
}
