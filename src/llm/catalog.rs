//! A small, dated table of context windows — the last resort when selecting a model.
//! Why this exists at all: the context window is not decoration.
//! Order of preference when the Model tab fills that field in.

/// One guess: `(needle, window)`, matched case-insensitively as a prefix of the first segment or as a substring.
const TABLE: &[(&str, u32)] = &[
    // DeepSeek (V4 family): 1M context, 384K max output
    ("deepseek-flash", 1_000_000),
    ("deepseek-v4", 1_000_000),
    ("deepseek-v4-pro", 1_000_000),
    // Legacy DeepSeek names, retired in July 2026 but still seen in old configs / gateways.
    ("deepseek-reasoner", 128_000),
    ("deepseek-chat", 128_000),
    ("deepseek", 128_000),
    // Others: kept short on purpose. A long table is a long list of numbers going stale.
    ("kimi-k2", 256_000),
    ("moonshot", 128_000),
    ("qwen3", 131_072),
    ("qwen", 131_072),
    ("glm-4", 128_000),
    ("gpt-4o", 128_000),
    ("claude", 200_000),
    ("llama-3", 128_000),
    ("mistral", 128_000),
];

/// A window hint for `model`, if this table happens to know it.
pub fn window_hint(model: &str) -> Option<u32> {
    let id = model.trim().to_ascii_lowercase();
    if id.is_empty() {
        return None;
    }
    // DeepSeek's latest names must win over the family fallback, so the scan is "first match wins"
    // in table order, and the table lists the specific entries first.
    TABLE
        .iter()
        .find(|(needle, _)| id.contains(needle))
        .map(|(_, w)| *w)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The specific entries have to win over the family fallback: `deepseek-chat` is 128K while `deepseek-flash`...
    #[test]
    fn the_more_specific_name_wins() {
        assert_eq!(window_hint("deepseek-flash"), Some(1_000_000));
        assert_eq!(window_hint("deepseek-chat"), Some(128_000));
        assert_eq!(window_hint("deepseek-reasoner"), Some(128_000));
        assert_eq!(window_hint("some-vendor/deepseek-flash"), Some(1_000_000));
    }

    /// Case and whitespace must not decide the answer: the model id is whatever the endpoint lists.
    #[test]
    fn matching_is_case_insensitive_and_ignores_surrounding_space() {
        assert_eq!(window_hint("  DeepSeek-Flash "), Some(1_000_000));
        assert_eq!(window_hint("KIMI-K2-Instruct"), Some(256_000));
    }

    /// An unknown model has **no** hint. Returning a default here would be the exact silent guess the
    /// module docs warn about.
    #[test]
    fn an_unknown_model_gets_no_guess() {
        assert_eq!(window_hint("totally-local-7b"), None);
        assert_eq!(window_hint(""), None);
    }
}
