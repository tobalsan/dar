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
