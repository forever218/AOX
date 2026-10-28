//! A small, dated table of context windows — the **last** resort when selecting a model.
//!
//! Why this exists at all: the context window is not decoration. It is the denominator of the GUI's
//! "how full is the context" ring *and* the input side of `compact::Policy`'s budget
//! (`window × compact_ratio − max_tokens`). Leave it too high and compaction never fires, so a long
//! conversation goes to the endpoint and comes back as a 400; leave it too low and context that would
//! still fit gets folded away for nothing.
//!
//! Order of preference when the Model tab fills that field in:
//!
//! 1. **what the endpoint itself reported** (`context_length` / `max_model_len` / … — OpenRouter and
//!    vLLM return it) — nothing beats the server saying it;
//! 2. **what the user typed for this exact model id last time** (remembered in the address book);
//! 3. **this table** — a hint, dated, and only for models that are stable enough to have a public
//!    number;
//! 4. **nothing** — the field then says "unknown" and offers a ladder of common values. It is
//!    deliberately *not* guessed: a wrong window is the hardest kind of bug to trace back to here.
//!
//! So: an entry here is a convenience, never a source of truth. When a number goes stale, the GUI
//! shows where the value came from ("from the endpoint" / "from the built-in table" / "typed"), and
//! the field is always editable.

/// One guess: `(needle, window)`, matched case-insensitively as a **prefix of the first segment** or
/// as a substring. Order matters — the list is scanned top to bottom.
///
/// Numbers are what the vendors publish; every entry was checked when it was added
/// (2026-10-27). They are rounded to the nearest thousand only when the vendor rounds.
const TABLE: &[(&str, u32)] = &[
    // DeepSeek (V4 family): 1M context, 384K max output.
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

    /// The specific entries have to win over the family fallback: `deepseek-chat` is 128K while
    /// `deepseek-flash` is 1M, and a family-first table would report 1M for both — which reads as
    /// "compaction never fires" on a legacy model.
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
