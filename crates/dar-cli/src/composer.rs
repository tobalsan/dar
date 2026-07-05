//! Per-agent composition crate generation.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use semver::{Version, VersionReq};
use serde::Deserialize;

/// Standard `.gitignore` block for an agent folder. Lines are grouped under
/// comment headers; blank lines separate groups. This is the single source of
/// truth for the entries `dar init-build` guarantees in every agent folder.
const STANDARD_GITIGNORE: &str = "\
# secret
.env

# built binary (rebuilt from .dar crate)
/bin/

# per-agent crate build output
/.dar/target/

# run history + tui runtime
/data/

# logs
/logs/

# per-issue repo checkouts
/workspaces/

# runner session state
/claude-sessions/
/opencode-sessions/
/pi-sessions/
";

const GENERATED_TOML_HEADER: &str = "# generated - do not hand-edit\n";
const GENERATED_RUST_HEADER: &str = "// # generated - do not hand-edit\n";
const STOCK_CRATE_VERSION_REQ: &str = concat!(
    env!("CARGO_PKG_VERSION_MAJOR"),
    ".",
    env!("CARGO_PKG_VERSION_MINOR")
);
const LOCAL_CRATE_VERSION: &str = env!("CARGO_PKG_VERSION");

const PATCHED_REGISTRY_CRATES: &[(&str, &str)] = &[
    ("dar-host-api", "crates/host-api"),
    ("dar-cap-chat", "crates/cap-chat"),
    ("dar-cap-runner", "crates/cap-runner"),
    ("dar-orchestrator-api", "crates/orchestrator-api"),
    ("dar-tool-registry", "crates/tool-registry"),
    ("dar-extension-sdk", "crates/extension-sdk"),
];

struct StockExtension {
    package: &'static str,
    factory: &'static str,
}

impl StockExtension {
    fn feature(&self) -> String {
        format!("stock-{}", self.package)
    }
}

#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct BuildOptions {
    pub vendor: bool,
    pub offline: bool,
    pub target: Option<String>,
    pub static_: bool,
    pub universal: bool,
}

const STOCK_EXTENSIONS: &[StockExtension] = &[
    StockExtension {
        package: "tool-registry-host",
        factory: "tool_registry_host::ToolRegistryHostExtension",
    },
    StockExtension {
        package: "frontend-log",
        factory: "frontend_log::FrontendLogExtension",
    },
    StockExtension {
        package: "system-context",
        factory: "system_context::SystemContextExtension",
    },
    StockExtension {
        package: "tracker-files",
        factory: "tracker_files::TrackerFilesExtension",
    },
    StockExtension {
        package: "tracker-linear",
        factory: "tracker_linear::TrackerLinearExtension",
    },
    StockExtension {
        package: "orchestrator",
        factory: "orchestrator::OrchestratorExtension::default()",
    },
    StockExtension {
        package: "dashboard",
        factory: "dashboard::DashboardExtension::default()",
    },
    StockExtension {
        package: "runner-pi",
        factory: "runner_pi::RunnerPiExtension",
    },
    StockExtension {
        package: "runner-codex",
        factory: "runner_codex::RunnerCodexExtension",
    },
    StockExtension {
        package: "runner-opencode",
        factory: "runner_opencode::RunnerOpenCodeExtension",
    },
    StockExtension {
        package: "runner-cli",
        factory: "runner_cli::RunnerCliExtension",
    },
    StockExtension {
        package: "runner-fake",
        factory: "runner_fake::RunnerFakeExtension",
    },
    StockExtension {
        package: "runner-builtin",
        factory: "runner_builtin::RunnerBuiltinExtension",
    },
    StockExtension {
        package: "chat-opencode",
        factory: "chat_opencode::ChatOpenCodeExtension",
    },
    StockExtension {
        package: "chat-pi",
        factory: "chat_pi::ChatPiExtension",
    },
    StockExtension {
        package: "chat-codex",
        factory: "chat_codex::ChatCodexExtension",
    },
    StockExtension {
        package: "tui",
        factory: "tui::TuiExtension",
    },
    StockExtension {
        package: "scheduler",
        factory: "scheduler::SchedulerExtension::default()",
    },
];

#[derive(Debug, Clone, Eq, PartialEq)]
struct LocalExtension {
    package: String,
    factory: String,
    path: PathBuf,
    requires_stock: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct AgentSelection {
    /// Absent for a passive agent (no orchestrator trio). `tracker-linear`
    /// remains baseline-linked; `tracker-files` links only when selected.
    #[serde(default)]
    tracker: Option<SelectedUse>,
    runner: SelectedUse,
    #[serde(default = "default_foreground")]
    foreground: String,
    /// Per-extension config sections. Presence of an opt-in stock extension's
    /// key here selects it into the composed binary.
    #[serde(default)]
    extensions: std::collections::HashMap<String, serde_yaml::Value>,
}

impl AgentSelection {
    /// Whether an opt-in stock extension is linked into the composed binary.
    /// Selection is by *presence* of the `extensions.<id>` section. The
    /// `enabled: false` flag is a runtime kill switch (the extension still
    /// links and loads, but stays idle), mirroring aihub, so it does not
    /// affect build-time selection.
    fn extension_selected(&self, id: &str) -> bool {
        self.extensions.contains_key(id)
    }
}

#[derive(Debug, Deserialize)]
struct SelectedUse {
    #[serde(rename = "use", alias = "sdk", alias = "type")]
    use_: String,
}

pub fn init_build(agent: &Path) -> Result<()> {
    init_build_with_options(agent, BuildOptions::default())
}

pub fn init_build_with_options(agent: &Path, options: BuildOptions) -> Result<()> {
    let (crate_dir, _) = write_composition_crate(agent)?;
    refresh_lockfile(&crate_dir, options.offline && !options.vendor)?;
    if options.vendor {
        vendor_dependencies(&crate_dir)?;
    }
    Ok(())
}

pub fn compose(agent: &Path) -> Result<bool> {
    write_composition_crate(agent).map(|(_, changed)| changed)
}

fn write_composition_crate(agent: &Path) -> Result<(PathBuf, bool)> {
    let agent = agent
        .canonicalize()
        .with_context(|| format!("resolving agent folder {}", agent.display()))?;
    let crate_dir = agent.join(".dar");
    fs::create_dir_all(crate_dir.join("src"))
        .with_context(|| format!("creating {}", crate_dir.join("src").display()))?;

    let source_root = dar_source_root()?;
    let locals = discover_extensions(&agent, &source_root)?;
    let stock = selected_stock_extensions(&agent, &locals)?;
    let mut changed = write_if_changed(
        &crate_dir.join("Cargo.toml"),
        &cargo_toml(&crate_dir, &stock, &locals, &source_root),
    )?;
    changed |= write_if_changed(&crate_dir.join("src/main.rs"), &main_rs(&stock, &locals))?;
    changed |= write_if_changed(
        &crate_dir.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"1.83\"\n",
    )?;
    changed |= ensure_agent_gitignore(&agent)?;
    Ok((crate_dir, changed))
}

const ALL_RUNNERS: &[&str] = &[
    "runner-pi",
    "runner-codex",
    "runner-opencode",
    "runner-cli",
    "runner-fake",
    "runner-builtin",
];

fn selected_stock_extensions(
    agent: &Path,
    locals: &[LocalExtension],
) -> Result<Vec<&'static StockExtension>> {
    let selection = agent_selection(agent)?;
    // Validate runner.use early so a typo fails at build time.
    let _ = runner_package(&selection.runner.use_)?;
    let mut packages = vec![
        "tool-registry-host",
        "frontend-log",
        "system-context",
        "orchestrator",
        "dashboard",
        "tracker-linear",
    ];
    // Passive agents omit `tracker`; files tracker links only when selected.
    // Linear stays baseline-linked for shared Linear CLI/export support.
    if let Some(tracker) = &selection.tracker {
        packages.push(tracker_package(&tracker.use_)?);
    }
    packages.extend_from_slice(ALL_RUNNERS);
    packages.extend(foreground_packages(&selection)?);
    // Opt-in stock extensions: selected only when present in agent.yaml's
    // `extensions` map. Absent → binary behaves as today.
    if selection.extension_selected("scheduler") {
        packages.push("scheduler");
    }
    // Stock extensions required by local extensions.
    for local in locals {
        for package in &local.requires_stock {
            packages.push(package.as_str());
        }
    }
    let mut selected = Vec::new();
    for stock in STOCK_EXTENSIONS {
        if packages.iter().any(|package| package == &stock.package) {
            selected.push(stock);
        }
    }
    for package in packages {
        if !STOCK_EXTENSIONS
            .iter()
            .any(|stock| stock.package == package)
        {
            bail!("unknown stock extension package {package}");
        }
    }
    Ok(selected)
}

/// Locate the dar source checkout that supplies the `publish = false` stock
/// extension crates. Honors `DAR_SRC`; otherwise walks up from the running
/// binary to the checkout root. Resolved fresh each compose (never baked at
/// compile time) so a relocated `dar` binary still emits correct paths.
fn dar_source_root() -> Result<PathBuf> {
    let is_root = |d: &Path| {
        d.join("crates/host-api/Cargo.toml").exists()
            && d.join("extensions/orchestrator/Cargo.toml").exists()
    };
    if let Some(src) = std::env::var_os("DAR_SRC") {
        let root = PathBuf::from(&src)
            .canonicalize()
            .with_context(|| format!("resolving DAR_SRC {}", Path::new(&src).display()))?;
        if !is_root(&root) {
            bail!(
                "DAR_SRC {} is not a dar checkout root (missing crates/host-api or extensions/orchestrator)",
                root.display()
            );
        }
        return Ok(root);
    }
    let exe = std::env::current_exe().context("resolving current executable path")?;
    for ancestor in exe.ancestors() {
        if is_root(ancestor) {
            return ancestor
                .canonicalize()
                .with_context(|| format!("canonicalizing dar source root {}", ancestor.display()));
        }
    }
    bail!("could not locate dar source tree; set DAR_SRC to the dar checkout root")
}

fn agent_selection(agent: &Path) -> Result<AgentSelection> {
    let path = agent.join("agent.yaml");
    let raw = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_yaml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

fn tracker_package(use_: &str) -> Result<&'static str> {
    match use_ {
        "files" => Ok("tracker-files"),
        "linear" => Ok("tracker-linear"),
        other => bail!("unknown tracker.use {other:?}"),
    }
}

fn runner_package(use_: &str) -> Result<&'static str> {
    match use_ {
        "pi" => Ok("runner-pi"),
        "codex" => Ok("runner-codex"),
        "opencode" => Ok("runner-opencode"),
        "cli" => Ok("runner-cli"),
        "fake" => Ok("runner-fake"),
        "builtin" => Ok("runner-builtin"),
        other => bail!("unknown runner.use {other:?}"),
    }
}

const ALL_CHATS: &[&str] = &["chat-pi", "chat-codex", "chat-opencode"];

fn foreground_packages(selection: &AgentSelection) -> Result<Vec<&'static str>> {
    match selection.foreground.as_str() {
        "logs" => Ok(vec!["frontend-log"]),
        "tui" => {
            let mut packages = vec!["frontend-log", "tui"];
            packages.extend_from_slice(ALL_CHATS);
            Ok(packages)
        }
        other => bail!("unknown foreground {other:?}"),
    }
}

fn default_foreground() -> String {
    "logs".to_string()
}

pub fn build(agent: &Path) -> Result<()> {
    build_with_options(agent, BuildOptions::default())
}

pub fn build_with_options(agent: &Path, options: BuildOptions) -> Result<()> {
    let (crate_dir, _) = write_composition_crate(agent)?;
    validate_build_options(&options)?;
    if !crate_dir.join("Cargo.lock").exists() {
        refresh_lockfile(&crate_dir, false)?;
    }
    if options.vendor {
        vendor_dependencies(&crate_dir)?;
    }
    let agent = agent
        .canonicalize()
        .with_context(|| format!("resolving agent folder {}", agent.display()))?;
    if options.universal {
        return build_universal(&crate_dir, &agent, options.offline);
    }
    let target = build_target(&options)?;
    if options.static_ {
        crate::doctor::check_static_build_prereqs(
            target.as_deref().context("static build target missing")?,
        )?;
    }
    let mut args = vec![
        "build".to_string(),
        "--release".to_string(),
        "--locked".to_string(),
    ];
    if options.offline {
        args.push("--offline".to_string());
    }
    if let Some(target) = target.as_deref() {
        args.push("--target".to_string());
        args.push(target.to_string());
    }
    run_cargo(&crate_dir, &args)?;
    fs::create_dir_all(agent.join("bin"))
        .with_context(|| format!("creating {}", agent.join("bin").display()))?;
    let binary = built_binary_path(&crate_dir, target.as_deref());
    fs::copy(&binary, agent.join("bin").join(binary_name("dar")))
        .with_context(|| format!("copying {}", binary.display()))?;
    Ok(())
}

pub fn build_to(agent: &Path, dest: &Path) -> Result<()> {
    build_to_with_options(agent, dest, BuildOptions::default())
}

pub fn build_to_with_options(agent: &Path, dest: &Path, options: BuildOptions) -> Result<()> {
    validate_build_options(&options)?;
    let agent = agent
        .canonicalize()
        .with_context(|| format!("resolving agent folder {}", agent.display()))?;
    let crate_dir = agent.join(".dar");
    if options.vendor {
        vendor_dependencies(&crate_dir)?;
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    if options.universal {
        build_universal_to(&crate_dir, dest, options.offline)?;
        return Ok(());
    }
    let target = build_target(&options)?;
    if options.static_ {
        crate::doctor::check_static_build_prereqs(
            target.as_deref().context("static build target missing")?,
        )?;
    }
    let mut args = vec![
        "build".to_string(),
        "--release".to_string(),
        "--locked".to_string(),
    ];
    if options.offline {
        args.push("--offline".to_string());
    }
    if let Some(target) = target.as_deref() {
        args.push("--target".to_string());
        args.push(target.to_string());
    }
    run_cargo_with_stderr(&crate_dir, args)?;
    let binary = built_binary_path(&crate_dir, target.as_deref());
    fs::copy(&binary, dest).with_context(|| {
        format!(
            "copying built dar binary from {} to {}",
            binary.display(),
            dest.display()
        )
    })?;
    Ok(())
}

fn validate_build_options(options: &BuildOptions) -> Result<()> {
    if options.universal && (options.static_ || options.target.is_some()) {
        bail!("--universal cannot be combined with --static or --target");
    }
    Ok(())
}

fn build_target(options: &BuildOptions) -> Result<Option<String>> {
    if let Some(target) = &options.target {
        return Ok(Some(target.to_string()));
    }
    if options.static_ {
        return Ok(Some(default_static_target()?));
    }
    Ok(None)
}

fn default_static_target() -> Result<String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl".to_string()),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-musl".to_string()),
        _ => bail!("--static is supported only on Linux x86_64/aarch64; use --target <musl-triple> on supported Linux hosts"),
    }
}

fn build_universal(crate_dir: &Path, agent: &Path, offline: bool) -> Result<()> {
    fs::create_dir_all(agent.join("bin"))
        .with_context(|| format!("creating {}", agent.join("bin").display()))?;
    build_universal_to(
        crate_dir,
        &agent.join("bin").join(binary_name("dar")),
        offline,
    )
}

fn build_universal_to(crate_dir: &Path, dest: &Path, offline: bool) -> Result<()> {
    if std::env::consts::OS != "macos" {
        bail!("--universal is supported only on macOS");
    }
    for target in ["aarch64-apple-darwin", "x86_64-apple-darwin"] {
        let mut args = vec![
            "build".to_string(),
            "--release".to_string(),
            "--locked".to_string(),
            "--target".to_string(),
            target.to_string(),
        ];
        if offline {
            args.push("--offline".to_string());
        }
        run_cargo_with_stderr(crate_dir, args)?;
    }
    let status = Command::new("lipo")
        .arg("-create")
        .arg("-output")
        .arg(dest)
        .arg(built_binary_path(crate_dir, Some("aarch64-apple-darwin")))
        .arg(built_binary_path(crate_dir, Some("x86_64-apple-darwin")))
        .status()
        .context("running lipo")?;
    if !status.success() {
        bail!("lipo exited with {status}");
    }
    Ok(())
}

pub fn lock_refresh(agent: &Path) -> Result<()> {
    let (crate_dir, _) = write_composition_crate(agent)?;
    run_cargo(&crate_dir, &["update"])
}

fn run_cargo_with_stderr<I, S>(crate_dir: &Path, args: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new("cargo")
        .args(args)
        .current_dir(crate_dir)
        .output()
        .with_context(|| format!("running cargo in {}", crate_dir.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprint!("{stderr}");
        bail!("cargo exited with {}", output.status);
    }
    Ok(())
}

fn discover_extensions(agent: &Path, source_root: &Path) -> Result<Vec<LocalExtension>> {
    let extensions_dir = agent.join("extensions");
    if !extensions_dir.exists() {
        return Ok(Vec::new());
    }
    let mut extensions = Vec::new();
    for entry in fs::read_dir(&extensions_dir)
        .with_context(|| format!("reading {}", extensions_dir.display()))?
    {
        let entry = entry?;
        let manifest_path = entry.path().join("Cargo.toml");
        if !manifest_path.is_file() {
            continue;
        }
        let manifest = fs::read_to_string(&manifest_path)
            .with_context(|| format!("reading {}", manifest_path.display()))?;
        let value = manifest
            .parse::<toml::Value>()
            .with_context(|| format!("parsing {}", manifest_path.display()))?;
        validate_patched_registry_deps(&manifest_path, &value, source_root)?;
        let package = value
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(toml::Value::as_str)
            .context("extension manifest missing [package] name")?
            .to_string();
        let meta = value.get("package").and_then(|p| p.get("metadata"));
        let factory = meta
            .and_then(|m| m.get("dar"))
            .and_then(|a| a.get("factory"))
            .and_then(toml::Value::as_str);
        let Some(factory) = factory else {
            continue;
        };
        let requires_stock = meta
            .and_then(|m| m.get("dar"))
            .and_then(|a| a.get("requires_stock"))
            .and_then(toml::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        item.as_str()
                            .map(str::to_string)
                            .context("package.metadata.dar.requires_stock entries must be strings")
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?
            .unwrap_or_default();
        extensions.push(LocalExtension {
            package,
            factory: factory.to_string(),
            path: PathBuf::from("../extensions").join(entry.file_name()),
            requires_stock,
        });
    }
    extensions.sort_by(|a, b| a.package.cmp(&b.package));
    Ok(extensions)
}

fn validate_patched_registry_deps(
    manifest_path: &Path,
    manifest: &toml::Value,
    source_root: &Path,
) -> Result<()> {
    let local_version = Version::parse(LOCAL_CRATE_VERSION)
        .with_context(|| format!("parsing local dar version {LOCAL_CRATE_VERSION}"))?;
    for (dep_name, req) in registry_dar_dependency_reqs(manifest) {
        if VersionReq::parse(&req)
            .with_context(|| {
                format!(
                    "parsing dependency requirement {dep_name} = {req:?} in {}",
                    manifest_path.display()
                )
            })?
            .matches(&local_version)
        {
            continue;
        }
        bail!(
            "{} depends on {dep_name} {req:?}, but local dar checkout {} provides {LOCAL_CRATE_VERSION}; use a matching dar checkout or update the extension dependency",
            manifest_path.display(),
            source_root.display()
        );
    }
    Ok(())
}

fn registry_dar_dependency_reqs(manifest: &toml::Value) -> Vec<(String, String)> {
    let mut reqs = Vec::new();
    collect_registry_dar_dependency_reqs(manifest.get("dependencies"), &mut reqs);
    collect_registry_dar_dependency_reqs(manifest.get("dev-dependencies"), &mut reqs);
    collect_registry_dar_dependency_reqs(manifest.get("build-dependencies"), &mut reqs);
    if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
        for target in targets.values() {
            collect_registry_dar_dependency_reqs(target.get("dependencies"), &mut reqs);
            collect_registry_dar_dependency_reqs(target.get("dev-dependencies"), &mut reqs);
            collect_registry_dar_dependency_reqs(target.get("build-dependencies"), &mut reqs);
        }
    }
    reqs
}

fn collect_registry_dar_dependency_reqs(
    section: Option<&toml::Value>,
    reqs: &mut Vec<(String, String)>,
) {
    let Some(deps) = section.and_then(toml::Value::as_table) else {
        return;
    };
    for (key, dep) in deps {
        match dep {
            toml::Value::String(req) if is_patched_registry_crate(key) => {
                reqs.push((key.clone(), req.clone()));
            }
            toml::Value::Table(table) => {
                if table.contains_key("path") || table.contains_key("git") {
                    continue;
                }
                let package = table
                    .get("package")
                    .and_then(toml::Value::as_str)
                    .unwrap_or(key);
                if let Some(req) = table
                    .get("version")
                    .and_then(toml::Value::as_str)
                    .filter(|_| is_patched_registry_crate(package))
                {
                    reqs.push((package.to_string(), req.to_string()));
                }
            }
            _ => {}
        }
    }
}

fn is_patched_registry_crate(name: &str) -> bool {
    PATCHED_REGISTRY_CRATES
        .iter()
        .any(|(crate_name, _)| *crate_name == name)
}

fn cargo_toml(
    _crate_dir: &Path,
    stock: &[&StockExtension],
    locals: &[LocalExtension],
    source_root: &Path,
) -> String {
    let mut out = String::from(GENERATED_TOML_HEADER);
    out.push_str(
        r#"[package]
name = "dar-agent"
version = "0.1.0"
edition = "2021"
rust-version = "1.83"

[[bin]]
name = "dar"
path = "src/main.rs"

[workspace]

[dependencies]
"#,
    );
    stock_dependency(
        &mut out,
        "host-api",
        "crates/host-api",
        STOCK_CRATE_VERSION_REQ,
        false,
        source_root,
    );
    stock_dependency(
        &mut out,
        "dar-cli-core",
        "crates/dar-cli",
        STOCK_CRATE_VERSION_REQ,
        false,
        source_root,
    );
    out.push_str("tokio = { version = \"1.43\", features = [\"rt-multi-thread\", \"macros\", \"signal\"] }\n");
    for stock in stock {
        stock_dependency(
            &mut out,
            stock.package,
            &format!("extensions/{}", stock.package),
            STOCK_CRATE_VERSION_REQ,
            true,
            source_root,
        );
    }
    for local in locals {
        out.push_str(&format!(
            "{} = {{ path = \"{}\" }}\n",
            local.package,
            toml_path(&local.path)
        ));
    }
    patch_crates_io(&mut out, source_root);
    out.push_str("\n[features]\n");
    let default_features = stock
        .iter()
        .map(|stock| format!("\"{}\"", stock.feature()))
        .collect::<Vec<_>>()
        .join(", ");
    out.push_str(&format!("default = [{default_features}]\n"));
    for stock in stock {
        out.push_str(&format!(
            "{} = [\"dep:{}\"]\n",
            stock.feature(),
            stock.package
        ));
    }
    out
}

fn patch_crates_io(out: &mut String, source_root: &Path) {
    out.push_str("\n[patch.crates-io]\n");
    for (package, rel_path) in PATCHED_REGISTRY_CRATES {
        let abs = source_root.join(rel_path);
        let abs = abs.canonicalize().unwrap_or(abs);
        out.push_str(&format!(
            "{package} = {{ path = \"{}\" }}\n",
            toml_path(&abs)
        ));
    }
}

fn stock_package(key: &str) -> String {
    if key == "dar-cli-core" {
        "dar-cli-core".into()
    } else {
        format!("dar-{key}")
    }
}

fn stock_dependency(
    out: &mut String,
    key: &str,
    rel_path: &str,
    version: &str,
    optional: bool,
    source_root: &Path,
) {
    let pkg = stock_package(key);
    let abs = source_root.join(rel_path);
    let abs = abs.canonicalize().unwrap_or(abs);
    let abs = toml_path(&abs);
    let optional = if optional { ", optional = true" } else { "" };
    out.push_str(&format!(
        "{key} = {{ package = \"{pkg}\", version = \"{version}\", path = \"{abs}\"{optional} }}\n"
    ));
}

fn main_rs(stock: &[&StockExtension], locals: &[LocalExtension]) -> String {
    let mut out = String::from(GENERATED_RUST_HEADER);
    out.push_str(
        r#"#[tokio::main(worker_threads = 2)]
async fn main() {
    let mut plugins: Vec<std::sync::Arc<dyn host_api::Extension>> = Vec::new();
"#,
    );
    for stock in stock {
        out.push_str(&format!("    #[cfg(feature = \"{}\")]\n", stock.feature()));
        out.push_str(&format!(
            "    plugins.push(std::sync::Arc::new({}) as std::sync::Arc<dyn host_api::Extension>);\n",
            stock.factory
        ));
    }
    for local in locals {
        out.push_str(&format!(
            "    plugins.push(std::sync::Arc::new({}()) as std::sync::Arc<dyn host_api::Extension>);\n",
            local.factory
        ));
    }
    out.push_str("    plugins.shrink_to_fit();\n    dar_cli_core::run(plugins).await\n}\n");
    out
}

fn refresh_lockfile(crate_dir: &Path, offline: bool) -> Result<()> {
    let mut args = vec!["generate-lockfile"];
    if offline {
        args.push("--offline");
    }
    run_cargo(crate_dir, &args)
}

fn vendor_dependencies(crate_dir: &Path) -> Result<()> {
    let args = ["vendor", "vendor", "--locked"];
    let output = Command::new("cargo")
        .args(args)
        .current_dir(crate_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("running cargo vendor in {}", crate_dir.display()))?;
    if !output.status.success() {
        bail!("cargo vendor exited with {}", output.status);
    }
    fs::create_dir_all(crate_dir.join(".cargo"))
        .with_context(|| format!("creating {}", crate_dir.join(".cargo").display()))?;
    fs::write(crate_dir.join(".cargo/config.toml"), output.stdout)
        .with_context(|| format!("writing {}", crate_dir.join(".cargo/config.toml").display()))?;
    Ok(())
}

fn run_cargo<I, S>(crate_dir: &Path, args: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let status = Command::new("cargo")
        .args(args)
        .current_dir(crate_dir)
        .status()
        .with_context(|| format!("running cargo in {}", crate_dir.display()))?;
    if !status.success() {
        bail!("cargo exited with {status}");
    }
    Ok(())
}

/// Ensure the agent folder's `.gitignore` contains every standard entry.
///
/// - Creates the file with the full standard block when it is absent.
/// - Otherwise appends only the standard entries that are missing, preserving
///   all pre-existing lines (including user-added ones) verbatim. Appended
///   entries keep their comment headers, but a header is only emitted when at
///   least one entry under it is actually being added.
/// - Idempotent: re-running adds nothing once all entries are present.
///
/// Membership is decided on non-comment, non-blank lines, so `.env` already
/// added by `tracker-linear`'s `init_workflow` is never duplicated here.
pub(crate) fn ensure_agent_gitignore(agent: &Path) -> Result<bool> {
    let path = agent.join(".gitignore");
    let existing = match fs::read_to_string(&path) {
        Ok(contents) => Some(contents),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };

    let Some(existing) = existing else {
        fs::write(&path, STANDARD_GITIGNORE)
            .with_context(|| format!("writing {}", path.display()))?;
        return Ok(true);
    };

    let present: std::collections::HashSet<&str> = existing
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();

    let mut additions = String::new();
    let mut header: Option<&str> = None;
    let mut header_written = false;
    for line in STANDARD_GITIGNORE.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('#') {
            header = Some(line);
            header_written = false;
            continue;
        }
        if present.contains(trimmed) {
            continue;
        }
        if !header_written {
            if let Some(header) = header {
                if !additions.is_empty() {
                    additions.push('\n');
                }
                additions.push_str(header);
                additions.push('\n');
            }
            header_written = true;
        }
        additions.push_str(line);
        additions.push('\n');
    }

    if additions.is_empty() {
        return Ok(false);
    }

    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push('\n');
    next.push_str(&additions);
    fs::write(&path, next).with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

fn write_if_changed(path: &Path, content: &str) -> Result<bool> {
    if matches!(fs::read_to_string(path), Ok(existing) if existing == content) {
        return Ok(false);
    }
    fs::write(path, content).with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

fn toml_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn binary_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn built_binary_path(crate_dir: &Path, target: Option<&str>) -> PathBuf {
    let target_dir = crate_dir.join("target");
    match target {
        Some(target) => target_dir
            .join(target)
            .join("release")
            .join(binary_name("dar")),
        None => target_dir.join("release").join(binary_name("dar")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_discovers_extensions_and_generates_stable_crate_files() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path();
        write_test_agent_yaml(agent);
        write_test_extension(agent);

        init_build(agent).unwrap();

        let manifest = std::fs::read_to_string(agent.join(".dar/Cargo.toml")).unwrap();
        let source = std::fs::read_to_string(agent.join(".dar/src/main.rs")).unwrap();
        manifest.parse::<toml::Value>().unwrap();
        assert!(manifest.starts_with("# generated - do not hand-edit\n"));
        assert!(manifest.contains("\n[workspace]\n"));
        assert!(manifest.contains("host-api = { package = \"dar-host-api\", version = "));
        assert!(manifest.contains("version = \"0."));
        assert!(manifest.contains("my-ext = { path = \"../extensions/my-ext\" }"));
        assert!(manifest.contains("\n[patch.crates-io]\n"));
        for (package, _) in PATCHED_REGISTRY_CRATES {
            assert!(
                manifest.contains(&format!("{package} = {{ path = ")),
                "missing patch for {package}: {manifest}"
            );
        }
        assert!(source.starts_with("// # generated - do not hand-edit\n"));
        assert!(source.contains("my_ext::extension()"));

        let manifest_before = manifest;
        let source_before = source;
        init_build(agent).unwrap();

        assert_eq!(
            manifest_before,
            std::fs::read_to_string(agent.join(".dar/Cargo.toml")).unwrap()
        );
        assert_eq!(
            source_before,
            std::fs::read_to_string(agent.join(".dar/src/main.rs")).unwrap()
        );
    }

    #[test]
    fn compose_accepts_registry_sdk_dep_when_version_matches() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path();
        write_test_agent_yaml(agent);
        write_test_extension_with_dependency(agent, "dar-extension-sdk = \"0.3\"");

        compose(agent).unwrap();

        let manifest = std::fs::read_to_string(agent.join(".dar/Cargo.toml")).unwrap();
        assert!(manifest.contains("\n[patch.crates-io]\n"));
        assert!(manifest.contains("dar-extension-sdk = { path = "));
    }

    #[test]
    fn compose_rejects_registry_sdk_dep_when_version_mismatches() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path();
        write_test_agent_yaml(agent);
        write_test_extension_with_dependency(agent, "dar-extension-sdk = \"=0.2.0\"");

        let err = compose(agent).unwrap_err();

        let message = format!("{err:#}");
        assert!(message.contains("depends on dar-extension-sdk \"=0.2.0\""));
        assert!(message.contains(&format!("provides {LOCAL_CRATE_VERSION}")));
    }

    #[test]
    fn compose_validates_renamed_registry_sdk_dep() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path();
        write_test_agent_yaml(agent);
        write_test_extension_with_dependency(
            agent,
            "sdk = { package = \"dar-extension-sdk\", version = \"0.3\" }",
        );

        compose(agent).unwrap();
    }

    #[test]
    fn compose_ignores_path_sdk_dep_version_for_local_extensions() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path();
        write_test_agent_yaml(agent);
        let sdk = toml_path(&dar_source_root().unwrap().join("crates/extension-sdk"));
        write_test_extension_with_dependency(
            agent,
            &format!("dar-extension-sdk = {{ version = \"=0.2.0\", path = \"{sdk}\" }}"),
        );

        compose(agent).unwrap();
    }

    #[test]
    fn build_compiles_generated_crate_and_copies_agent_binary() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path();
        write_test_agent_yaml(agent);
        write_test_extension(agent);

        build(agent).unwrap();

        assert!(agent.join("bin").join(binary_name("dar")).is_file());
    }

    #[test]
    fn init_build_refreshes_lockfile_when_manifest_changes() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path();
        write_test_agent_yaml(agent);

        init_build(agent).unwrap();
        let original_lock = std::fs::read_to_string(agent.join(".dar/Cargo.lock")).unwrap();

        write_test_extension(agent);
        init_build(agent).unwrap();

        let manifest = std::fs::read_to_string(agent.join(".dar/Cargo.toml")).unwrap();
        let refreshed_lock = std::fs::read_to_string(agent.join(".dar/Cargo.lock")).unwrap();
        assert!(manifest.contains("my-ext = { path = \"../extensions/my-ext\" }"));
        assert_ne!(original_lock, refreshed_lock);
        assert!(refreshed_lock.contains("name = \"my-ext\""));
    }

    #[test]
    fn build_does_not_refresh_stale_lockfile() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path();
        write_test_agent_yaml(agent);

        init_build(agent).unwrap();
        write_test_extension(agent);

        let err = build(agent).unwrap_err();

        assert!(
            err.to_string().contains("cargo exited"),
            "unexpected error: {err:#}"
        );
        let lock = std::fs::read_to_string(agent.join(".dar/Cargo.lock")).unwrap();
        assert!(!lock.contains("name = \"my-ext\""));
    }

    #[test]
    fn target_build_reads_binary_from_target_specific_release_dir() {
        let temp = tempfile::tempdir().unwrap();
        let crate_dir = temp.path().join(".dar");
        let target = "x86_64-unknown-linux-musl";

        assert_eq!(
            built_binary_path(&crate_dir, Some(target)),
            crate_dir
                .join("target")
                .join(target)
                .join("release")
                .join(binary_name("dar"))
        );
    }

    #[test]
    fn host_build_reads_binary_from_default_release_dir() {
        let temp = tempfile::tempdir().unwrap();
        let crate_dir = temp.path().join(".dar");

        assert_eq!(
            built_binary_path(&crate_dir, None),
            crate_dir
                .join("target")
                .join("release")
                .join(binary_name("dar"))
        );
    }

    #[test]
    fn vendor_writes_cargo_config_and_supports_offline_builds() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path();
        write_test_agent_yaml(agent);

        init_build_with_options(
            agent,
            BuildOptions {
                vendor: true,
                offline: false,
                ..BuildOptions::default()
            },
        )
        .unwrap();

        assert!(agent.join(".dar/vendor").is_dir());
        let config = std::fs::read_to_string(agent.join(".dar/.cargo/config.toml")).unwrap();
        assert!(config.contains("vendored-sources"));
        assert!(config.contains("directory = \"vendor\""));
        run_cargo(
            &agent.join(".dar"),
            &["build", "--release", "--locked", "--offline"],
        )
        .unwrap();
    }

    #[test]
    fn lock_refresh_regenerates_and_updates_existing_composition_lockfile() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path();
        write_test_agent_yaml(agent);

        init_build(agent).unwrap();
        write_test_extension(agent);

        lock_refresh(agent).unwrap();

        let manifest = std::fs::read_to_string(agent.join(".dar/Cargo.toml")).unwrap();
        let lock = std::fs::read_to_string(agent.join(".dar/Cargo.lock")).unwrap();
        assert!(manifest.contains("my-ext = { path = \"../extensions/my-ext\" }"));
        assert!(lock.contains("name = \"my-ext\""));
    }

    #[test]
    fn init_build_feature_gates_stock_extensions_from_agent_yaml() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path();
        write_agent_yaml(agent, "files", "fake", "logs", "");

        init_build(agent).unwrap();

        let manifest = std::fs::read_to_string(agent.join(".dar/Cargo.toml")).unwrap();
        let source = std::fs::read_to_string(agent.join(".dar/src/main.rs")).unwrap();
        assert!(manifest.contains("tracker-files = { package = \"dar-tracker-files\", version = "));
        assert!(manifest.contains("optional = true"));
        assert!(manifest.contains("[features]\ndefault = ["));
        assert!(manifest.contains("stock-tracker-files = [\"dep:tracker-files\"]"));
        assert!(manifest.contains("stock-runner-fake = [\"dep:runner-fake\"]"));
        assert!(manifest.contains("stock-orchestrator = [\"dep:orchestrator\"]"));
        assert!(
            manifest.contains("system-context = { package = \"dar-system-context\", version = ")
        );
        assert!(manifest.contains("stock-system-context = [\"dep:system-context\"]"));
        assert!(manifest.contains("stock-frontend-log = [\"dep:frontend-log\"]"));
        assert!(
            manifest.contains("tracker-linear = { package = \"dar-tracker-linear\", version = ")
        );
        assert!(manifest.contains("stock-tracker-linear = [\"dep:tracker-linear\"]"));
        assert!(source.contains("system_context::SystemContextExtension"));
        assert!(source.contains("tracker_linear::TrackerLinearExtension"));
        assert!(source.contains("#[cfg(feature = \"stock-tracker-files\")]"));
        assert!(source.contains("tracker_files::TrackerFilesExtension"));
    }

    #[test]
    fn init_build_includes_tui_provider_closure() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path();
        write_agent_yaml(agent, "files", "fake", "tui", "");

        init_build(agent).unwrap();

        let manifest = std::fs::read_to_string(agent.join(".dar/Cargo.toml")).unwrap();
        let source = std::fs::read_to_string(agent.join(".dar/src/main.rs")).unwrap();
        for package in ["frontend-log", "chat-pi", "tui"] {
            assert!(
                manifest.contains(&format!("{package} = {{ package = ")),
                "{package} should be linked for foreground: tui"
            );
        }
        assert!(source.contains("frontend_log::FrontendLogExtension"));
        assert!(source.contains("chat_pi::ChatPiExtension"));
        assert!(source.contains("tui::TuiExtension"));
    }

    #[test]
    fn init_build_includes_tui_chat_backend_for_followed_runner() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path();
        write_agent_yaml(agent, "files", "codex", "tui", "");

        init_build(agent).unwrap();

        let manifest = std::fs::read_to_string(agent.join(".dar/Cargo.toml")).unwrap();
        let source = std::fs::read_to_string(agent.join(".dar/src/main.rs")).unwrap();
        assert!(manifest.contains("chat-codex = { package = \"dar-chat-codex\", version = "));
        assert!(manifest.contains("chat-pi = { package = \"dar-chat-pi\", version = "));
        assert!(manifest.contains("runner-codex = { package = \"dar-runner-codex\", version = "));
        assert!(manifest.contains("stock-chat-codex = [\"dep:chat-codex\"]"));
        assert!(source.contains("chat_codex::ChatCodexExtension"));
        assert!(source.contains("chat_pi::ChatPiExtension"));
    }

    #[test]
    fn init_build_includes_explicit_tui_chat_backend() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path();
        write_agent_yaml(
            agent,
            "files",
            "fake",
            "tui",
            r#"
extensions:
  tui:
    chat:
      backend: opencode
"#,
        );

        init_build(agent).unwrap();

        let manifest = std::fs::read_to_string(agent.join(".dar/Cargo.toml")).unwrap();
        let source = std::fs::read_to_string(agent.join(".dar/src/main.rs")).unwrap();
        assert!(manifest.contains("chat-opencode = { package = \"dar-chat-opencode\", version = "));
        assert!(manifest.contains("chat-pi = { package = \"dar-chat-pi\", version = "));
        assert!(manifest.contains("runner-fake = { package = \"dar-runner-fake\", version = "));
        assert!(manifest.contains("stock-chat-opencode = [\"dep:chat-opencode\"]"));
        assert!(source.contains("chat_opencode::ChatOpenCodeExtension"));
        assert!(source.contains("chat_pi::ChatPiExtension"));
    }

    #[test]
    fn init_build_allows_explicit_custom_tui_chat_backend() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path();
        write_agent_yaml(
            agent,
            "files",
            "fake",
            "tui",
            r#"
extensions:
  tui:
    chat:
      backend: my-chat
"#,
        );

        init_build(agent).unwrap();

        let manifest = std::fs::read_to_string(agent.join(".dar/Cargo.toml")).unwrap();
        assert!(manifest.contains("chat-pi = { package = \"dar-chat-pi\", version = "));
        assert!(manifest.contains("chat-codex = { package = \"dar-chat-codex\", version = "));
        assert!(manifest.contains("chat-opencode = { package = \"dar-chat-opencode\", version = "));
    }

    #[test]
    fn init_build_always_includes_all_runners() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path();
        write_agent_yaml(agent, "files", "fake", "logs", "");

        init_build(agent).unwrap();

        let manifest = std::fs::read_to_string(agent.join(".dar/Cargo.toml")).unwrap();
        for pkg in [
            "runner-pi",
            "runner-codex",
            "runner-opencode",
            "runner-cli",
            "runner-fake",
            "runner-builtin",
        ] {
            assert!(
                manifest.contains(&format!("{pkg} = {{ package = ")),
                "{pkg} should always be linked regardless of runner.use"
            );
        }
    }

    #[test]
    fn init_build_feature_gates_builtin_runner_dependency() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path();
        write_agent_yaml(agent, "files", "builtin", "logs", "");

        init_build(agent).unwrap();

        let manifest = std::fs::read_to_string(agent.join(".dar/Cargo.toml")).unwrap();
        let source = std::fs::read_to_string(agent.join(".dar/src/main.rs")).unwrap();
        assert!(
            manifest.contains("runner-builtin = { package = \"dar-runner-builtin\", version = ")
        );
        assert!(manifest.contains("optional = true"));
        assert!(manifest.contains("stock-runner-builtin = [\"dep:runner-builtin\"]"));
        assert!(source.contains("#[cfg(feature = \"stock-runner-builtin\")]"));
        assert!(source.contains("runner_builtin::RunnerBuiltinExtension"));
    }

    #[test]
    fn init_build_always_includes_dashboard() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path();
        write_agent_yaml(agent, "files", "fake", "logs", "");

        init_build(agent).unwrap();

        let manifest = std::fs::read_to_string(agent.join(".dar/Cargo.toml")).unwrap();
        assert!(
            manifest.contains("dashboard = { package = \"dar-dashboard\", version = "),
            "dashboard is baseline and must always be linked (presence registry)"
        );
        let main = std::fs::read_to_string(agent.join(".dar/src/main.rs")).unwrap();
        assert!(
            main.contains("dashboard::DashboardExtension::default()"),
            "dashboard extension must be in the generated plugins! list"
        );
    }

    #[test]
    fn scheduler_absent_from_agent_yaml_is_not_linked() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path();
        write_agent_yaml(agent, "files", "fake", "logs", "");

        compose(agent).unwrap();

        let manifest = std::fs::read_to_string(agent.join(".dar/Cargo.toml")).unwrap();
        let main = std::fs::read_to_string(agent.join(".dar/src/main.rs")).unwrap();
        assert!(
            !manifest.contains("scheduler = { package = "),
            "scheduler must be opt-in: absent agent.yaml section → not linked"
        );
        assert!(!main.contains("scheduler::SchedulerExtension"));
    }

    #[test]
    fn scheduler_selected_when_present_in_agent_yaml() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path();
        write_agent_yaml(
            agent,
            "files",
            "fake",
            "logs",
            "extensions:\n  scheduler: {}\n",
        );

        compose(agent).unwrap();

        let manifest = std::fs::read_to_string(agent.join(".dar/Cargo.toml")).unwrap();
        let main = std::fs::read_to_string(agent.join(".dar/src/main.rs")).unwrap();
        assert!(manifest.contains("scheduler = { package = \"dar-scheduler\", version = "));
        assert!(manifest.contains("stock-scheduler = [\"dep:scheduler\"]"));
        assert!(main.contains("#[cfg(feature = \"stock-scheduler\")]"));
        assert!(main.contains("scheduler::SchedulerExtension"));
        let registry_pos = main
            .find("tool_registry_host::ToolRegistryHostExtension")
            .expect("tool registry host must be generated");
        let scheduler_pos = main
            .find("scheduler::SchedulerExtension::default()")
            .expect("scheduler must be generated");
        assert!(
            registry_pos < scheduler_pos,
            "tool registry host must register before scheduler tools: {main}"
        );
    }

    #[test]
    fn scheduler_disabled_flag_still_links_for_kill_switch() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path();
        write_agent_yaml(
            agent,
            "files",
            "fake",
            "logs",
            "extensions:\n  scheduler:\n    enabled: false\n",
        );

        compose(agent).unwrap();

        // enabled:false is a runtime kill switch, not a build-time exclusion:
        // the extension still links and loads, then stays idle at runtime.
        let main = std::fs::read_to_string(agent.join(".dar/src/main.rs")).unwrap();
        assert!(main.contains("scheduler::SchedulerExtension"));
    }

    #[test]
    fn toml_paths_use_forward_slashes() {
        assert_eq!(
            toml_path(Path::new(r"..\extensions\my-ext")),
            "../extensions/my-ext"
        );
    }

    fn write_test_extension(agent: &Path) {
        write_test_extension_with_metadata(
            agent,
            r#"[package.metadata.dar]
factory = "my_ext::extension"
"#,
        );
    }

    fn write_test_extension_with_dependency(agent: &Path, dependency: &str) {
        write_test_extension_manifest(agent, dependency);
    }

    fn write_test_extension_with_metadata(agent: &Path, metadata: &str) {
        // Stock crates are `publish = false`, so the local extension's
        // host-api dep must resolve by path into the dar source checkout
        // (the same mechanism the composer emits for stock deps).
        let host_api = toml_path(&dar_source_root().unwrap().join("crates/host-api"));
        write_test_extension_manifest(
            agent,
            &format!(
                "host-api = {{ package = \"dar-host-api\", version = \"{STOCK_CRATE_VERSION_REQ}\", path = \"{host_api}\" }}"
            ),
        );
        let manifest = agent.join("extensions/my-ext/Cargo.toml");
        let mut contents = std::fs::read_to_string(&manifest).unwrap();
        contents = contents.replace(
            "[package.metadata.dar]\nfactory = \"my_ext::extension\"",
            metadata.trim_end(),
        );
        std::fs::write(manifest, contents).unwrap();
    }

    fn write_test_extension_manifest(agent: &Path, dependency: &str) {
        let extension = agent.join("extensions/my-ext");
        std::fs::create_dir_all(extension.join("src")).unwrap();
        std::fs::write(
            extension.join("Cargo.toml"),
            format!(
                r#"[package]
name = "my-ext"
version = "0.1.0"
edition = "2021"

[package.metadata.dar]
factory = "my_ext::extension"

[dependencies]
{dependency}
"#
            ),
        )
        .unwrap();
        std::fs::write(
            extension.join("src/lib.rs"),
            r#"use host_api::Extension;

pub fn extension() -> Box<dyn Extension> {
    Box::new(MyExt)
}

struct MyExt;

impl Extension for MyExt {
    fn id(&self) -> &'static str {
        "my-ext"
    }
}
"#,
        )
        .unwrap();
    }

    #[test]
    fn local_extension_can_require_chat_pi_under_logs_foreground() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path();
        write_agent_yaml(agent, "files", "fake", "logs", "");
        write_test_extension_with_metadata(
            agent,
            r#"[package.metadata.dar]
factory = "my_ext::extension"
requires_stock = ["chat-pi"]
"#,
        );

        compose(agent).unwrap();

        let manifest = std::fs::read_to_string(agent.join(".dar/Cargo.toml")).unwrap();
        let source = std::fs::read_to_string(agent.join(".dar/src/main.rs")).unwrap();
        assert!(manifest.contains("chat-pi = { package = \"dar-chat-pi\", version = "));
        assert!(source.contains("chat_pi::ChatPiExtension"));
    }

    #[test]
    fn init_build_creates_full_gitignore_when_absent() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path();
        write_test_agent_yaml(agent);

        compose(agent).unwrap();

        let contents = std::fs::read_to_string(agent.join(".gitignore")).unwrap();
        assert_eq!(contents, STANDARD_GITIGNORE);
        for entry in [
            ".env",
            "/bin/",
            "/.dar/target/",
            "/data/",
            "/logs/",
            "/workspaces/",
            "/claude-sessions/",
            "/opencode-sessions/",
            "/pi-sessions/",
        ] {
            assert!(
                contents.lines().any(|line| line == entry),
                "missing entry {entry}"
            );
        }
    }

    #[test]
    fn init_build_appends_missing_entries_and_preserves_user_lines() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path();
        write_test_agent_yaml(agent);
        // Pre-existing file with a user line plus `.env` already added (mirrors
        // tracker-linear's init_workflow behavior).
        std::fs::write(agent.join(".gitignore"), "my-secret-notes.txt\n.env\n").unwrap();

        compose(agent).unwrap();

        let contents = std::fs::read_to_string(agent.join(".gitignore")).unwrap();
        // User line untouched and still first.
        assert!(contents.starts_with("my-secret-notes.txt\n.env\n"));
        // All standard entries present exactly once (`.env` not re-added).
        for entry in [
            ".env",
            "/bin/",
            "/.dar/target/",
            "/data/",
            "/logs/",
            "/workspaces/",
            "/claude-sessions/",
            "/opencode-sessions/",
            "/pi-sessions/",
        ] {
            assert_eq!(
                contents.lines().filter(|line| *line == entry).count(),
                1,
                "entry {entry} should appear exactly once"
            );
        }
    }

    #[test]
    fn init_build_gitignore_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path();
        write_test_agent_yaml(agent);

        compose(agent).unwrap();
        let first = std::fs::read_to_string(agent.join(".gitignore")).unwrap();
        compose(agent).unwrap();
        let second = std::fs::read_to_string(agent.join(".gitignore")).unwrap();

        assert_eq!(first, second, "re-running must not change .gitignore");
        for entry in [".env", "/bin/", "/data/", "/pi-sessions/"] {
            assert_eq!(
                second.lines().filter(|line| *line == entry).count(),
                1,
                "entry {entry} duplicated on re-run"
            );
        }
    }

    fn write_test_agent_yaml(agent: &Path) {
        write_agent_yaml(agent, "files", "fake", "logs", "");
    }

    fn write_agent_yaml(agent: &Path, tracker: &str, runner: &str, foreground: &str, extra: &str) {
        std::fs::write(
            agent.join("agent.yaml"),
            format!(
                r#"id: test-agent
name: Test Agent

tracker:
  use: {tracker}
  config:
    path: ./issues
  active_states: [todo]
  terminal_states: [done]

runner:
  use: {runner}

orchestrator:
  poll_interval_ms: 10000
  max_concurrent: 1
  max_retries: 3
  retry_backoff_ms: 30000

workspace:
  root: ./workspaces

dashboard:
  bind: 127.0.0.1
  port: 7878

foreground: {foreground}
{extra}"#
            ),
        )
        .unwrap();
    }
}
