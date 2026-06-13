//! Per-agent composition crate generation.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

const GENERATED_TOML_HEADER: &str = "# generated - do not hand-edit\n";
const GENERATED_RUST_HEADER: &str = "// # generated - do not hand-edit\n";

struct StockExtension {
    package: &'static str,
    factory: &'static str,
}

const STOCK_EXTENSIONS: &[StockExtension] = &[
    StockExtension {
        package: "frontend-log",
        factory: "frontend_log::FrontendLogExtension",
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
        package: "runner-claude",
        factory: "runner_claude::RunnerClaudeExtension",
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
];

#[derive(Debug, Clone, Eq, PartialEq)]
struct LocalExtension {
    package: String,
    factory: String,
    path: PathBuf,
}

pub fn init_build(agent: &Path) -> Result<()> {
    let agent = agent
        .canonicalize()
        .with_context(|| format!("resolving agent folder {}", agent.display()))?;
    let crate_dir = agent.join(".agentropy");
    fs::create_dir_all(crate_dir.join("src"))
        .with_context(|| format!("creating {}", crate_dir.join("src").display()))?;

    let locals = discover_extensions(&agent)?;
    write_if_changed(
        &crate_dir.join("Cargo.toml"),
        &cargo_toml(&crate_dir, &locals)?,
    )?;
    write_if_changed(&crate_dir.join("src/main.rs"), &main_rs(&locals))?;
    write_if_changed(
        &crate_dir.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"1.83\"\n",
    )?;
    refresh_lockfile(&crate_dir)?;
    Ok(())
}

pub fn build(agent: &Path) -> Result<()> {
    init_build(agent)?;
    let agent = agent
        .canonicalize()
        .with_context(|| format!("resolving agent folder {}", agent.display()))?;
    let crate_dir = agent.join(".agentropy");
    run_cargo(&crate_dir, ["build", "--release", "--locked"])?;
    fs::create_dir_all(agent.join("bin"))
        .with_context(|| format!("creating {}", agent.join("bin").display()))?;
    let binary = crate_dir
        .join("target")
        .join("release")
        .join(binary_name("agentropy"));
    fs::copy(&binary, agent.join("bin").join(binary_name("agentropy")))
        .with_context(|| format!("copying {}", binary.display()))?;
    Ok(())
}

fn discover_extensions(agent: &Path) -> Result<Vec<LocalExtension>> {
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
        let package = value
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(toml::Value::as_str)
            .context("extension manifest missing [package] name")?
            .to_string();
        let factory = value
            .get("package")
            .and_then(|p| p.get("metadata"))
            .and_then(|m| m.get("agentropy"))
            .and_then(|a| a.get("factory"))
            .and_then(toml::Value::as_str);
        let Some(factory) = factory else {
            continue;
        };
        extensions.push(LocalExtension {
            package,
            factory: factory.to_string(),
            path: PathBuf::from("../extensions").join(entry.file_name()),
        });
    }
    extensions.sort_by(|a, b| a.package.cmp(&b.package));
    Ok(extensions)
}

fn cargo_toml(crate_dir: &Path, locals: &[LocalExtension]) -> Result<String> {
    let repo = repo_root();
    let mut out = String::from(GENERATED_TOML_HEADER);
    out.push_str(
        r#"[package]
name = "agentropy-agent"
version = "0.1.0"
edition = "2021"
rust-version = "1.83"

[[bin]]
name = "agentropy"
path = "src/main.rs"

[workspace]

[dependencies]
"#,
    );
    dependency(
        &mut out,
        "host-api",
        &repo.join("crates/host-api"),
        crate_dir,
    )?;
    dependency(
        &mut out,
        "agentropy-cli",
        &repo.join("crates/agentropy-cli"),
        crate_dir,
    )?;
    out.push_str("tokio = { version = \"1.43\", features = [\"rt-multi-thread\", \"macros\", \"signal\"] }\n");
    for stock in STOCK_EXTENSIONS {
        dependency(
            &mut out,
            stock.package,
            &repo.join("extensions").join(stock.package),
            crate_dir,
        )?;
    }
    for local in locals {
        out.push_str(&format!(
            "{} = {{ path = \"{}\" }}\n",
            local.package,
            toml_path(&local.path)
        ));
    }
    Ok(out)
}

fn dependency(out: &mut String, package: &str, path: &Path, crate_dir: &Path) -> Result<()> {
    out.push_str(&format!(
        "{package} = {{ path = \"{}\" }}\n",
        toml_path(&relative_path(crate_dir, path)?)
    ));
    Ok(())
}

fn main_rs(locals: &[LocalExtension]) -> String {
    let mut out = String::from(GENERATED_RUST_HEADER);
    out.push_str(
        r#"#[tokio::main(worker_threads = 2)]
async fn main() {
    agentropy_cli::run(host_api::plugins![
"#,
    );
    for stock in STOCK_EXTENSIONS {
        out.push_str(&format!("        {},\n", stock.factory));
    }
    for local in locals {
        out.push_str(&format!("        {}(),\n", local.factory));
    }
    out.push_str("    ])\n    .await\n}\n");
    out
}

fn refresh_lockfile(crate_dir: &Path) -> Result<()> {
    run_cargo(crate_dir, ["generate-lockfile"])
}

fn run_cargo<const N: usize>(crate_dir: &Path, args: [&str; N]) -> Result<()> {
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

fn write_if_changed(path: &Path, content: &str) -> Result<bool> {
    if matches!(fs::read_to_string(path), Ok(existing) if existing == content) {
        return Ok(false);
    }
    fs::write(path, content).with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("agentropy-cli lives under crates/")
        .to_path_buf()
}

fn relative_path(from_dir: &Path, to: &Path) -> Result<PathBuf> {
    let from_components = from_dir.components().collect::<Vec<_>>();
    let to_components = to.components().collect::<Vec<_>>();
    let shared = from_components
        .iter()
        .zip(to_components.iter())
        .take_while(|(a, b)| a == b)
        .count();
    if shared == 0 {
        return Ok(to.to_path_buf());
    }
    let mut out = PathBuf::new();
    for _ in shared..from_components.len() {
        out.push("..");
    }
    for component in &to_components[shared..] {
        out.push(component.as_os_str());
    }
    Ok(out)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_discovers_extensions_and_generates_stable_crate_files() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path();
        write_test_extension(agent);

        init_build(agent).unwrap();

        let manifest = std::fs::read_to_string(agent.join(".agentropy/Cargo.toml")).unwrap();
        let source = std::fs::read_to_string(agent.join(".agentropy/src/main.rs")).unwrap();
        manifest.parse::<toml::Value>().unwrap();
        assert!(manifest.starts_with("# generated - do not hand-edit\n"));
        assert!(manifest.contains("\n[workspace]\n"));
        assert!(manifest.contains("my-ext = { path = \"../extensions/my-ext\" }"));
        assert!(source.starts_with("// # generated - do not hand-edit\n"));
        assert!(source.contains("my_ext::extension(),"));

        let manifest_before = manifest;
        let source_before = source;
        init_build(agent).unwrap();

        assert_eq!(
            manifest_before,
            std::fs::read_to_string(agent.join(".agentropy/Cargo.toml")).unwrap()
        );
        assert_eq!(
            source_before,
            std::fs::read_to_string(agent.join(".agentropy/src/main.rs")).unwrap()
        );
    }

    #[test]
    fn build_compiles_generated_crate_and_copies_agent_binary() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path();
        write_test_extension(agent);

        build(agent).unwrap();

        assert!(agent.join("bin").join(binary_name("agentropy")).is_file());
    }

    #[test]
    fn init_build_refreshes_lockfile_when_manifest_changes() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path();

        init_build(agent).unwrap();
        let original_lock = std::fs::read_to_string(agent.join(".agentropy/Cargo.lock")).unwrap();

        write_test_extension(agent);
        init_build(agent).unwrap();

        let manifest = std::fs::read_to_string(agent.join(".agentropy/Cargo.toml")).unwrap();
        let refreshed_lock = std::fs::read_to_string(agent.join(".agentropy/Cargo.lock")).unwrap();
        assert!(manifest.contains("my-ext = { path = \"../extensions/my-ext\" }"));
        assert_ne!(original_lock, refreshed_lock);
        assert!(refreshed_lock.contains("name = \"my-ext\""));
    }

    #[test]
    fn toml_paths_use_forward_slashes() {
        assert_eq!(
            toml_path(Path::new(r"..\extensions\my-ext")),
            "../extensions/my-ext"
        );
    }

    fn write_test_extension(agent: &Path) {
        let extension = agent.join("extensions/my-ext");
        std::fs::create_dir_all(extension.join("src")).unwrap();
        let host_api_path = repo_root().join("crates/host-api");
        std::fs::write(
            extension.join("Cargo.toml"),
            format!(
                r#"[package]
name = "my-ext"
version = "0.1.0"
edition = "2021"

[package.metadata.agentropy]
factory = "my_ext::extension"

[dependencies]
host-api = {{ path = "{}" }}
"#,
                host_api_path.display()
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
}
