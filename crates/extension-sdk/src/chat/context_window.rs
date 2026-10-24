//! Best-effort context-window sizes for chat models.

/// Best-effort context-window size for a pi model id/pattern. Matches on
/// substrings of the model name so `provider/model:thinking` forms still
/// resolve. Unknown models return `None` — accuracy over presence, the status
/// line then shows the raw token count with no percentage.
pub fn context_window_for_model(model: &str) -> Option<u64> {
    let m = model.to_lowercase();
    // Order matters: check the most specific tokens first.
    if m.contains("claude") {
        // Sonnet/Opus/Haiku families are 200k (1M tiers are opt-in betas).
        return Some(200_000);
    }
    if m.contains("gpt-4.1") || m.contains("gpt-5") {
        return Some(1_000_000);
    }
    if m.contains("o1") || m.contains("o3") || m.contains("o4") {
        return Some(200_000);
    }
    if m.contains("gpt-4o") || m.contains("gpt-4-turbo") {
        return Some(128_000);
    }
    if m.contains("gemini-2") || m.contains("gemini-1.5") {
        return Some(1_000_000);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_window_lookup_matches_families_and_unknowns_are_none() {
        assert_eq!(context_window_for_model("claude-opus-4-8"), Some(200_000));
        assert_eq!(
            context_window_for_model("anthropic/claude-sonnet-4:high"),
            Some(200_000)
        );
        assert_eq!(context_window_for_model("gpt-5.5"), Some(1_000_000));
        assert_eq!(context_window_for_model("gpt-4o-mini"), Some(128_000));
        assert_eq!(
            context_window_for_model("gemini-2.0-flash"),
            Some(1_000_000)
        );
        assert_eq!(context_window_for_model("some-future-model"), None);
    }
}
