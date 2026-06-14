use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

fn main() -> Result<()> {
    CargoAgentropy::parse_from(cargo_subcommand_args()).run()
}

fn cargo_subcommand_args() -> Vec<std::ffi::OsString> {
    let mut args: Vec<_> = std::env::args_os().collect();
    if args.get(1).and_then(|s| s.to_str()) == Some("agentropy") {
        args.remove(1);
    }
    args
}

#[derive(Debug, Parser)]
#[command(name = "cargo-agentropy", version, about = "agentropy Cargo helpers")]
struct CargoAgentropy {
    #[command(subcommand)]
    command: Command,
}

impl CargoAgentropy {
    fn run(self) -> Result<()> {
        match self.command {
            Command::New(args) => scaffold(args),
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Scaffold a host-api extension crate under extensions/.
    New(NewArgs),
}

#[derive(Debug, Parser)]
struct NewArgs {
    /// Extension crate name, for example my-extension.
    name: String,
    /// Extension lifecycle shape to demonstrate.
    #[arg(long, value_enum)]
    kind: ExtensionKind,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ExtensionKind {
    Background,
    Service,
    Foreground,
}

fn scaffold(args: NewArgs) -> Result<()> {
    validate_crate_name(&args.name)?;
    let dir = PathBuf::from("extensions").join(&args.name);
    if dir.exists() {
        bail!("{} already exists", dir.display());
    }

    let type_name = extension_type_name(&args.name);
    let manifest = cargo_toml(&args.name)?;
    std::fs::create_dir_all(dir.join("src"))
        .with_context(|| format!("creating {}", dir.join("src").display()))?;
    std::fs::write(dir.join("Cargo.toml"), manifest)
        .with_context(|| format!("writing {}", dir.join("Cargo.toml").display()))?;
    std::fs::write(
        dir.join("src/lib.rs"),
        lib_rs(&args.name, &type_name, args.kind),
    )
    .with_context(|| format!("writing {}", dir.join("src/lib.rs").display()))?;

    println!("created {}", dir.display());
    print!("{}", post_scaffold_guidance(&args.name));
    Ok(())
}

fn validate_crate_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        && name.bytes().next().is_some_and(|b| b.is_ascii_lowercase());
    if !valid {
        bail!("crate name must start with a lowercase letter and contain only lowercase letters, digits, '-' or '_'");
    }
    Ok(())
}

fn crate_ident(name: &str) -> String {
    name.replace('-', "_")
}

fn extension_type_name(name: &str) -> String {
    let mut out = String::new();
    for part in name.split(['-', '_']).filter(|part| !part.is_empty()) {
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    out.push_str("Extension");
    out
}

fn cargo_toml(name: &str) -> Result<String> {
    cargo_toml_with_source(name, &stock_source()?)
}

fn cargo_toml_with_source(name: &str, source: &StockSource) -> Result<String> {
    let ident = crate_ident(name);
    Ok(format!(
        r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2021"
rust-version = "1.83"

[package.metadata.agentropy]
factory = "{ident}::extension"

[dependencies]
anyhow = "1"
host-api = {{ git = "{git}", rev = "{rev}" }}
tokio = {{ version = "1.43", features = ["macros", "rt", "sync", "time"] }}

[workspace]
"#,
        git = source.git,
        rev = source.rev
    ))
}

fn post_scaffold_guidance(name: &str) -> String {
    format!(
        r#"agent-local extension ready.
`agentropy build --dir .` auto-discovers extensions/{name}/ from its package metadata.
"#
    )
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct StockSource {
    git: String,
    rev: String,
}

fn stock_source() -> Result<StockSource> {
    let repo = repo_root();
    let git = git_output(&repo, ["config", "--get", "remote.origin.url"])?;
    ensure_portable_git_url(&git)?;
    let rev = git_output(&repo, ["rev-parse", "HEAD"])?;
    Ok(StockSource { git, rev })
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("dist lives under the repository root")
        .to_path_buf()
}

fn ensure_portable_git_url(url: &str) -> Result<()> {
    if url
        .split_once(':')
        .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("file"))
        || url.starts_with('/')
        || url.starts_with("./")
        || url.starts_with("../")
        || has_windows_drive_prefix(url)
        || Path::new(url).is_absolute()
    {
        bail!("remote.origin.url must be a portable git URL, got {url:?}");
    }
    let has_scheme = url.contains("://");
    let is_scp_like_ssh = url
        .split_once(':')
        .is_some_and(|(prefix, suffix)| prefix.contains('@') && !suffix.is_empty());
    if !has_scheme && !is_scp_like_ssh {
        bail!("remote.origin.url must be a portable git URL, got {url:?}");
    }
    Ok(())
}

fn has_windows_drive_prefix(url: &str) -> bool {
    let bytes = url.as_bytes();
    bytes.len() >= 3 && bytes[1] == b':' && bytes[2] == b'\\' && bytes[0].is_ascii_alphabetic()
}

fn git_output<const N: usize>(repo: &Path, args: [&str; N]) -> Result<String> {
    let output = ProcessCommand::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .with_context(|| format!("running git in {}", repo.display()))?;
    if !output.status.success() {
        bail!("git exited with {}", output.status);
    }
    Ok(String::from_utf8(output.stdout)
        .context("git output was not utf-8")?
        .trim()
        .to_string())
}

fn lib_rs(name: &str, type_name: &str, kind: ExtensionKind) -> String {
    match kind {
        ExtensionKind::Background => background_rs(name, type_name),
        ExtensionKind::Service => service_rs(name, type_name),
        ExtensionKind::Foreground => foreground_rs(name, type_name),
    }
}

fn background_rs(name: &str, type_name: &str) -> String {
    format!(
        r#"use anyhow::Result;
use host_api::{{Extension, RegisterCtx, StartCtx}};

pub const TICK_TOPIC: &str = "{name}.tick";

pub struct {type_name};

pub fn extension() -> Box<dyn Extension> {{
    Box::new({type_name})
}}

impl Extension for {type_name} {{
    fn id(&self) -> &'static str {{
        "{name}"
    }}

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {{
        Box::pin(async move {{
            ctx.bus.register_broadcast::<u64>(TICK_TOPIC, 16)?;
            Ok(())
        }})
    }}

    fn start<'a>(&'a self, ctx: StartCtx) -> host_api::BoxFuture<'a, Result<()>> {{
        Box::pin(async move {{
            let mut shutdown = ctx.shutdown.clone();
            let bus = ctx.host.bus.clone();
            tokio::spawn(async move {{
                let mut tick = 0_u64;
                loop {{
                    tokio::select! {{
                        _ = shutdown.cancelled() => break,
                        _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {{
                            tick += 1;
                            let _ = bus.publish(TICK_TOPIC, tick);
                        }}
                    }}
                }}
            }});
            Ok(())
        }})
    }}
}}
"#
    )
}

fn service_rs(name: &str, type_name: &str) -> String {
    format!(
        r#"use std::sync::Arc;

use anyhow::Result;
use host_api::{{Extension, RegisterCtx}};

pub trait {type_name}Service: Send + Sync {{
    fn hello(&self) -> &'static str;
}}

struct Service;

impl {type_name}Service for Service {{
    fn hello(&self) -> &'static str {{
        "hello from {name}"
    }}
}}

pub struct {type_name};

pub fn extension() -> Box<dyn Extension> {{
    Box::new({type_name})
}}

impl Extension for {type_name} {{
    fn id(&self) -> &'static str {{
        "{name}"
    }}

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {{
        Box::pin(async move {{
            ctx.services
                .service::<dyn {type_name}Service>(self.id(), Arc::new(Service))?;
            Ok(())
        }})
    }}
}}
"#
    )
}

fn foreground_rs(name: &str, type_name: &str) -> String {
    format!(
        r#"use std::sync::Arc;

use anyhow::Result;
use host_api::{{ExclusiveTerminal, Extension, Foreground, RegisterCtx, StartCtx}};

pub struct {type_name};

pub fn extension() -> Box<dyn Extension> {{
    Box::new({type_name})
}}

impl Extension for {type_name} {{
    fn id(&self) -> &'static str {{
        "{name}"
    }}

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {{
        Box::pin(async move {{
            ctx.foreground
                .foreground(self.id(), Arc::new(|| Box::new({type_name}Foreground)))?;
            Ok(())
        }})
    }}
}}

struct {type_name}Foreground;

impl Foreground for {type_name}Foreground {{
    fn run<'a>(
        &'a mut self,
        mut ctx: StartCtx,
        mut terminal: ExclusiveTerminal,
    ) -> host_api::BoxFuture<'a, Result<()>> {{
        Box::pin(async move {{
            writeln!(terminal.writer(), "foreground {name} started")?;
            ctx.shutdown.cancelled().await;
            terminal.restore();
            Ok(())
        }})
    }}
}}
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_toml_includes_agentropy_factory_marker_and_workspace() {
        let manifest = cargo_toml_with_source(
            "my-ext",
            &StockSource {
                git: "https://github.com/tobalsan/dar.git".to_string(),
                rev: "abc123".to_string(),
            },
        )
        .unwrap();

        assert!(manifest.contains("[package.metadata.agentropy]\nfactory = \"my_ext::extension\""));
        assert!(manifest.contains(
            r#"host-api = { git = "https://github.com/tobalsan/dar.git", rev = "abc123" }"#
        ));
        assert!(!manifest.contains("../../crates/host-api"));
        assert!(manifest.contains("\n[workspace]\n"));
    }

    #[test]
    fn post_scaffold_guidance_describes_agent_local_discovery() {
        let guidance = post_scaffold_guidance("my-ext");

        assert!(guidance.contains("agentropy build --dir ."));
        assert!(guidance.contains("auto-discovers extensions/my-ext"));
        assert!(!guidance.contains("dist/root Cargo.toml"));
    }

    #[test]
    fn scaffolded_extension_kinds_include_factory() {
        for kind in [
            ExtensionKind::Background,
            ExtensionKind::Service,
            ExtensionKind::Foreground,
        ] {
            let source = lib_rs("my-ext", "MyExtExtension", kind);

            assert!(source.contains("pub fn extension() -> Box<dyn Extension>"));
            assert!(source.contains("Box::new(MyExtExtension)"));
        }
    }
}
