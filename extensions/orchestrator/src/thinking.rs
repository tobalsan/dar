//! Canonical reasoning-level (`thinking`, alias `effort`) config.
//!
//! One knob on the canonical scale `none | minimal | low | medium | high |
//! xhigh`, validated against the resolved runner's supported subset at
//! config-load / boot time. Unsupported or unknown levels are rejected with a
//! clean error naming the runner and its allowed values — we never clamp or
//! pass an unsupported value through.
//!
//! Per-runner mapping:
//!
//! | Runner             | Mechanism                          | Supported subset                              |
//! | ------------------ | ---------------------------------- | --------------------------------------------- |
//! | `pi`               | `--thinking <level>`               | none (→ pi's `off`), minimal, low, medium, high, xhigh |
//! | `codex`            | `-c model_reasoning_effort=<level>`| minimal, low, medium, high, xhigh             |
//! | `claude`/`claude-code` | `--effort <level>`             | low, medium, high, xhigh                      |
//! | `opencode`         | reasoningEffort (deferred, ALG-226)| —                                             |
//! | `cli` / `fake`     | ignored                            | —                                             |
//!
//! `opencode` mapping is intentionally deferred until the native OpenCode
//! runner lands (ALG-226); until then it is treated as an "ignore" runner.

/// The canonical level scale, in ascending order.
const CANONICAL: [&str; 6] = ["none", "minimal", "low", "medium", "high", "xhigh"];

/// Levels pi accepts via `--thinking` (after `none` → `off` mapping).
const PI_LEVELS: [&str; 6] = ["none", "minimal", "low", "medium", "high", "xhigh"];

/// Levels codex accepts via `-c model_reasoning_effort=<level>`.
const CODEX_LEVELS: [&str; 5] = ["minimal", "low", "medium", "high", "xhigh"];

/// Levels claude accepts via `--effort <level>`.
const CLAUDE_LEVELS: [&str; 4] = ["low", "medium", "high", "xhigh"];

/// Allowed levels for a resolved runner kind, or `None` when the runner ignores
/// the setting entirely (`cli`, `fake`, and any unknown runner).
fn allowed_levels(runner_kind: &str) -> Option<&'static [&'static str]> {
    match runner_kind {
        "pi" | "" => Some(&PI_LEVELS),
        "codex" => Some(&CODEX_LEVELS),
        "claude" | "claude-code" => Some(&CLAUDE_LEVELS),
        // `opencode` mapping deferred until ALG-226 lands; treat as ignore for now.
        _ => None,
    }
}

/// Validate a configured thinking level against the resolved runner's supported
/// subset. Absent level is always fine (no flag emitted). A numeric/budget
/// value (the removed pi token-budget semantics) is rejected pointing at the
/// canonical level scale. An unknown or unsupported level fails naming the
/// runner and its allowed values.
///
/// Runners that ignore the setting (`cli`, `fake`, unknown) accept any level
/// without error, but the level is never forwarded.
pub fn validate_thinking_for_runner(runner_kind: &str, level: Option<&str>) -> anyhow::Result<()> {
    let Some(level) = level else {
        return Ok(());
    };
    let level = level.trim();
    if level.is_empty() {
        return Ok(());
    }

    // Reject the removed numeric token-budget semantics outright, with a hint at
    // the level scale, regardless of runner.
    if level.chars().all(|c| c.is_ascii_digit()) {
        anyhow::bail!(
            "thinking/effort no longer accepts a numeric budget (got {level:?}); use a level word: {}",
            CANONICAL.join(", ")
        );
    }

    // Runners that ignore the setting accept anything; nothing is forwarded.
    let Some(allowed) = allowed_levels(runner_kind) else {
        return Ok(());
    };

    if !allowed.contains(&level) {
        anyhow::bail!(
            "invalid thinking/effort level {level:?} for runner {runner_kind:?}; allowed: {}",
            allowed.join(", ")
        );
    }
    Ok(())
}

/// Map a validated canonical level to the value pi expects for `--thinking`:
/// `none` becomes pi's `off`, every other level passes through unchanged.
pub fn pi_thinking_value(level: &str) -> &str {
    if level == "none" {
        "off"
    } else {
        level
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_level_is_ok_for_every_runner() {
        for runner in ["pi", "codex", "claude", "claude-code", "cli", "fake", "opencode"] {
            assert!(validate_thinking_for_runner(runner, None).is_ok());
            assert!(validate_thinking_for_runner(runner, Some("  ")).is_ok());
        }
    }

    #[test]
    fn pi_accepts_full_scale_including_none() {
        for level in ["none", "minimal", "low", "medium", "high", "xhigh"] {
            assert!(
                validate_thinking_for_runner("pi", Some(level)).is_ok(),
                "pi should accept {level}"
            );
        }
    }

    #[test]
    fn codex_rejects_none() {
        let err = validate_thinking_for_runner("codex", Some("none")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("codex"), "{msg}");
        assert!(msg.contains("minimal, low, medium, high, xhigh"), "{msg}");
    }

    #[test]
    fn codex_accepts_supported_levels() {
        for level in ["minimal", "low", "medium", "high", "xhigh"] {
            assert!(validate_thinking_for_runner("codex", Some(level)).is_ok());
        }
    }

    #[test]
    fn claude_rejects_minimal_and_none() {
        for bad in ["none", "minimal"] {
            let err = validate_thinking_for_runner("claude", Some(bad)).unwrap_err();
            assert!(err.to_string().contains("low, medium, high, xhigh"));
        }
        for runner in ["claude", "claude-code"] {
            for level in ["low", "medium", "high", "xhigh"] {
                assert!(validate_thinking_for_runner(runner, Some(level)).is_ok());
            }
        }
    }

    #[test]
    fn unknown_level_is_rejected_with_runner_and_allowed() {
        let err = validate_thinking_for_runner("pi", Some("bananas")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("bananas"), "{msg}");
        assert!(msg.contains("pi"), "{msg}");
    }

    #[test]
    fn numeric_budget_is_rejected_pointing_at_scale() {
        let err = validate_thinking_for_runner("pi", Some("8000")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("numeric budget"), "{msg}");
        assert!(msg.contains("none, minimal, low, medium, high, xhigh"), "{msg}");
    }

    #[test]
    fn cli_and_fake_ignore_any_level() {
        for runner in ["cli", "fake"] {
            assert!(validate_thinking_for_runner(runner, Some("xhigh")).is_ok());
            assert!(validate_thinking_for_runner(runner, Some("anything")).is_ok());
        }
    }

    #[test]
    fn pi_maps_none_to_off() {
        assert_eq!(pi_thinking_value("none"), "off");
        assert_eq!(pi_thinking_value("high"), "high");
        assert_eq!(pi_thinking_value("minimal"), "minimal");
    }
}
