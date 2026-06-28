//! System-file resolver: assemble an agent's identity context into one string.
//!
//! Pure, isolated, and dependency-light. Given an agent root folder and a list
//! of declared [`SystemFileEntry`] values, [`resolve`] produces an ordered list
//! of [`ResolvedFile`]s and an assembled, path-tagged [`SystemContext`] string.
//!
//! ## Ordering & rules
//!
//! 1. `AGENTS.md` (agent root) is prepended first if present. Missing → silent
//!    skip. It always occupies position 0 when present, regardless of whether it
//!    is also listed in `system_files`.
//! 2. Each `system_files` entry follows, in declared order. An entry is either a
//!    bare string path or `{ path, required? }` (`required` defaults `false`).
//! 3. A `required` entry that is missing is a hard error. An optional entry that
//!    is missing produces a [`ResolveWarning`] and is skipped; the rest still
//!    resolves.
//! 4. Entries are de-duped by resolved (canonical) path. `AGENTS.md` listed
//!    again in `system_files` warns and is skipped (it stays at position 0).
//! 5. Every entry must stay contained within the agent root. `..` and absolute
//!    escapes are rejected (a hard error). Symlinks pointing *within* the root
//!    are accepted.
//!
//! All paths are interpreted relative to the agent root.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

pub mod bus;

/// The conventional agent-identity file resolved first, when present.
pub const AGENTS_MD: &str = "AGENTS.md";

/// One declared `system_files` entry: a bare path or a `{ path, required? }`
/// table. `required` defaults to `false`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum SystemFileEntry {
    /// `- some/file.md`
    Bare(String),
    /// `- { path: some/file.md, required: true }`
    Detailed {
        path: String,
        #[serde(default)]
        required: bool,
    },
}

impl SystemFileEntry {
    fn path(&self) -> &str {
        match self {
            SystemFileEntry::Bare(p) => p,
            SystemFileEntry::Detailed { path, .. } => path,
        }
    }

    fn required(&self) -> bool {
        match self {
            SystemFileEntry::Bare(_) => false,
            SystemFileEntry::Detailed { required, .. } => *required,
        }
    }
}

/// One file that resolved to readable content, in final assembly order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFile {
    /// Root-relative, forward-slash display path used in the tagged block.
    pub display_path: String,
    /// File contents.
    pub contents: String,
}

/// A non-fatal condition encountered while resolving (missing optional file,
/// duplicate entry, `AGENTS.md` re-listed). Surfaced so callers can log them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveWarning {
    /// An optional entry pointed at a file that does not exist; skipped.
    MissingOptional { path: String },
    /// An entry resolved to a path already included; skipped.
    Duplicate { path: String },
    /// `AGENTS.md` was listed in `system_files`; skipped (it stays at pos 0).
    AgentsMdRelisted { path: String },
}

impl std::fmt::Display for ResolveWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveWarning::MissingOptional { path } => {
                write!(f, "optional system file {path:?} not found; skipping")
            }
            ResolveWarning::Duplicate { path } => {
                write!(f, "duplicate system file {path:?}; skipping")
            }
            ResolveWarning::AgentsMdRelisted { path } => write!(
                f,
                "{path:?} is already loaded as {AGENTS_MD}; skipping the system_files entry"
            ),
        }
    }
}

/// A fatal resolution failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// A `required` entry pointed at a file that does not exist.
    MissingRequired { path: String },
    /// An entry escaped the agent root (`..`, absolute, or symlink target
    /// outside the root).
    Containment { path: String, reason: String },
    /// A file existed but could not be read.
    Read { path: String, reason: String },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::MissingRequired { path } => {
                write!(f, "required system file {path:?} not found")
            }
            ResolveError::Containment { path, reason } => {
                write!(f, "system file {path:?} escapes the agent folder: {reason}")
            }
            ResolveError::Read { path, reason } => {
                write!(f, "reading system file {path:?} failed: {reason}")
            }
        }
    }
}

impl std::error::Error for ResolveError {}

/// The assembled, agent-wide system context: the path-tagged string plus the
/// ordered list of files that contributed to it. Published on the retained bus
/// topic at boot so every surface reads the same identity.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SystemContext {
    /// The assembled context string (each file in a delimited, tagged block).
    pub text: String,
    /// Resolved files in assembly order.
    pub files: Vec<ResolvedFile>,
    /// Non-fatal conditions surfaced during resolution.
    pub warnings: Vec<ResolveWarning>,
}

impl SystemContext {
    /// `true` when no files resolved (empty identity context).
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// Resolve `AGENTS.md` + `system_files` into an assembled [`SystemContext`].
///
/// `root` is the agent folder. `entries` is the declared `system_files` list
/// (`None` ⇒ absent key ⇒ `AGENTS.md` only).
pub fn resolve(
    root: &Path,
    entries: Option<&[SystemFileEntry]>,
) -> Result<SystemContext, ResolveError> {
    let mut files: Vec<ResolvedFile> = Vec::new();
    let mut warnings: Vec<ResolveWarning> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    // 1. AGENTS.md first (optional, silent skip when absent).
    let agents_md = root.join(AGENTS_MD);
    let agents_md_canonical = if agents_md.exists() {
        let canonical = contain(root, AGENTS_MD, &agents_md, AGENTS_MD)?;
        let contents = read(&agents_md, AGENTS_MD)?;
        seen.insert(canonical.clone());
        files.push(ResolvedFile {
            display_path: AGENTS_MD.to_string(),
            contents,
        });
        Some(canonical)
    } else {
        None
    };

    // 2. system_files entries, in declared order.
    for entry in entries.unwrap_or(&[]) {
        let rel = entry.path();
        let display = normalize_display(rel);
        let abs = root.join(rel);

        // Containment is checked first so escapes fail even when missing.
        let canonical = contain(root, rel, &abs, &display)?;

        if !abs.exists() {
            if entry.required() {
                return Err(ResolveError::MissingRequired { path: display });
            }
            warnings.push(ResolveWarning::MissingOptional { path: display });
            continue;
        }

        if agents_md_canonical.as_ref() == Some(&canonical) {
            warnings.push(ResolveWarning::AgentsMdRelisted { path: display });
            continue;
        }
        if !seen.insert(canonical) {
            warnings.push(ResolveWarning::Duplicate { path: display });
            continue;
        }

        let contents = read(&abs, &display)?;
        files.push(ResolvedFile {
            display_path: display,
            contents,
        });
    }

    let text = assemble(&files);
    Ok(SystemContext {
        text,
        files,
        warnings,
    })
}

/// Wrap each resolved file in a delimited, path-tagged block and join them.
fn assemble(files: &[ResolvedFile]) -> String {
    let mut out = String::new();
    for file in files {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("<system-file path=\"{}\">\n", file.display_path));
        out.push_str(&file.contents);
        if !file.contents.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("</system-file>\n");
    }
    out
}

/// Reject `..`/absolute escapes, accept symlinks within `root`, and return the
/// canonical path of an existing file (or its canonical parent + name when the
/// file does not yet exist, so missing-file handling stays the caller's job).
///
/// `rel` is the declared (root-relative) path used for the `..`/absolute checks;
/// `abs` is `root.join(rel)`, the path actually probed on disk.
fn contain(root: &Path, rel: &str, abs: &Path, display: &str) -> Result<PathBuf, ResolveError> {
    let rel = Path::new(rel);
    let root = root.canonicalize().map_err(|e| ResolveError::Containment {
        path: display.to_string(),
        reason: format!("canonicalizing agent root: {e}"),
    })?;
    if rel.is_absolute() {
        return Err(ResolveError::Containment {
            path: display.to_string(),
            reason: "absolute paths are not allowed".to_string(),
        });
    }
    if rel.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(ResolveError::Containment {
            path: display.to_string(),
            reason: "parent traversal (`..`) is not allowed".to_string(),
        });
    }
    let canonical = if abs.exists() {
        abs.canonicalize().map_err(|e| ResolveError::Containment {
            path: display.to_string(),
            reason: format!("canonicalizing path: {e}"),
        })?
    } else {
        let parent = abs.parent().unwrap_or(abs);
        let parent = if parent.as_os_str().is_empty() {
            root.clone()
        } else {
            parent
                .canonicalize()
                .map_err(|e| ResolveError::Containment {
                    path: display.to_string(),
                    reason: format!("canonicalizing parent: {e}"),
                })?
        };
        parent.join(abs.file_name().unwrap_or_default())
    };
    if !canonical.starts_with(&root) {
        return Err(ResolveError::Containment {
            path: display.to_string(),
            reason: "resolves outside the agent folder".to_string(),
        });
    }
    Ok(canonical)
}

fn read(path: &Path, display: &str) -> Result<String, ResolveError> {
    std::fs::read_to_string(path).map_err(|e| ResolveError::Read {
        path: display.to_string(),
        reason: e.to_string(),
    })
}

/// Normalize a declared path for display: forward slashes, no `./` prefix.
fn normalize_display(rel: &str) -> String {
    let trimmed = rel.strip_prefix("./").unwrap_or(rel);
    trimmed.replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(dir: &Path, rel: &str, contents: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn bare(p: &str) -> SystemFileEntry {
        SystemFileEntry::Bare(p.to_string())
    }

    fn required(p: &str) -> SystemFileEntry {
        SystemFileEntry::Detailed {
            path: p.to_string(),
            required: true,
        }
    }

    #[test]
    fn agents_md_first_then_entries_in_declared_order() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), AGENTS_MD, "i am the agent");
        write(dir.path(), "a.md", "alpha");
        write(dir.path(), "b.md", "beta");

        let ctx = resolve(dir.path(), Some(&[bare("b.md"), bare("a.md")])).unwrap();

        let order: Vec<_> = ctx.files.iter().map(|f| f.display_path.as_str()).collect();
        assert_eq!(order, vec![AGENTS_MD, "b.md", "a.md"]);
    }

    #[test]
    fn missing_agents_md_skipped_silently() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "a.md", "alpha");

        let ctx = resolve(dir.path(), Some(&[bare("a.md")])).unwrap();

        assert_eq!(ctx.files.len(), 1);
        assert_eq!(ctx.files[0].display_path, "a.md");
        assert!(ctx.warnings.is_empty());
    }

    #[test]
    fn missing_required_entry_errors() {
        let dir = TempDir::new().unwrap();
        let err = resolve(dir.path(), Some(&[required("nope.md")])).unwrap_err();
        assert_eq!(
            err,
            ResolveError::MissingRequired {
                path: "nope.md".to_string()
            }
        );
    }

    #[test]
    fn missing_optional_warns_and_rest_resolves() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "b.md", "beta");

        let ctx = resolve(dir.path(), Some(&[bare("missing.md"), bare("b.md")])).unwrap();

        assert_eq!(ctx.files.len(), 1);
        assert_eq!(ctx.files[0].display_path, "b.md");
        assert_eq!(
            ctx.warnings,
            vec![ResolveWarning::MissingOptional {
                path: "missing.md".to_string()
            }]
        );
    }

    #[test]
    fn agents_md_relisted_appears_once_at_position_zero() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), AGENTS_MD, "agent");
        write(dir.path(), "a.md", "alpha");

        let ctx = resolve(dir.path(), Some(&[bare("a.md"), bare(AGENTS_MD)])).unwrap();

        let order: Vec<_> = ctx.files.iter().map(|f| f.display_path.as_str()).collect();
        assert_eq!(order, vec![AGENTS_MD, "a.md"]);
        assert_eq!(
            ctx.warnings,
            vec![ResolveWarning::AgentsMdRelisted {
                path: AGENTS_MD.to_string()
            }]
        );
    }

    #[test]
    fn duplicate_entries_deduped() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "a.md", "alpha");

        let ctx = resolve(dir.path(), Some(&[bare("a.md"), bare("./a.md")])).unwrap();

        assert_eq!(ctx.files.len(), 1);
        assert_eq!(
            ctx.warnings,
            vec![ResolveWarning::Duplicate {
                path: "a.md".to_string()
            }]
        );
    }

    #[test]
    fn parent_traversal_rejected() {
        let dir = TempDir::new().unwrap();
        let err = resolve(dir.path(), Some(&[bare("../escape.md")])).unwrap_err();
        assert!(matches!(err, ResolveError::Containment { .. }));
    }

    #[test]
    fn absolute_path_rejected() {
        let dir = TempDir::new().unwrap();
        let err = resolve(dir.path(), Some(&[bare("/etc/passwd")])).unwrap_err();
        assert!(matches!(err, ResolveError::Containment { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_within_root_accepted() {
        use std::os::unix::fs::symlink;
        let dir = TempDir::new().unwrap();
        write(dir.path(), "real/target.md", "real content");
        symlink(
            dir.path().join("real/target.md"),
            dir.path().join("link.md"),
        )
        .unwrap();

        let ctx = resolve(dir.path(), Some(&[bare("link.md")])).unwrap();

        assert_eq!(ctx.files.len(), 1);
        assert_eq!(ctx.files[0].contents, "real content");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escaping_root_rejected() {
        use std::os::unix::fs::symlink;
        let outside = TempDir::new().unwrap();
        write(outside.path(), "secret.md", "leak");
        let dir = TempDir::new().unwrap();
        symlink(outside.path().join("secret.md"), dir.path().join("link.md")).unwrap();

        let err = resolve(dir.path(), Some(&[bare("link.md")])).unwrap_err();
        assert!(matches!(err, ResolveError::Containment { .. }));
    }

    #[test]
    fn absent_system_files_key_resolves_agents_md_only() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), AGENTS_MD, "just me");
        write(dir.path(), "ignored.md", "not referenced");

        let ctx = resolve(dir.path(), None).unwrap();

        assert_eq!(ctx.files.len(), 1);
        assert_eq!(ctx.files[0].display_path, AGENTS_MD);
    }

    #[test]
    fn absent_key_and_no_agents_md_is_empty() {
        let dir = TempDir::new().unwrap();
        let ctx = resolve(dir.path(), None).unwrap();
        assert!(ctx.is_empty());
        assert!(ctx.text.is_empty());
    }

    #[test]
    fn assembled_string_wraps_each_file_in_tagged_block_in_order() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), AGENTS_MD, "agent body");
        write(dir.path(), "extra.md", "extra body");

        let ctx = resolve(dir.path(), Some(&[bare("extra.md")])).unwrap();

        let expected = "<system-file path=\"AGENTS.md\">\nagent body\n</system-file>\n\
                        \n<system-file path=\"extra.md\">\nextra body\n</system-file>\n";
        assert_eq!(ctx.text, expected);
        // Order preserved and AGENTS.md block precedes extra.md block.
        let agents_at = ctx.text.find("AGENTS.md").unwrap();
        let extra_at = ctx.text.find("extra.md").unwrap();
        assert!(agents_at < extra_at);
    }

    #[test]
    fn detailed_entry_required_defaults_false() {
        let entry: SystemFileEntry =
            serde_yaml_from_str("{ path: foo.md }").expect("parse detailed entry");
        assert_eq!(entry.required(), false);
        assert_eq!(entry.path(), "foo.md");
    }

    #[test]
    fn bare_and_detailed_round_trip() {
        let list: Vec<SystemFileEntry> =
            serde_yaml_from_str("- foo.md\n- { path: bar.md, required: true }\n")
                .expect("parse mixed list");
        assert_eq!(
            list,
            vec![
                SystemFileEntry::Bare("foo.md".to_string()),
                SystemFileEntry::Detailed {
                    path: "bar.md".to_string(),
                    required: true,
                },
            ]
        );
    }

    // Minimal YAML helper so the resolver crate's own tests can exercise the
    // serde derive without taking a hard dep on serde_yaml in this crate.
    fn serde_yaml_from_str<T: serde::de::DeserializeOwned>(
        s: &str,
    ) -> Result<T, serde_yaml::Error> {
        serde_yaml::from_str(s)
    }
}
