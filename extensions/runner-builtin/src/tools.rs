//! Native Pi-compatible default coding tools for the builtin runner.
//!
//! Semantics mirror Pi's built-in tools from
//! https://github.com/earendil-works/pi:
//! `packages/coding-agent/src/core/tools/{read,write,edit,bash}.ts`.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::fs;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use tool_registry::{ToolContent, ToolExecutor, ToolOutcome, ToolRegistryHandle, ToolSpec};

const DEFAULT_MAX_LINES: usize = 2_000;
const DEFAULT_MAX_BYTES: usize = 50 * 1024;

pub fn register_into(registry: &dyn ToolRegistryHandle, root: PathBuf) -> Result<()> {
    registry.register_tool(
        read_spec(),
        std::sync::Arc::new(ReadTool { root: root.clone() }),
    )?;
    registry.register_tool(
        write_spec(),
        std::sync::Arc::new(WriteTool { root: root.clone() }),
    )?;
    registry.register_tool(
        edit_spec(),
        std::sync::Arc::new(EditTool { root: root.clone() }),
    )?;
    registry.register_tool(bash_spec(), std::sync::Arc::new(BashTool { root }))?;
    Ok(())
}

fn read_spec() -> ToolSpec {
    ToolSpec::new(
        "read",
        "Read the contents of a file. Supports text files and images (jpg, jpeg, png, gif, webp, bmp). Images are sent as attachments. For text files, output is truncated to 2000 lines or 50KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to read (relative or absolute)" },
                "offset": { "type": "number", "description": "Line number to start reading from (1-indexed)" },
                "limit": { "type": "number", "description": "Maximum number of lines to read" }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
    )
    .reads()
}

fn write_spec() -> ToolSpec {
    ToolSpec::new(
        "write",
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to write (relative or absolute)" },
                "content": { "type": "string", "description": "Content to write to the file" }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        }),
    )
    .writes()
}

fn edit_spec() -> ToolSpec {
    ToolSpec::new(
        "edit",
        "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes.",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to edit (relative or absolute)" },
                "edits": {
                    "type": "array",
                    "description": "One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": { "type": "string", "description": "Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call." },
                            "newText": { "type": "string", "description": "Replacement text for this targeted edit." }
                        },
                        "required": ["oldText", "newText"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["path", "edits"],
            "additionalProperties": false
        }),
    )
    .with_access(true, true)
}

fn bash_spec() -> ToolSpec {
    ToolSpec::new(
        "bash",
        "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last 2000 lines or 50KB (whichever is hit first). Optionally provide a timeout in seconds.",
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Bash command to execute" },
                "timeout": { "type": "number", "description": "Timeout in seconds (optional, no default timeout)" }
            },
            "required": ["command"],
            "additionalProperties": false
        }),
    )
    .with_access(true, true)
}

struct ReadTool {
    root: PathBuf,
}
struct WriteTool {
    root: PathBuf,
}
struct EditTool {
    root: PathBuf,
}
struct BashTool {
    root: PathBuf,
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}
#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}
#[derive(Deserialize)]
struct EditArgs {
    path: String,
    edits: Vec<Replacement>,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Replacement {
    old_text: String,
    new_text: String,
}
#[derive(Deserialize)]
struct BashArgs {
    command: String,
    timeout: Option<f64>,
}

#[async_trait::async_trait]
impl ToolExecutor for ReadTool {
    async fn execute(&self, args: Value) -> Result<ToolOutcome> {
        let args: ReadArgs = serde_json::from_value(args)?;
        let path = resolve_to_root(&self.root, &args.path);
        let bytes = fs::read(&path).await?;
        if let Some(mime_type) = supported_image_mime_type(&path, &bytes) {
            let note = format!("Read image file [{mime_type}]");
            return Ok(ToolOutcome {
                text: note.clone(),
                content: vec![
                    ToolContent::Text { text: note },
                    ToolContent::Image {
                        data: base64_encode(&bytes),
                        mime_type: mime_type.to_string(),
                    },
                ],
                is_error: false,
                error: None,
            });
        }
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.split('\n').collect();
        let start = args.offset.unwrap_or(1).saturating_sub(1);
        if start >= lines.len() {
            bail!(
                "Offset {} is beyond end of file ({} lines total)",
                args.offset.unwrap_or(1),
                lines.len()
            );
        }
        let end = args
            .limit
            .map(|n| (start + n).min(lines.len()))
            .unwrap_or(lines.len());
        let selected = lines[start..end].join("\n");
        Ok(ToolOutcome::ok(truncate_head(
            &selected,
            start + 1,
            lines.len(),
            end < lines.len(),
        )))
    }
}

#[async_trait::async_trait]
impl ToolExecutor for WriteTool {
    async fn execute(&self, args: Value) -> Result<ToolOutcome> {
        let args: WriteArgs = serde_json::from_value(args)?;
        let path = resolve_to_root(&self.root, &args.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(&path, &args.content).await?;
        Ok(ToolOutcome::ok(format!(
            "Successfully wrote {} bytes to {}",
            args.content.len(),
            args.path
        )))
    }
}

#[async_trait::async_trait]
impl ToolExecutor for EditTool {
    async fn execute(&self, args: Value) -> Result<ToolOutcome> {
        let args: EditArgs = serde_json::from_value(args)?;
        if args.edits.is_empty() {
            bail!("Edit tool input is invalid. edits must contain at least one replacement.");
        }
        let path = resolve_to_root(&self.root, &args.path);
        let original = fs::read_to_string(&path)
            .await
            .with_context(|| format!("Could not edit file: {}", args.path))?;
        let new_content = apply_exact_edits(&original, &args.edits, &args.path)?;
        fs::write(&path, new_content).await?;
        Ok(ToolOutcome::ok(format!(
            "Successfully replaced {} block(s) in {}.",
            args.edits.len(),
            args.path
        )))
    }
}

#[async_trait::async_trait]
impl ToolExecutor for BashTool {
    async fn execute(&self, args: Value) -> Result<ToolOutcome> {
        let args: BashArgs = serde_json::from_value(args)?;
        let mut cmd = Command::new("/bin/bash");
        cmd.arg("-lc")
            .arg(&args.command)
            .current_dir(&self.root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = cmd.output();
        let output = if let Some(secs) = args.timeout {
            if !secs.is_finite() || secs <= 0.0 {
                bail!("Invalid timeout: must be a finite number of seconds");
            }
            timeout(Duration::from_secs_f64(secs), child)
                .await
                .map_err(|_| anyhow!("Command timed out after {secs} seconds"))??
        } else {
            child.await?
        };
        let mut text = String::new();
        text.push_str(&String::from_utf8_lossy(&output.stdout));
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        let text = truncate_tail(&text);
        let prefix = match output.status.code() {
            Some(0) => String::new(),
            Some(code) => format!("[exit code {code}]\n"),
            None => "[terminated by signal]\n".to_string(),
        };
        Ok(ToolOutcome::ok(format!("{prefix}{text}")))
    }
}

fn supported_image_mime_type(path: &Path, bytes: &[u8]) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let mime = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => return None,
    };
    let valid_magic = match ext.as_str() {
        "jpg" | "jpeg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        "png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "webp" => bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP",
        "bmp" => bytes.starts_with(b"BM"),
        _ => false,
    };
    valid_magic.then_some(mime)
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0b0000_0011) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 & 0b0000_1111) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0b0011_1111) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn resolve_to_root(root: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn apply_exact_edits(original: &str, edits: &[Replacement], path: &str) -> Result<String> {
    let mut matches = Vec::new();
    for (idx, edit) in edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            bail!("edits[{idx}].oldText is empty in {path}. Provide exact text to replace.");
        }
        let found: Vec<_> = original.match_indices(&edit.old_text).collect();
        match found.len() {
            0 => bail!("edits[{idx}].oldText was not found in {path}."),
            1 => matches.push((found[0].0, found[0].0 + edit.old_text.len(), idx)),
            n => bail!("edits[{idx}].oldText appears {n} times in {path}. Make it unique or merge surrounding context."),
        }
    }
    matches.sort_by_key(|m| m.0);
    for pair in matches.windows(2) {
        if pair[0].1 > pair[1].0 {
            bail!("edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target disjoint regions.", pair[0].2, pair[1].2);
        }
    }
    let mut out = String::new();
    let mut cursor = 0;
    for (start, end, idx) in matches {
        out.push_str(&original[cursor..start]);
        out.push_str(&edits[idx].new_text);
        cursor = end;
    }
    out.push_str(&original[cursor..]);
    if out == original {
        bail!("Edit would not change {path}.");
    }
    Ok(out)
}

fn truncate_head(text: &str, start_line: usize, total_lines: usize, user_limited: bool) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out_lines = lines.len().min(DEFAULT_MAX_LINES);
    let mut out = lines[..out_lines].join("\n");
    while out.len() > DEFAULT_MAX_BYTES && out_lines > 0 {
        out_lines -= 1;
        out = lines[..out_lines].join("\n");
    }
    let truncated = out_lines < lines.len();
    if truncated || user_limited {
        let end_line = start_line + out_lines.saturating_sub(1);
        let next = end_line + 1;
        out.push_str(&format!("\n\n[Showing lines {start_line}-{end_line} of {total_lines}. Use offset={next} to continue.]"));
    }
    out
}

fn truncate_tail(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let start = lines.len().saturating_sub(DEFAULT_MAX_LINES);
    let mut out = lines[start..].join("\n");
    while out.len() > DEFAULT_MAX_BYTES && !out.is_empty() {
        let cut = out.len().saturating_sub(1024);
        out = out[out.len() - cut..].to_string();
    }
    if start > 0 || out.len() < text.len() {
        format!("[output truncated]\n{out}")
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tool_registry::ToolRegistry;

    #[test]
    fn specs_match_pi_default_tool_names_and_shapes() {
        let specs = [read_spec(), write_spec(), edit_spec(), bash_spec()];
        let names: Vec<_> = specs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["read", "write", "edit", "bash"]);
        assert_eq!(specs[0].input_schema["required"], json!(["path"]));
        assert_eq!(
            specs[1].input_schema["required"],
            json!(["path", "content"])
        );
        assert_eq!(
            specs[2].input_schema["properties"]["edits"]["items"]["required"],
            json!(["oldText", "newText"])
        );
        assert_eq!(specs[3].input_schema["required"], json!(["command"]));
    }

    #[tokio::test]
    async fn dispatches_write_read_edit_and_bash() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = ToolRegistry::new();
        register_into(&reg, tmp.path().to_path_buf()).unwrap();

        assert!(
            !reg.dispatch("write", json!({"path":"a.txt","content":"hello old"}))
                .await
                .is_error
        );
        let read = reg.dispatch("read", json!({"path":"a.txt"})).await;
        assert_eq!(read.text, "hello old");
        assert!(
            !reg.dispatch(
                "edit",
                json!({"path":"a.txt","edits":[{"oldText":"old","newText":"new"}]})
            )
            .await
            .is_error
        );
        let read = reg.dispatch("read", json!({"path":"a.txt"})).await;
        assert_eq!(read.text, "hello new");
        let bash = reg.dispatch("bash", json!({"command":"printf ok"})).await;
        assert_eq!(bash.text, "ok");
    }

    #[tokio::test]
    async fn read_png_returns_text_note_and_image_attachment() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = ToolRegistry::new();
        register_into(&reg, tmp.path().to_path_buf()).unwrap();
        fs::write(
            tmp.path().join("pixel.png"),
            [
                0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n', 0, 1, 2, 3,
            ],
        )
        .await
        .unwrap();

        let read = reg.dispatch("read", json!({"path":"pixel.png"})).await;

        assert!(!read.is_error);
        assert_eq!(read.text, "Read image file [image/png]");
        assert_eq!(
            read.content,
            vec![
                ToolContent::Text {
                    text: "Read image file [image/png]".to_string()
                },
                ToolContent::Image {
                    data: "iVBORw0KGgoAAQID".to_string(),
                    mime_type: "image/png".to_string()
                }
            ]
        );
        let mcp = read.to_mcp_result();
        assert_eq!(mcp["content"][0]["type"], "text");
        assert_eq!(mcp["content"][1]["type"], "image");
        assert_eq!(mcp["content"][1]["mimeType"], "image/png");
        assert_eq!(mcp["content"][1]["data"], "iVBORw0KGgoAAQID");
    }
}
