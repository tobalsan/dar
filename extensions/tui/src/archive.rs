//! Session archive helpers: locate the newest persisted chat session and read
//! its id so the foreground can resume it on launch.
//!
//! pi writes one `.jsonl` per session under the sessions dir, named
//! `<ISO-timestamp>_<uuid>.jsonl`. Because the timestamp leads the name, plain
//! lexical filename ordering equals chronological ordering, so "newest" is just
//! the lexically-greatest `.jsonl` filename — no need to read mtimes or parse
//! the timestamp. The session id lives in the file's header line, a JSON object
//! `{"type":"session","version":N,"id":"<id>"}` written first.
//!
//! Everything here is best-effort: a missing/empty dir or an unreadable/
//! malformed newest file yields `None`, and the caller falls back to opening a
//! fresh session rather than blocking the human from chatting.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

use serde_json::Value;

/// Upper bound on how many sessions `list` returns, so the tool output stays
/// bounded regardless of how many `.jsonl` files have accumulated.
pub const LIST_LIMIT: usize = 50;

/// One entry in the session listing: enough for the agent to recognize and
/// later recall a prior conversation, drawn entirely from a file's name and its
/// single header line — never any message body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    /// Session id from the header line (`{"type":"session",...,"id":...}`).
    pub id: String,
    /// Start time as the ISO-timestamp prefix of the filename
    /// (`<ISO-timestamp>_<uuid>.jsonl`), or `None` when the name doesn't carry
    /// one.
    pub start_time: Option<String>,
    /// A short human label: the header's `label`/`title` when present,
    /// otherwise the file stem. Always bounded in length.
    pub label: String,
}

/// Max label length kept per entry, so a stray long header field can't blow up
/// the listing.
const LABEL_MAX: usize = 120;

/// List persisted sessions newest-first, reading only each file's header line
/// (never any message body). A missing/unreadable dir yields an empty list; a
/// malformed or header-less file is skipped, not fatal. The result is bounded
/// to [`LIST_LIMIT`] entries.
pub fn list(sessions_dir: &Path) -> Vec<SessionInfo> {
    let mut files = match fs::read_dir(sessions_dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .collect::<Vec<_>>(),
        Err(_) => return Vec::new(),
    };
    // Newest-first: lexical filename order == chronological for the
    // `<ISO-timestamp>_<uuid>.jsonl` naming, so reverse the lexical sort.
    files.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    files
        .into_iter()
        .filter_map(|path| read_session_info(&path))
        .take(LIST_LIMIT)
        .collect()
}

/// Read one session's listing entry from its header line: the `id` (required),
/// an optional `label`/`title`, and the start time from the filename. `None`
/// when the file can't be read or its header is malformed / carries no `id` —
/// the caller skips it.
fn read_session_info(path: &Path) -> Option<SessionInfo> {
    let header = read_header_value(path)?;
    let id = header.get("id").and_then(Value::as_str)?;
    if id.is_empty() {
        return None;
    }
    let file_stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let start_time = file_stem.split_once('_').map(|(ts, _)| ts.to_string());
    let label = header
        .get("label")
        .or_else(|| header.get("title"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&file_stem)
        .chars()
        .take(LABEL_MAX)
        .collect();
    Some(SessionInfo {
        id: id.to_string(),
        start_time,
        label,
    })
}

/// Read and parse a session file's header line (the first non-empty line, a
/// JSON object). `None` on any read/parse failure.
fn read_header_value(path: &Path) -> Option<Value> {
    let file = fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line).ok()?;
        if read == 0 {
            return None; // EOF before any non-empty line.
        }
        if line.trim().is_empty() {
            continue;
        }
        return serde_json::from_str(line.trim()).ok();
    }
}

/// Resolve the id of the newest persisted session under `sessions_dir`, or
/// `None` when there is nothing resumable: the dir is missing/empty, the newest
/// file can't be read, or its header is malformed / carries no `id`. Never
/// errors — resume is always optional.
pub fn newest_session_id(sessions_dir: &Path) -> Option<String> {
    let newest = newest_session_file(sessions_dir)?;
    read_session_id(&newest)
}

/// The lexically-greatest `*.jsonl` filename in `sessions_dir` (== newest given
/// the `<ISO-timestamp>_<uuid>.jsonl` naming), as a full path. `None` when the
/// dir is missing/unreadable or holds no `.jsonl` files.
fn newest_session_file(sessions_dir: &Path) -> Option<std::path::PathBuf> {
    let entries = fs::read_dir(sessions_dir).ok()?;
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .max_by(|a, b| a.file_name().cmp(&b.file_name()))
}

/// Read the `id` from a session file's header line (the first non-empty line, a
/// JSON object with an `id` string). `None` on any read/parse failure or a
/// missing/empty id — the caller treats that as "not resumable".
fn read_session_id(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line).ok()?;
        if read == 0 {
            return None; // EOF before any non-empty line.
        }
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line.trim()).ok()?;
        let id = value.get("id").and_then(Value::as_str)?;
        if id.is_empty() {
            return None;
        }
        return Some(id.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, contents: &str) {
        fs::write(dir.join(name), contents).unwrap();
    }

    #[test]
    fn reads_id_from_header_line() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.jsonl");
        fs::write(
            &path,
            "{\"type\":\"session\",\"version\":3,\"id\":\"abc-123\"}\n\
             {\"type\":\"message_update\"}\n",
        )
        .unwrap();
        assert_eq!(read_session_id(&path).as_deref(), Some("abc-123"));
    }

    #[test]
    fn reads_id_skipping_leading_blank_lines() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.jsonl");
        fs::write(&path, "\n   \n{\"id\":\"xyz\"}\n").unwrap();
        assert_eq!(read_session_id(&path).as_deref(), Some("xyz"));
    }

    #[test]
    fn malformed_header_yields_none() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.jsonl");
        fs::write(&path, "not json at all\n").unwrap();
        assert_eq!(read_session_id(&path), None);
    }

    #[test]
    fn header_without_id_yields_none() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.jsonl");
        fs::write(&path, "{\"type\":\"session\",\"version\":3}\n").unwrap();
        assert_eq!(read_session_id(&path), None);
    }

    #[test]
    fn empty_file_yields_none() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.jsonl");
        fs::write(&path, "").unwrap();
        assert_eq!(read_session_id(&path), None);
    }

    #[test]
    fn selects_newest_by_filename_ordering() {
        let temp = tempfile::tempdir().unwrap();
        // Lexical order == chronological for ISO-timestamp-led names.
        write(
            temp.path(),
            "2024-01-01T00:00:00Z_aaa.jsonl",
            "{\"id\":\"old\"}\n",
        );
        write(
            temp.path(),
            "2024-06-15T12:30:00Z_bbb.jsonl",
            "{\"id\":\"newest\"}\n",
        );
        write(
            temp.path(),
            "2024-03-10T08:00:00Z_ccc.jsonl",
            "{\"id\":\"middle\"}\n",
        );
        assert_eq!(newest_session_id(temp.path()).as_deref(), Some("newest"));
    }

    #[test]
    fn ignores_non_jsonl_files_when_selecting_newest() {
        let temp = tempfile::tempdir().unwrap();
        // A later-sorting non-jsonl file must not win.
        write(
            temp.path(),
            "2024-01-01T00:00:00Z_a.jsonl",
            "{\"id\":\"win\"}\n",
        );
        write(temp.path(), "zzzz-not-a-session.log", "garbage\n");
        write(temp.path(), "README.txt", "hi\n");
        assert_eq!(newest_session_id(temp.path()).as_deref(), Some("win"));
    }

    #[test]
    fn empty_dir_yields_none() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(newest_session_id(temp.path()), None);
    }

    #[test]
    fn missing_dir_yields_none() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("does-not-exist");
        assert_eq!(newest_session_id(&missing), None);
    }

    #[test]
    fn list_returns_sessions_newest_first() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            "2024-01-01T00:00:00Z_aaa.jsonl",
            "{\"type\":\"session\",\"id\":\"old\"}\n",
        );
        write(
            temp.path(),
            "2024-06-15T12:30:00Z_bbb.jsonl",
            "{\"type\":\"session\",\"id\":\"newest\"}\n",
        );
        write(
            temp.path(),
            "2024-03-10T08:00:00Z_ccc.jsonl",
            "{\"type\":\"session\",\"id\":\"middle\"}\n",
        );
        let ids: Vec<String> = list(temp.path()).into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["newest", "middle", "old"]);
    }

    #[test]
    fn list_reads_start_time_from_filename_and_header_label() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            "2024-06-15T12:30:00Z_bbb.jsonl",
            "{\"type\":\"session\",\"id\":\"x\",\"label\":\"My chat\"}\n",
        );
        let entries = list(temp.path());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "x");
        assert_eq!(
            entries[0].start_time.as_deref(),
            Some("2024-06-15T12:30:00Z")
        );
        assert_eq!(entries[0].label, "My chat");
    }

    #[test]
    fn list_falls_back_to_file_stem_label_when_header_has_none() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            "2024-06-15T12:30:00Z_bbb.jsonl",
            "{\"type\":\"session\",\"id\":\"x\"}\n",
        );
        let entries = list(temp.path());
        assert_eq!(entries[0].label, "2024-06-15T12:30:00Z_bbb");
    }

    #[test]
    fn list_reads_only_the_header_line_not_message_bodies() {
        let temp = tempfile::tempdir().unwrap();
        // A body line that, if ever parsed as a header, would carry a different
        // id. `list` must report the header id only.
        write(
            temp.path(),
            "2024-06-15T12:30:00Z_bbb.jsonl",
            "{\"type\":\"session\",\"id\":\"header-id\"}\n\
             {\"type\":\"message_update\",\"id\":\"body-id\"}\n",
        );
        let entries = list(temp.path());
        assert_eq!(entries[0].id, "header-id");
    }

    #[test]
    fn list_skips_malformed_and_headerless_files() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            "2024-01-01T00:00:00Z_good.jsonl",
            "{\"type\":\"session\",\"id\":\"good\"}\n",
        );
        write(
            temp.path(),
            "2024-02-02T00:00:00Z_broken.jsonl",
            "not json\n",
        );
        write(temp.path(), "2024-03-03T00:00:00Z_empty.jsonl", "");
        write(
            temp.path(),
            "2024-04-04T00:00:00Z_noid.jsonl",
            "{\"type\":\"session\",\"version\":3}\n",
        );
        let ids: Vec<String> = list(temp.path()).into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["good"]);
    }

    #[test]
    fn list_ignores_non_jsonl_files() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            "2024-01-01T00:00:00Z_a.jsonl",
            "{\"id\":\"win\"}\n",
        );
        write(temp.path(), "zzzz-not-a-session.log", "garbage\n");
        let ids: Vec<String> = list(temp.path()).into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["win"]);
    }

    #[test]
    fn list_empty_dir_yields_empty() {
        let temp = tempfile::tempdir().unwrap();
        assert!(list(temp.path()).is_empty());
    }

    #[test]
    fn list_missing_dir_yields_empty() {
        let temp = tempfile::tempdir().unwrap();
        assert!(list(&temp.path().join("nope")).is_empty());
    }

    #[test]
    fn list_is_bounded_to_the_limit() {
        let temp = tempfile::tempdir().unwrap();
        for i in 0..(LIST_LIMIT + 10) {
            write(
                temp.path(),
                &format!("2024-01-01T00:00:{i:02}Z_s.jsonl"),
                "{\"type\":\"session\",\"id\":\"s\"}\n",
            );
        }
        assert_eq!(list(temp.path()).len(), LIST_LIMIT);
    }

    #[test]
    fn list_label_is_truncated() {
        let temp = tempfile::tempdir().unwrap();
        let long = "x".repeat(LABEL_MAX + 50);
        write(
            temp.path(),
            "2024-06-15T12:30:00Z_bbb.jsonl",
            &format!("{{\"type\":\"session\",\"id\":\"x\",\"label\":\"{long}\"}}\n"),
        );
        let entries = list(temp.path());
        assert_eq!(entries[0].label.chars().count(), LABEL_MAX);
    }

    #[test]
    fn malformed_newest_file_yields_none_even_with_valid_older() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            "2024-01-01T00:00:00Z_old.jsonl",
            "{\"id\":\"valid-old\"}\n",
        );
        // Newest by name is corrupt: we must NOT silently fall back to the older
        // session — the caller opens a fresh session instead.
        write(
            temp.path(),
            "2024-09-09T09:09:09Z_new.jsonl",
            "totally broken\n",
        );
        assert_eq!(newest_session_id(temp.path()), None);
    }
}
