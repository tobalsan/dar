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
const RUST_IMAGE: &str = "rust:1.88-bookworm";
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
}

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
/// comments survive. `None` when a top-level `sandboxed:` key already exists.
fn with_sandboxed_line(text: &str) -> Option<String> {
    if text.lines().any(|l| l.starts_with("sandboxed:")) {
        return None;
    }
    let mut out = text.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("sandboxed: true\n");
    Some(out)
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
fn render_compose(root: &Path, cfg: &SandboxConfig) -> String {
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
    out.push_str(
        "    tmpfs:\n      - /tmp\n      - /home/agent:uid=${UID:-1000},gid=${GID:-1000}\n",
    );
    out.push_str("    volumes:\n");
    out.push_str("      - ./bin/dar-sandbox:/agent/bin/dar:ro\n");
    out.push_str("      - ./agent.yaml:/agent/agent.yaml:ro\n");
    let mut ro: Vec<String> = vec!["AGENTS.md".into(), "TOOLS.md".into()];
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
    out.push_str("      # - ${WORKSPACE_SRC}:/code\n");
    out
}

fn render_env_example(names: &[String]) -> String {
    let mut out = String::from(
        "# Copy to .env (git-ignored, mode 600). Never commit secrets.\n\
         # Host user the container runs as (bind-mounted dirs must be writable by it).\n\
         UID=1000\nGID=1000\n\n",
    );
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
- Granular mounts: `agent.yaml`, `AGENTS.md`, `TOOLS.md`, other system files and `skills/` are read-only. Only `memory.md`, `memory/`, `data/`, `logs/` (and `pi-agent/` for pi) are writable.\n\
- The agent cannot edit `Dockerfile`, `docker-compose.yml` or `.env`, so it cannot plant code the host later runs.\n\
- All capabilities dropped, `no-new-privileges`, read-only root fs, CPU/memory/pids limits.\n\
- The Docker socket is never mounted.\n\n\
## Build / run / update\n\n\
```bash\ndar build                 # also builds bin/dar-sandbox (static musl, inside Docker)\ndocker compose up -d      # start\ndocker compose restart    # pick up a rebuilt binary\ndocker compose logs -f\n```\n\
{auth}\n\
## Workspace mounts\n\n\
Uncomment `- ${{WORKSPACE_SRC}}:/code` in `docker-compose.yml`, set `WORKSPACE_SRC` in `.env`, and add more lines the same way.\n\n\
## Gotcha\n\n\
Host paths must exist before `docker compose up`; Docker creates missing ones as root-owned directories. New optional files (e.g. a later `TOOLS.md`) need a compose edit to be mounted. Edits to a single-file mount (`memory.md`) that replace the file (rename) fail with EBUSY; append in place.\n"
    )
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
    match with_sandboxed_line(&raw) {
        Some(updated) => {
            fs::write(&yaml_path, updated)
                .with_context(|| format!("writing {}", yaml_path.display()))?;
            report.note("agent.yaml (sandboxed: true)", true);
        }
        None => report.note("agent.yaml (sandboxed: true)", false),
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
    write_if_missing(
        root,
        "Dockerfile",
        &render_dockerfile(cfg.runner_kind()),
        &mut report,
    )?;
    write_if_missing(
        root,
        "docker-compose.yml",
        &render_compose(root, &cfg),
        &mut report,
    )?;
    let env_example = render_env_example(&env_placeholders(&cfg));
    write_if_missing(root, ".env.example", &env_example, &mut report)?;
    let env_missing = !root.join(".env").exists();
    write_if_missing(root, ".env", &env_example, &mut report)?;
    #[cfg(unix)]
    if env_missing {
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

fn sandbox_triple() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("x86_64-unknown-linux-musl"),
        "aarch64" => Ok("aarch64-unknown-linux-musl"),
        other => bail!("sandbox build supports only x86_64/aarch64 hosts (got {other})"),
    }
}

/// Build the static musl binary inside Docker and install it at
/// `<agent>/bin/dar-sandbox`. `crate_dir` is the already-composed `.dar`.
pub fn build_binary(agent: &Path, crate_dir: &Path) -> Result<()> {
    check_runner(load_config(agent)?.runner_kind())?;
    let triple = sandbox_triple()?;
    let src = composer::dar_source_root()?;
    let target_dir = crate_dir.join("target/sandbox");
    let script = format!(
        "export RUSTUP_TOOLCHAIN=$RUST_VERSION; apt-get update -qq && apt-get install -y -qq musl-tools cmake && rustup target add {triple} && cargo build --release --target {triple}"
    );
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
        .args(["bash", "-c", &script]);
    let status = cmd.status().context("running docker (is it installed?)")?;
    if !status.success() {
        bail!("docker sandbox build exited with {status}");
    }
    let built = target_dir.join(triple).join("release/dar");
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
            with_sandboxed_line("id: a").as_deref(),
            Some("id: a\nsandboxed: true\n")
        );
        assert_eq!(with_sandboxed_line("id: a\nsandboxed: true\n"), None);
        assert_eq!(with_sandboxed_line("sandboxed: false\n"), None);
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
}
