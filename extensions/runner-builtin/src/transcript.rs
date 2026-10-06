//! Append-only chat transcript for the builtin chat backend.
//!
//! One `<timestamp>_<id>.jsonl` file per session in `ChatSessionParams.session_dir`,
//! using the shared archive convention (`crates/extension-sdk/src/chat/archive.rs`):
//! a `{"type":"session","backend":"builtin"}` header, then one JSON line per entry:
//!
//! * `{"type":"message","timestamp":…,"message":<OpenAI chat message>}`
//! * `{"type":"compaction","timestamp":…,"summary":"…"}` — resume replays from the
//!   last compaction.
//!
//! The file is created lazily on the first appended entry, so an opened-but-unused
//! session leaves nothing behind. chat-web discovers the live id by scanning for a
//! header whose file was modified after the session opened, which the first append
//! satisfies.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

pub(crate) const BACKEND_ID: &str = "builtin";

pub(crate) struct Transcript {
    dir: PathBuf,
    id: String,
    path: Option<PathBuf>,
    /// The resumed file's last line lacks a newline; the next append must
    /// start one so entries never share a line.
    needs_newline: bool,
}

/// Prefix of the single history message that replaces compacted history.
const SUMMARY_PREFIX: &str = "Summary of the conversation so far:";

/// The history message carrying a compaction `summary`. A user message: every
/// OpenAI-compatible provider accepts one mid-conversation (a second system
/// message is not universally accepted).
pub(crate) fn summary_message(summary: &str) -> Value {
    json!({"role": "user", "content": format!("{SUMMARY_PREFIX}\n\n{summary}")})
}

impl Transcript {
    pub(crate) fn fresh(dir: &Path, id: String) -> Self {
        Self {
            dir: dir.to_path_buf(),
            id,
            path: None,
            needs_newline: false,
        }
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    /// Reopens the builtin transcript with header id `id` and returns the history
    /// it describes (from the last compaction onward). `None` when no such readable
    /// builtin transcript exists.
    pub(crate) fn resume(dir: &Path, id: &str) -> Option<(Self, Vec<Value>)> {
        let path = find_transcript(dir, id)?;
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) => {
                tracing::warn!(path = %path.display(), %err, "cannot read builtin transcript");
                return None;
            }
        };
        let mut history = Vec::new();
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            let Ok(entry) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            match entry["type"].as_str() {
                Some("message") if entry["message"].is_object() => {
                    history.push(entry["message"].clone());
                }
                Some("compaction") => {
                    history = vec![summary_message(
                        entry["summary"].as_str().unwrap_or_default(),
                    )];
                }
                _ => {}
            }
        }
        Some((
            Self {
                dir: dir.to_path_buf(),
                id: id.to_string(),
                path: Some(path),
                needs_newline: !text.ends_with('\n'),
            },
            history,
        ))
    }

    pub(crate) fn append_message(&mut self, message: &Value) {
        self.append(json!({"type": "message", "message": message}));
    }

    pub(crate) fn append_compaction(&mut self, summary: &str) {
        self.append(json!({"type": "compaction", "summary": summary}));
    }

    /// Best-effort: a failed write only costs persistence, never the turn.
    fn append(&mut self, mut entry: Value) {
        entry["timestamp"] = json!(chrono::Utc::now().to_rfc3339());
        if let Err(err) = self.write_entry(&entry) {
            tracing::warn!(session = %self.id, %err, "failed to persist builtin chat transcript");
        }
    }

    fn write_entry(&mut self, entry: &Value) -> std::io::Result<()> {
        let mut text = String::new();
        let path = match &self.path {
            Some(path) => path.clone(),
            None => {
                std::fs::create_dir_all(&self.dir)?;
                let stamp = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%S-%3fZ");
                let header = json!({
                    "type": "session",
                    "version": 3,
                    "id": self.id,
                    "backend": BACKEND_ID,
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                });
                text.push_str(&format!("{header}\n"));
                self.dir.join(format!("{stamp}_{}.jsonl", self.id))
            }
        };
        if self.needs_newline {
            text.push('\n');
        }
        text.push_str(&format!("{entry}\n"));
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?
            .write_all(text.as_bytes())?;
        self.path = Some(path);
        self.needs_newline = false;
        Ok(())
    }
}

fn find_transcript(dir: &Path, id: &str) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .find(|path| {
            let Ok(file) = std::fs::File::open(path) else {
                return false;
            };
            let mut first = String::new();
            let read = std::io::BufRead::read_line(&mut std::io::BufReader::new(file), &mut first);
            let Ok(header) = read.map(|_| serde_json::from_str::<Value>(first.trim())) else {
                return false;
            };
            header.is_ok_and(|header| {
                header["id"].as_str() == Some(id) && header["backend"].as_str() == Some(BACKEND_ID)
            })
        })
}
