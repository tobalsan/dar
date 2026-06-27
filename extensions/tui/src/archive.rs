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

/// Upper bound on how many search hits `search` returns across all sessions, so
/// a common term can't flood the agent's context with locations.
pub const SEARCH_LIMIT: usize = 50;

/// Max characters of a matched message kept in a search hit's snippet — a
/// bounded window around the match, never the full message body.
pub const SNIPPET_MAX: usize = 200;

/// Hard maximum number of messages a single `read` call may return. This is the
/// single chokepoint that makes it impossible for a recall read to flood the
/// agent's context window: any request larger than this is clamped down to it.
pub const READ_MAX: usize = 20;

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

/// One message extracted from a session file: its position in the file
/// (`index`, counting only recognized user/assistant messages) and its role and
/// text. The text is the joined `text` content of the message envelope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    /// Zero-based position among the recognized user/assistant messages in the
    /// session, in file order. Stable across `search` and `read`.
    pub index: usize,
    /// `"user"` or `"assistant"`.
    pub role: String,
    /// The message's joined text content.
    pub text: String,
}

/// One search hit: a location, not a body. Identifies the session and the
/// message index where the term matched, plus a bounded snippet around the
/// match — never the full message text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchHit {
    pub session_id: String,
    pub message_index: usize,
    pub snippet: String,
}

/// Search all persisted sessions for a case-insensitive substring `query` over
/// user+assistant message text. Returns hits as locations
/// (`{session_id, message_index, snippet}`) newest-session-first, with each
/// snippet capped at [`SNIPPET_MAX`] chars and the total bounded to
/// [`SEARCH_LIMIT`]. An empty `query` or empty/missing corpus yields no hits.
/// Malformed lines and files are skipped, never fatal.
pub fn search(sessions_dir: &Path, query: &str) -> Vec<SearchHit> {
    let needle: String = lower_aligned_chars(query.trim()).into_iter().collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut files = match fs::read_dir(sessions_dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .collect::<Vec<_>>(),
        Err(_) => return Vec::new(),
    };
    // Newest-first, same ordering as `list`.
    files.sort_by(|a, b| b.file_name().cmp(&a.file_name()));

    let mut hits = Vec::new();
    for path in files {
        let Some(id) = read_session_id_opt(&path) else {
            continue; // headerless / malformed / no id → skip the whole file.
        };
        for msg in read_messages(&path) {
            if hits.len() >= SEARCH_LIMIT {
                return hits;
            }
            if let Some(snippet) = snippet_for(&msg.text, &needle) {
                hits.push(SearchHit {
                    session_id: id.clone(),
                    message_index: msg.index,
                    snippet,
                });
            }
        }
    }
    hits
}

/// Read a ranged slice of a session's user+assistant messages, starting at
/// `start` (zero-based message index) and returning at most `count` messages —
/// but never more than [`READ_MAX`], the hard cap. A `count` above the cap is
/// clamped down; a `start` past the end yields an empty slice. Successive calls
/// with advancing `start` page through a long session. Malformed lines are
/// skipped; a missing/unreadable file or unknown `session_id` yields an empty
/// slice.
pub fn read(sessions_dir: &Path, session_id: &str, start: usize, count: usize) -> Vec<Message> {
    let Some(path) = session_file_by_id(sessions_dir, session_id) else {
        return Vec::new();
    };
    let capped = count.min(READ_MAX);
    if capped == 0 {
        return Vec::new();
    }
    read_messages(&path)
        .into_iter()
        .skip(start)
        .take(capped)
        .collect()
}

/// Find the session file whose header `id` equals `session_id`. Scans the dir's
/// `.jsonl` files reading only each header line. `None` when the dir is
/// unreadable or no file carries that id.
fn session_file_by_id(sessions_dir: &Path, session_id: &str) -> Option<std::path::PathBuf> {
    if session_id.is_empty() {
        return None;
    }
    let entries = fs::read_dir(sessions_dir).ok()?;
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .find(|path| read_session_id_opt(path).as_deref() == Some(session_id))
}

/// Like [`read_session_id`] but kept distinct so `search`/`read` can resolve a
/// file's id without coupling to the resume path.
fn read_session_id_opt(path: &Path) -> Option<String> {
    let header = read_header_value(path)?;
    let id = header.get("id").and_then(Value::as_str)?;
    if id.is_empty() {
        return None;
    }
    Some(id.to_string())
}

/// Read all recognized user/assistant messages from a session file in file
/// order, assigning each a stable zero-based `index`. Lines that don't parse or
/// aren't user/assistant message envelopes are skipped (malformed-line
/// tolerance); the header line is naturally skipped as it carries no message
/// role. A missing/unreadable file yields an empty list.
fn read_messages(path: &Path) -> Vec<Message> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return Vec::new(),
    };
    let reader = BufReader::new(file);
    let mut messages = Vec::new();
    let mut index = 0;
    for line in reader.lines() {
        let Ok(line) = line else { continue }; // skip an unreadable line, not the rest.
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue; // malformed/partial line — tolerate, skip.
        };
        if let Some((role, text)) = message_role_text(&value) {
            messages.push(Message { index, role, text });
            index += 1;
        }
    }
    messages
}

/// Extract `(role, text)` from a persisted line when it is a user/assistant
/// message. Tolerates both the wrapped pi shape
/// (`{"type":"message_end"|..., "message":{"role","content"}}`) and a flat
/// `{"role","content"}` shape. `content` may be a plain string or an array of
/// `{"type":"text","text":...}` / string entries; non-text blocks are ignored.
/// `None` for anything that isn't a user/assistant message with non-empty text.
fn message_role_text(value: &Value) -> Option<(String, String)> {
    let message = value.get("message").unwrap_or(value);
    let role = message.get("role").and_then(Value::as_str)?;
    if role != "user" && role != "assistant" {
        return None;
    }
    let content = message.get("content")?;
    let text = content_text(content);
    if text.is_empty() {
        return None;
    }
    Some((role.to_string(), text))
}

/// Join the text of a message `content` value: a plain string is itself; an
/// array joins each entry's text (string entries verbatim, object entries via
/// their `"text"` field), skipping non-text blocks. Anything else yields an
/// empty string.
fn content_text(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    let Some(arr) = content.as_array() else {
        return String::new();
    };
    let parts: Vec<&str> = arr
        .iter()
        .filter_map(|entry| {
            if let Some(s) = entry.as_str() {
                Some(s)
            } else {
                entry.get("text").and_then(Value::as_str)
            }
        })
        .filter(|s| !s.is_empty())
        .collect();
    parts.join(" ")
}

/// Build a bounded snippet around the first case-insensitive occurrence of
/// `needle` (already lowercased) in `text`, or `None` when it doesn't occur.
/// The window is centered on the match and capped at [`SNIPPET_MAX`] chars so a
/// hit reveals a location, not the whole message body.
///
/// All matching and slicing is done in `char` space so the snippet is a clean
/// character window, never splitting a multi-byte sequence, and so a
/// case-folding that changes byte length (e.g. Turkish `İ`) can't desync a byte
/// offset against the original text.
fn snippet_for(text: &str, needle: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let needle_chars: Vec<char> = needle.chars().collect();
    let match_char = find_chars_ci(&chars, &needle_chars)?;
    if chars.len() <= SNIPPET_MAX {
        return Some(text.to_string());
    }
    // Reserve room for the leading/trailing ellipsis markers inside the cap so
    // the *final returned* snippet — markers included — never exceeds
    // SNIPPET_MAX chars. Worst case both markers are present, so the body
    // window is SNIPPET_MAX minus two marker chars.
    let window = SNIPPET_MAX.saturating_sub(2);
    let pad = window.saturating_sub(needle_chars.len()) / 2;
    let start = match_char.saturating_sub(pad);
    let end = (start + window).min(chars.len());
    let start = end.saturating_sub(window);
    let mut snippet = String::new();
    if start > 0 {
        snippet.push('…');
    }
    snippet.extend(chars[start..end].iter());
    if end < chars.len() {
        snippet.push('…');
    }
    debug_assert!(snippet.chars().count() <= SNIPPET_MAX);
    Some(snippet)
}

/// Index of the first char-position in `haystack` where the (already lowercased)
/// `needle` chars occur case-insensitively, or `None`. Works entirely in `char`
/// space, lowercasing each haystack char on the fly so the returned index is a
/// valid offset into the caller's `chars` vector regardless of byte-length
/// changes from case folding.
fn find_chars_ci(haystack: &[char], needle: &[char]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    let lowered: Vec<char> = haystack.iter().map(|c| lower_aligned_char(*c)).collect();
    lowered
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Lowercase a string into a `Vec<char>` using a 1:1 char mapping
/// ([`lower_aligned_char`]), so a folded string keeps the same char count as
/// the source. Used to fold both the search needle and haystack the same way,
/// keeping match indices valid offsets into the original `chars` vector.
fn lower_aligned_chars(s: &str) -> Vec<char> {
    s.chars().map(lower_aligned_char).collect()
}

/// Lowercase a single char without ever expanding it to more than one char:
/// chars whose lowercase form is a single char fold normally; the rare chars
/// whose lowercase expands (e.g. `İ` -> `i` + combining dot) are left as-is.
/// This keeps case-insensitive matching index-stable; full Unicode-correct case
/// folding isn't needed for lexical recall.
fn lower_aligned_char(c: char) -> char {
    let mut it = c.to_lowercase();
    match (it.next(), it.next()) {
        (Some(single), None) => single,
        _ => c,
    }
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

    /// A session file with a header plus a few user/assistant message lines in
    /// the wrapped pi shape.
    fn session_with_messages(id: &str, msgs: &[(&str, &str)]) -> String {
        let mut out = format!("{{\"type\":\"session\",\"id\":\"{id}\"}}\n");
        for (role, text) in msgs {
            out.push_str(&format!(
                "{{\"type\":\"message_end\",\"message\":{{\"role\":\"{role}\",\"content\":[{{\"type\":\"text\",\"text\":{}}}]}}}}\n",
                serde_json::Value::String((*text).to_string())
            ));
        }
        out
    }

    #[test]
    fn search_finds_term_with_session_id_and_message_index() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            "2024-06-15T12:30:00Z_a.jsonl",
            &session_with_messages(
                "sess-a",
                &[
                    ("user", "tell me about widgets"),
                    ("assistant", "a widget is a small gadget"),
                    ("user", "thanks"),
                ],
            ),
        );
        let hits = search(temp.path(), "gadget");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "sess-a");
        assert_eq!(hits[0].message_index, 1);
        assert!(hits[0].snippet.contains("gadget"));
    }

    #[test]
    fn search_is_case_insensitive_over_user_and_assistant() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            "2024-06-15T12:30:00Z_a.jsonl",
            &session_with_messages(
                "sess-a",
                &[("user", "FooBar is great"), ("assistant", "indeed FOOBAR")],
            ),
        );
        let hits = search(temp.path(), "foobar");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].message_index, 0);
        assert_eq!(hits[1].message_index, 1);
    }

    #[test]
    fn search_snippet_is_bounded_and_not_the_full_body() {
        let temp = tempfile::tempdir().unwrap();
        let long = format!("{}NEEDLE{}", "x".repeat(500), "y".repeat(500));
        write(
            temp.path(),
            "2024-06-15T12:30:00Z_a.jsonl",
            &session_with_messages("sess-a", &[("assistant", &long)]),
        );
        let hits = search(temp.path(), "needle");
        assert_eq!(hits.len(), 1);
        // The full returned snippet field — ellipsis markers included — never
        // exceeds the cap, and is far shorter than the ~1000-char body.
        let len = hits[0].snippet.chars().count();
        assert!(len <= SNIPPET_MAX, "snippet len {len}");
        assert!(hits[0].snippet.to_lowercase().contains("needle"));
    }

    #[test]
    fn search_tolerates_malformed_lines() {
        let temp = tempfile::tempdir().unwrap();
        let mut contents = String::from("{\"type\":\"session\",\"id\":\"sess-a\"}\n");
        contents.push_str("not json at all\n");
        contents.push_str("{\"type\":\"message_end\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"find the needle here\"}]}}\n");
        contents.push_str("{ broken partial line\n");
        write(temp.path(), "2024-06-15T12:30:00Z_a.jsonl", &contents);
        let hits = search(temp.path(), "needle");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].message_index, 0);
    }

    #[test]
    fn search_skips_headerless_files_and_handles_empty_corpus() {
        let temp = tempfile::tempdir().unwrap();
        // No id in header → whole file skipped.
        write(
            temp.path(),
            "2024-06-15T12:30:00Z_noid.jsonl",
            "{\"type\":\"session\"}\n{\"type\":\"message_end\",\"message\":{\"role\":\"user\",\"content\":\"needle\"}}\n",
        );
        assert!(search(temp.path(), "needle").is_empty());
        // Empty / missing corpus.
        let empty = tempfile::tempdir().unwrap();
        assert!(search(empty.path(), "needle").is_empty());
        assert!(search(&empty.path().join("nope"), "needle").is_empty());
    }

    #[test]
    fn search_snippet_with_match_in_middle_stays_within_cap_including_markers() {
        // Match centered in a long body produces both leading and trailing
        // ellipsis markers; the full returned field must still be <= SNIPPET_MAX.
        let temp = tempfile::tempdir().unwrap();
        let long = format!("{}NEEDLE{}", "x".repeat(500), "y".repeat(500));
        write(
            temp.path(),
            "2024-06-15T12:30:00Z_a.jsonl",
            &session_with_messages("sess-a", &[("assistant", &long)]),
        );
        let hits = search(temp.path(), "needle");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.starts_with('…'));
        assert!(hits[0].snippet.ends_with('…'));
        assert!(
            hits[0].snippet.chars().count() <= SNIPPET_MAX,
            "snippet len {}",
            hits[0].snippet.chars().count()
        );
    }

    #[test]
    fn search_snippet_handles_case_folding_that_changes_byte_length() {
        // `İ` (U+0130) lowercases to two chars; a long message made of it before
        // the match must neither panic nor mis-slice — it must still locate the
        // term and return a bounded snippet.
        let temp = tempfile::tempdir().unwrap();
        let body = format!("{}xneedle here", "İ".repeat(300));
        write(
            temp.path(),
            "2024-06-15T12:30:00Z_a.jsonl",
            &session_with_messages("sess-a", &[("assistant", &body)]),
        );
        let hits = search(temp.path(), "needle");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].message_index, 0);
        assert!(hits[0].snippet.to_lowercase().contains("needle"));
        let len = hits[0].snippet.chars().count();
        assert!(len <= SNIPPET_MAX, "snippet len {len}");
    }

    #[test]
    fn search_empty_query_yields_no_hits() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            "2024-06-15T12:30:00Z_a.jsonl",
            &session_with_messages("sess-a", &[("user", "anything")]),
        );
        assert!(search(temp.path(), "").is_empty());
        assert!(search(temp.path(), "   ").is_empty());
    }

    #[test]
    fn search_is_bounded_to_the_limit() {
        let temp = tempfile::tempdir().unwrap();
        let msgs: Vec<(&str, &str)> = (0..(SEARCH_LIMIT + 20))
            .map(|_| ("user", "needle present"))
            .collect();
        write(
            temp.path(),
            "2024-06-15T12:30:00Z_a.jsonl",
            &session_with_messages("sess-a", &msgs),
        );
        assert_eq!(search(temp.path(), "needle").len(), SEARCH_LIMIT);
    }

    #[test]
    fn read_returns_requested_range() {
        let temp = tempfile::tempdir().unwrap();
        let msgs: Vec<(&str, String)> = (0..10).map(|i| ("user", format!("msg {i}"))).collect();
        let refs: Vec<(&str, &str)> = msgs.iter().map(|(r, t)| (*r, t.as_str())).collect();
        write(
            temp.path(),
            "2024-06-15T12:30:00Z_a.jsonl",
            &session_with_messages("sess-a", &refs),
        );
        let slice = read(temp.path(), "sess-a", 3, 4);
        assert_eq!(slice.len(), 4);
        assert_eq!(slice[0].index, 3);
        assert_eq!(slice[0].text, "msg 3");
        assert_eq!(slice[3].index, 6);
        assert_eq!(slice[3].text, "msg 6");
    }

    #[test]
    fn read_clamps_oversized_request_to_hard_cap() {
        let temp = tempfile::tempdir().unwrap();
        let msgs: Vec<(&str, String)> = (0..(READ_MAX + 50))
            .map(|i| ("user", format!("m{i}")))
            .collect();
        let refs: Vec<(&str, &str)> = msgs.iter().map(|(r, t)| (*r, t.as_str())).collect();
        write(
            temp.path(),
            "2024-06-15T12:30:00Z_a.jsonl",
            &session_with_messages("sess-a", &refs),
        );
        let slice = read(temp.path(), "sess-a", 0, READ_MAX + 1000);
        assert_eq!(slice.len(), READ_MAX);
    }

    #[test]
    fn read_pages_through_a_long_session() {
        let temp = tempfile::tempdir().unwrap();
        let total = READ_MAX * 2 + 5;
        let msgs: Vec<(&str, String)> = (0..total).map(|i| ("user", format!("m{i}"))).collect();
        let refs: Vec<(&str, &str)> = msgs.iter().map(|(r, t)| (*r, t.as_str())).collect();
        write(
            temp.path(),
            "2024-06-15T12:30:00Z_a.jsonl",
            &session_with_messages("sess-a", &refs),
        );
        let mut seen = Vec::new();
        let mut start = 0;
        loop {
            let page = read(temp.path(), "sess-a", start, READ_MAX);
            if page.is_empty() {
                break;
            }
            for m in &page {
                seen.push(m.index);
            }
            start += page.len();
        }
        let expected: Vec<usize> = (0..total).collect();
        assert_eq!(seen, expected);
    }

    #[test]
    fn read_past_end_is_empty_and_unknown_session_is_empty() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            "2024-06-15T12:30:00Z_a.jsonl",
            &session_with_messages("sess-a", &[("user", "only one")]),
        );
        assert!(read(temp.path(), "sess-a", 99, READ_MAX).is_empty());
        assert!(read(temp.path(), "missing-id", 0, READ_MAX).is_empty());
        assert!(read(temp.path(), "sess-a", 0, 0).is_empty());
    }

    #[test]
    fn read_tolerates_malformed_lines_and_string_content() {
        let temp = tempfile::tempdir().unwrap();
        let mut contents = String::from("{\"type\":\"session\",\"id\":\"sess-a\"}\n");
        contents.push_str("garbage line\n");
        // Flat shape + string content.
        contents.push_str("{\"role\":\"user\",\"content\":\"first\"}\n");
        contents.push_str("{ partial\n");
        contents.push_str("{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":\"second\"}}\n");
        write(temp.path(), "2024-06-15T12:30:00Z_a.jsonl", &contents);
        let slice = read(temp.path(), "sess-a", 0, READ_MAX);
        assert_eq!(slice.len(), 2);
        assert_eq!(slice[0].text, "first");
        assert_eq!(slice[0].index, 0);
        assert_eq!(slice[1].role, "assistant");
        assert_eq!(slice[1].text, "second");
        assert_eq!(slice[1].index, 1);
    }

    #[test]
    fn read_ignores_non_message_lines() {
        let temp = tempfile::tempdir().unwrap();
        let mut contents = String::from("{\"type\":\"session\",\"id\":\"sess-a\"}\n");
        // Tool/system noise that isn't a user/assistant message must not count.
        contents.push_str("{\"type\":\"message_update\"}\n");
        contents.push_str(
            "{\"type\":\"message_end\",\"message\":{\"role\":\"tool\",\"content\":\"x\"}}\n",
        );
        contents.push_str(
            "{\"type\":\"message_end\",\"message\":{\"role\":\"user\",\"content\":\"real\"}}\n",
        );
        write(temp.path(), "2024-06-15T12:30:00Z_a.jsonl", &contents);
        let slice = read(temp.path(), "sess-a", 0, READ_MAX);
        assert_eq!(slice.len(), 1);
        assert_eq!(slice[0].text, "real");
        assert_eq!(slice[0].index, 0);
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
