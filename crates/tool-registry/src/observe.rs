//! Tool-call observability: redaction + truncation of args/results and a
//! compact [`ToolCallObservation`] carrying tool name, read/write metadata,
//! success/failure, duration, and a redacted+truncated args/result summary.
//!
//! The goals (ALG-263):
//!   - Every tool call is observable for debugging — name, status, duration,
//!     and a *summary* of args + result — without dumping full raw payloads.
//!   - Host secrets never appear in a log. The bridge process loads the agent's
//!     `.env` (the same keys `runner-core::scrub_loaded_env` strips from child
//!     spawns); we build a [`Redactor`] from those exact secret *values* so any
//!     value that leaks into an arg or result string is masked before logging.
//!   - Redaction is value-based (not key-name heuristics) so it catches a secret
//!     no matter where it surfaces, plus a light token-shape pass for
//!     bearer/AKIA-style strings that never came from `.env`.

use std::time::Duration;

use serde_json::Value;

use crate::{ToolAccess, ToolOutcome};

/// Replacement text substituted for any redacted secret.
pub const REDACTED: &str = "[REDACTED]";

/// Max length (in bytes, on char boundaries) of a redacted args/result summary
/// before it is truncated. Keeps logs bounded — "no full raw result dumps".
pub const DEFAULT_MAX_SUMMARY_LEN: usize = 512;

/// Masks host secrets out of strings before they are logged.
///
/// Built primarily from the concrete secret *values* loaded from the agent's
/// `.env` (via [`Redactor::from_env_keys`], reusing the same scrub registry that
/// keeps those keys out of child spawns). Any occurrence of one of those values
/// in a tool's args or result is replaced with [`REDACTED`]. A secondary
/// token-shape pass masks common credential formats (e.g. `sk-...`, `AKIA...`,
/// long hex/base64-ish runs) that may appear without having come from `.env`.
#[derive(Clone, Default)]
pub struct Redactor {
    /// Concrete secret values to mask wherever they appear. Sorted longest-first
    /// so overlapping secrets redact the larger match.
    secrets: Vec<String>,
    /// Whether to additionally apply token-shape heuristics.
    token_shapes: bool,
}

impl Redactor {
    /// Add values without dropping previously known secrets. Bridge processes
    /// retain old values too: an extension may still have cached them.
    pub fn extend_secret_values<I, S>(&mut self, values: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut next = self.secrets.clone();
        next.extend(values.into_iter().map(Into::into));
        *self = Self::from_secret_values(next);
    }
    /// A redactor that only applies token-shape heuristics (no known secret
    /// values). Useful where the `.env` scrub set is unavailable.
    pub fn token_shapes_only() -> Self {
        Self {
            secrets: Vec::new(),
            token_shapes: true,
        }
    }

    /// Build a redactor from explicit secret values plus the token-shape pass.
    /// Empty/whitespace-only and very short values are ignored to avoid masking
    /// innocuous substrings.
    pub fn from_secret_values<I, S>(values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut secrets: Vec<String> = values
            .into_iter()
            .map(|v| v.into().trim().to_string())
            .filter(|v| v.len() >= 4)
            .collect();
        // Longest first so a secret that contains another redacts as one unit.
        secrets.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        secrets.dedup();
        Self {
            secrets,
            token_shapes: true,
        }
    }

    /// Build a redactor from the current values of the given env keys — typically
    /// the `.env`-loaded keys recorded in `runner-core`'s scrub registry, so the
    /// bridge masks exactly the secrets it loaded into its own process.
    pub fn from_env_keys<I, S>(keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let values = keys
            .into_iter()
            .filter_map(|k| std::env::var(k.as_ref()).ok());
        Self::from_secret_values(values)
    }

    /// Redact a string: mask every known secret value, then apply token-shape
    /// heuristics. The result never contains a known secret value.
    pub fn redact(&self, input: &str) -> String {
        let mut out = input.to_string();
        for secret in &self.secrets {
            if !secret.is_empty() && out.contains(secret.as_str()) {
                out = out.replace(secret.as_str(), REDACTED);
            }
        }
        if self.token_shapes {
            out = redact_token_shapes(&out);
        }
        out
    }

    /// Redact a JSON value by walking it and redacting every string (object keys
    /// are preserved; only values are scrubbed). Returns a JSON value so callers
    /// can re-serialize compactly.
    pub fn redact_value(&self, value: &Value) -> Value {
        match value {
            Value::String(s) => Value::String(self.redact(s)),
            Value::Array(items) => {
                Value::Array(items.iter().map(|v| self.redact_value(v)).collect())
            }
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), self.redact_value(v)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

/// Mask credential-shaped tokens that did not necessarily come from `.env`:
/// `sk-`/`pk-`/`ghp_`-prefixed keys, AWS `AKIA…`, and long opaque runs. This is
/// a best-effort net, not a parser — known `.env` values are handled exactly by
/// the value pass above.
fn redact_token_shapes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for token in split_keeping_delims(input) {
        if is_secret_shaped(token) {
            out.push_str(REDACTED);
        } else {
            out.push_str(token);
        }
    }
    out
}

/// Split into alternating runs of token characters and delimiters, preserving
/// everything so the rejoined string differs only where tokens were masked.
fn split_keeping_delims(input: &str) -> Vec<&str> {
    let is_tok = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/' | '+' | '.');
    let mut parts = Vec::new();
    let bytes = input.char_indices();
    let mut start = 0usize;
    let mut cur_tok: Option<bool> = None;
    for (idx, c) in bytes {
        let this = is_tok(c);
        match cur_tok {
            Some(prev) if prev == this => {}
            Some(_) => {
                parts.push(&input[start..idx]);
                start = idx;
            }
            None => {}
        }
        cur_tok = Some(this);
    }
    if start < input.len() {
        parts.push(&input[start..]);
    }
    parts
}

/// Heuristic: does this token look like a credential? Conservative on length so
/// ordinary words/ids are not masked.
/// Canonical UUID shape (8-4-4-4-12 hex). These are resource identifiers
/// (Linear issue/state/project IDs, etc.), not secrets — the agent needs them
/// to make mutations, so they must never be redacted by the token-shape pass.
fn is_uuid_shaped(token: &str) -> bool {
    let b = token.as_bytes();
    if b.len() != 36 {
        return false;
    }
    b.iter().enumerate().all(|(i, &c)| match i {
        8 | 13 | 18 | 23 => c == b'-',
        _ => c.is_ascii_hexdigit(),
    })
}

fn is_secret_shaped(token: &str) -> bool {
    // UUIDs are identifiers, not secrets — exempt them before the long-run check.
    if is_uuid_shaped(token) {
        return false;
    }
    let lower = token.to_ascii_lowercase();
    if (lower.starts_with("sk-")
        || lower.starts_with("pk-")
        || lower.starts_with("ghp_")
        || lower.starts_with("gho_")
        || lower.starts_with("xox")
        || token.starts_with("AKIA")
        || lower.starts_with("bearer"))
        && token.len() >= 8
    {
        return true;
    }
    // Long high-entropy-ish opaque run (letters+digits, >= 32 chars).
    if token.len() >= 32
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+' | '/'))
        && token.chars().any(|c| c.is_ascii_digit())
        && token.chars().any(|c| c.is_ascii_alphabetic())
    {
        return true;
    }
    false
}

/// Truncate `s` to at most `max` bytes on a char boundary, appending an
/// ellipsis marker noting how many bytes were dropped. A short string is
/// returned unchanged.
pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let dropped = s.len() - end;
    format!("{}…(+{dropped} bytes)", &s[..end])
}

/// A compact, log-safe record of one tool call. Carries everything the
/// acceptance criteria require — name, status, duration, read/write metadata —
/// plus a redacted+truncated args and result summary (never a full raw dump).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCallObservation {
    pub tool: String,
    pub access: ToolAccess,
    /// `"ok"` or `"error"` — derived from the structured outcome's `is_error`.
    pub status: &'static str,
    pub duration: Duration,
    /// Redacted + truncated JSON of the call args.
    pub args_summary: String,
    /// Redacted + truncated result text.
    pub result_summary: String,
}

impl ToolCallObservation {
    /// Build an observation from a finished call. Args and result are redacted
    /// (secrets masked) then truncated to [`DEFAULT_MAX_SUMMARY_LEN`].
    pub fn build(
        tool: &str,
        access: ToolAccess,
        outcome: &ToolOutcome,
        duration: Duration,
        args: &Value,
        redactor: &Redactor,
    ) -> Self {
        let args_redacted = redactor.redact_value(args);
        let args_summary = truncate(
            &serde_json::to_string(&args_redacted).unwrap_or_else(|_| "<unserializable>".into()),
            DEFAULT_MAX_SUMMARY_LEN,
        );
        let result_summary = truncate(&redactor.redact(&outcome.text), DEFAULT_MAX_SUMMARY_LEN);
        Self {
            tool: tool.to_string(),
            access,
            status: if outcome.is_error { "error" } else { "ok" },
            duration,
            args_summary,
            result_summary,
        }
    }

    /// A single-line, human-readable log message. Stable field order; not a
    /// stable format contract (tests assert on field *presence*, not layout).
    pub fn log_line(&self) -> String {
        format!(
            "tool={} status={} duration_ms={} access={} args={} result={}",
            self.tool,
            self.status,
            self.duration.as_millis(),
            self.access.label(),
            self.args_summary,
            self.result_summary,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redactor_masks_known_secret_values_anywhere() {
        let r = Redactor::from_secret_values(["super-secret-token-value"]);
        let masked = r.redact("auth header: Bearer super-secret-token-value done");
        assert!(!masked.contains("super-secret-token-value"));
        assert!(masked.contains(REDACTED));
    }

    #[test]
    fn redactor_trims_known_secret_values_before_storing() {
        let r = Redactor::from_secret_values(["  abcd-secret  "]);
        let masked = r.redact("token=abcd-secret");
        assert!(!masked.contains("abcd-secret"));
        assert!(masked.contains(REDACTED));
    }

    #[test]
    fn redactor_refresh_keeps_old_and_new_values() {
        let mut r = Redactor::from_secret_values(["old-bridge-secret"]);
        r.extend_secret_values(["new-bridge-secret"]);
        assert_eq!(r.redact("old-bridge-secret"), REDACTED);
        assert_eq!(r.redact("new-bridge-secret"), REDACTED);
    }

    #[test]
    fn redactor_masks_nested_json_string_values_only() {
        let r = Redactor::from_secret_values(["hunter2pass"]);
        let v = json!({ "password": "hunter2pass", "user": "alice", "n": 3 });
        let out = r.redact_value(&v);
        assert_eq!(out["password"], REDACTED);
        // Non-secret values and keys are preserved.
        assert_eq!(out["user"], "alice");
        assert_eq!(out["n"], 3);
    }

    #[test]
    fn token_shapes_catch_credentials_without_env() {
        let r = Redactor::token_shapes_only();
        let masked = r.redact("key=sk-ABCDEF0123456789 and AKIAIOSFODNN7EXAMPLE");
        assert!(!masked.contains("sk-ABCDEF0123456789"));
        assert!(!masked.contains("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn uuids_are_not_redacted_as_secrets() {
        let r = Redactor::token_shapes_only();
        let uuid = "a1b2c3d4-5e6f-7890-abcd-ef1234567890";
        let masked = r.redact(&format!("stateId={uuid} done"));
        // Linear resource IDs must survive so the agent can use them in mutations.
        assert!(
            masked.contains(uuid),
            "UUID should not be redacted: {masked}"
        );
        assert!(!masked.contains(REDACTED));
        // But real opaque credentials of similar length still get masked.
        let secret = "AbCd1234EfGh5678IjKl9012MnOp3456Qr";
        assert!(r.redact(secret).contains(REDACTED));
    }

    #[test]
    fn truncate_bounds_length_and_marks_drop() {
        let long = "x".repeat(DEFAULT_MAX_SUMMARY_LEN + 100);
        let out = truncate(&long, DEFAULT_MAX_SUMMARY_LEN);
        assert!(out.len() < long.len());
        assert!(out.contains("bytes)"));
    }

    #[test]
    fn truncate_keeps_short_strings_intact() {
        assert_eq!(truncate("short", DEFAULT_MAX_SUMMARY_LEN), "short");
    }

    #[test]
    fn observation_carries_required_fields_and_redacts() {
        let r = Redactor::from_secret_values(["topsecret-value-123"]);
        let outcome = ToolOutcome::error("failed using topsecret-value-123 internally");
        let obs = ToolCallObservation::build(
            "jobs_create",
            ToolAccess {
                read: false,
                write: true,
            },
            &outcome,
            Duration::from_millis(42),
            &json!({ "token": "topsecret-value-123", "id": "j1" }),
            &r,
        );
        assert_eq!(obs.tool, "jobs_create");
        assert_eq!(obs.status, "error");
        assert_eq!(obs.duration, Duration::from_millis(42));
        assert!(obs.access.write);
        // Redaction reaches both args and result.
        assert!(!obs.args_summary.contains("topsecret-value-123"));
        assert!(!obs.result_summary.contains("topsecret-value-123"));
        // No raw dump: the id is preserved, but the secret is gone.
        assert!(obs.args_summary.contains("j1"));

        let line = obs.log_line();
        assert!(line.contains("tool=jobs_create"));
        assert!(line.contains("status=error"));
        assert!(line.contains("duration_ms=42"));
        assert!(line.contains("access=write"));
    }

    #[test]
    fn observation_truncates_huge_result() {
        let r = Redactor::default();
        let big = "A".repeat(5000);
        let obs = ToolCallObservation::build(
            "t",
            ToolAccess::default(),
            &ToolOutcome::ok(big.clone()),
            Duration::from_millis(1),
            &json!({}),
            &r,
        );
        assert!(obs.result_summary.len() <= DEFAULT_MAX_SUMMARY_LEN + 32);
        assert!(obs.result_summary.len() < big.len());
    }
}
