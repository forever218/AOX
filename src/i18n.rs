//! Internationalisation (i18n) — **the language is fixed at compile time**: one distribution
//! speaks exactly one language.
//!
//! This module serves both front ends (`src/main.rs`'s CLI and `gui/`), so it lives in the lib.
//!
//! # Three design decisions
//!
//! 1. **The source language is English.** Every user-facing string in the code **is the English
//!    text**, and it plays two roles at once: the final wording of the `en` build, and the key
//!    used by every other language's catalogue. Adding a language is therefore "add one [`Lang`]
//!    variant + one `locales/<code>.toml`" — **not one literal in the source has to change**.
//!
//! 2. **The English build has zero drift.** Under the source language `t()` returns its argument
//!    unchanged and `tf!` is byte-for-byte equivalent to `format!`, so "build with `AOX_LANG=en`"
//!    produces output identical to "before i18n existed" — not one existing test needs editing
//!    (which is also why the tests are pinned to the source language, see [`t`]).
//!
//! 3. **There is no in-app switch.** The language is decided at build time by `AOX_LANG`
//!    (`build.rs` `include_str!`s exactly that one catalogue; the registry is `build.rs`'s
//!    `emit_lang`). Installing the English build gives you English throughout — UI, error
//!    messages, system prompt and tool descriptions all switch together; installing the Chinese
//!    build gives you Chinese throughout, and the **other** languages' strings never even enter
//!    the binary.
//!
//! # Usage
//!
//! ```ignore
//! use crate::i18n::{t, tf};
//!
//! println!("{}", t("Ready"));                      // plain text
//! println!("{}", tf!("Collapsed {} tool calls", n)); // with arguments (positional `{}` only)
//! ```
//!
//! ⚠️ Placeholders are **positional `{}` only**: forms such as `{:.1}` / `{:>9}` / `{name}` must be
//! formatted to a string by the caller first (the reason is documented on [`fill`]). The catalogue
//! and the source must agree on the *number* of placeholders.

use std::collections::HashMap;
use std::fmt;
use std::sync::OnceLock;

/// The registry `build.rs` generates: which language this binary speaks, and which catalogue it
/// embedded.
mod langs {
    include!(concat!(env!("OUT_DIR"), "/langs.rs"));
}

// ─────────────────────────── Language ───────────────────────────

/// The interface language.
///
/// **Adding a language**: ① drop a catalogue in `locales/`; ② add a variant here and one line each
/// in [`Lang::from_code`] and [`Lang::native_name`]; ③ build with `AOX_LANG=<code>`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Lang {
    /// English — **the source language** (the literals in the code are already this)
    #[default]
    En,
    /// Chinese
    Zh,
}

/// The source language: the English literals in the code *are* this variant's final wording.
pub const SOURCE: Lang = Lang::En;

impl Lang {
    /// The language code used on disk and on the build command line.
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Zh => "zh",
        }
    }

    /// The language's **self-designation** — always written in that language itself, never
    /// translated.
    ///
    /// This matters whenever a diagnostic reports "what language am I running as": someone who
    /// reads only Chinese must still recognise the two characters 中文 inside an English UI.
    pub fn native_name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::Zh => "中文",
        }
    }

    /// Parse a language code: `en` / `zh`, plus a few common aliases (`zh-CN` / `en_US` and the
    /// like are accepted too). Returns `None` when unrecognised.
    pub fn from_code(s: &str) -> Option<Lang> {
        // Take only the primary subtag: both `zh-CN` and `en_US` collapse to `zh` / `en`.
        let s = s.trim().to_ascii_lowercase();
        match s.split(['-', '_']).next().unwrap_or("") {
            "en" => Some(Lang::En),
            "zh" | "cn" => Some(Lang::Zh),
            _ => None,
        }
    }
}

static LANG: OnceLock<Lang> = OnceLock::new();

/// Which language this binary speaks.
///
/// An unrecognised code makes the program **fail loudly right here** rather than quietly falling
/// back to the source language — the build script has already verified that the catalogue exists,
/// so the only thing left that can go wrong is "the code isn't recognised", and that means
/// `build.rs` and [`Lang::from_code`] have drifted apart. It has to be said out loud.
pub fn lang() -> Lang {
    *LANG.get_or_init(|| {
        Lang::from_code(langs::CODE).unwrap_or_else(|| {
            panic!(
                "AOX_LANG=`{}` is not recognised: add this language code to Lang::from_code in \
                 src/i18n.rs.",
                langs::CODE
            )
        })
    })
}

/// Whether this binary is the source-language (English) build.
///
/// ⚠️ **Tests always run under the source language**: the several hundred existing tests assert
/// the source language's output ("the result should contain some particular word"), and i18n is
/// the identity transform for the source language, so not one of them needs editing under
/// `cargo test`. Release builds are unaffected — `cfg!(test)` is only true in a test build.
fn is_source() -> bool {
    cfg!(test) || lang() == SOURCE
}

// ─────────────────────────── Lookup ───────────────────────────

static MAP: OnceLock<HashMap<&'static str, String>> = OnceLock::new();

/// The "English source → translation" table, parsed and cached on first use.
///
/// The keys are `Box::leak`ed into `'static`: this table is built exactly once per process and
/// lives forever, and in exchange [`t`] can return `&'static str` directly (the GUI reads hundreds
/// of these per frame, so this saves an allocation each time).
fn map() -> &'static HashMap<&'static str, String> {
    MAP.get_or_init(|| {
        let parsed: HashMap<String, String> = match toml::from_str(langs::CATALOG) {
            Ok(m) => m,
            Err(e) => {
                // A broken catalogue must not stop the whole program: fall back to the source
                // language, and say why.
                eprintln!("[i18n] failed to parse locales/{}.toml: {e}", langs::CODE);
                return HashMap::new();
            }
        };
        parsed
            .into_iter()
            .map(|(k, v)| (&*Box::leak(k.into_boxed_str()) as &'static str, v))
            .collect()
    })
}

/// Look up the translation for the current language.
///
/// - Source language (English) ⇒ returned unchanged, **zero drift**.
/// - Any other language ⇒ table lookup; **a miss falls back to the English source** (better to
///   reveal one English string than to show a blank or mojibake — and the one that shows through
///   is exactly the clue for "which entry was missed". `tools/check_i18n.py` will root those out).
pub fn t(src: &'static str) -> &'static str {
    if is_source() {
        return src;
    }
    map().get(src).map(String::as_str).unwrap_or(src)
}

/// Positional fill: replace the `{}` in `tpl` with `args` in order.
///
/// Why not just use `format!`: `format!`'s template must be a **compile-time literal**, but this
/// template comes from a runtime lookup (the return value of `t(...)`). So we implement a minimal
/// filler ourselves.
///
/// It recognises bare `{}` only: width-bearing (`{:.1}`) and named (`{name}`) forms have no syntax
/// support here — format that item to a string first if you need it. The cost is slightly more
/// verbose call sites; the benefit is that "the placeholder in the catalogue" and "the placeholder
/// in the source" can only ever take one shape, so it is hard to get wrong.
pub fn fill(tpl: &str, args: &[&dyn fmt::Display]) -> String {
    let mut out = String::with_capacity(tpl.len() + 16);
    let mut it = args.iter();
    let mut rest = tpl;
    while let Some(i) = rest.find("{}") {
        out.push_str(&rest[..i]);
        match it.next() {
            Some(a) => {
                let _ = fmt::Write::write_fmt(&mut out, format_args!("{a}"));
            }
            // Not enough arguments: leave the placeholder visible rather than eating half the
            // sentence (a seeing-it is the clue that "the catalogue is missing an argument").
            None => out.push_str("{}"),
        }
        rest = &rest[i + 2..];
    }
    out.push_str(rest);
    out
}

/// [`t`] + [`fill`] combined: `tf!("Collapsed {} tool calls", n)`.
///
/// The only difference from `format!("Collapsed {} tool calls", n)` is that the template goes
/// through the catalogue first — so migrating from `format!` means changing the macro name and
/// leaving the arguments exactly as they are.
#[macro_export]
macro_rules! tf {
    ($s:literal $(, $arg:expr)* $(,)?) => {
        $crate::i18n::fill(
            $crate::i18n::t($s),
            &[$(&$arg as &dyn ::std::fmt::Display),*],
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The source language is the identity: `t()` must not change a single character. (The tests
    /// are pinned to the source language, see `is_source`.)
    #[test]
    fn source_language_is_identity() {
        assert!(is_source(), "unit tests must run under the source language");
        assert_eq!(t("Ready"), "Ready");
    }

    #[test]
    fn fill_replaces_in_order() {
        assert_eq!(fill("{} and {}", &[&"A", &"B"]), "A and B");
        // More arguments than placeholders: the extras are dropped, no panic.
        assert_eq!(fill("{}", &[&1, &2]), "1");
        // Not enough arguments: the leftover placeholder stays verbatim (better a visible one than
        // half a sentence eaten).
        assert_eq!(fill("{} and {}", &[&"A"]), "A and {}");
        // No placeholders: the whole string is returned.
        assert_eq!(fill("Ready", &[]), "Ready");
    }

    #[test]
    fn tf_macro_matches_format() {
        let n = 3usize;
        assert_eq!(tf!("Collapsed {} tool calls", n), format!("Collapsed {} tool calls", n));
        assert_eq!(tf!("Plain text"), format!("Plain text"));
    }

    #[test]
    fn lang_codes_roundtrip() {
        for l in [Lang::En, Lang::Zh] {
            assert_eq!(Lang::from_code(l.code()), Some(l));
        }
        assert_eq!(Lang::from_code("zh-CN"), Some(Lang::Zh));
        assert_eq!(Lang::from_code("en_US"), Some(Lang::En));
        assert_eq!(Lang::from_code("fr"), None);
    }

    /// Every language must be able to name itself, otherwise diagnostics cannot tell which build
    /// this is.
    #[test]
    fn every_lang_has_native_name() {
        for l in [Lang::En, Lang::Zh] {
            assert!(!l.native_name().is_empty());
        }
        assert_eq!(Lang::Zh.native_name(), "中文");
    }

    /// The compile-time registry and `Lang::from_code` must agree: an unrecognised code makes
    /// `lang()` panic on the spot.
    #[test]
    fn build_code_is_known() {
        assert!(
            Lang::from_code(langs::CODE).is_some(),
            "AOX_LANG=`{}` has no matching arm in Lang::from_code",
            langs::CODE
        );
    }

    /// "An embedded catalogue" and "is this the source language" must agree — this single test
    /// pins both `build.rs` and [`SOURCE`]: the source-language build must not carry a catalogue
    /// (carrying one would mean stuffing another language's strings into the English build), and a
    /// non-source build must carry one, otherwise the whole screen falls back to English.
    #[test]
    fn catalog_presence_matches_source() {
        if lang() == SOURCE {
            assert!(langs::CATALOG.is_empty(), "the source-language build must not embed any catalogue");
            assert!(map().is_empty(), "the source-language build needs no catalogue table");
        } else {
            assert!(!langs::CATALOG.is_empty(), "a non-source build must embed a catalogue");
            assert!(!map().is_empty(), "the catalogue parsed to empty, which is as good as untranslated");
        }
    }

    /// Every catalogue on disk must agree with the source on **how many placeholders there are**,
    /// and no key may be made of placeholders only.
    ///
    /// Both halves are invisible at runtime and both are catastrophic, which is why they are pinned
    /// here rather than left to review:
    ///
    /// * a key/value pair with different counts makes [`fill`] leave a `{}` in the output or drop an
    ///   argument on the floor, and only on the strings a test happens to exercise;
    /// * a key that is *only* placeholders — `"{}" = "…"` — hijacks the lookup for **every**
    ///   `tf!("{}", x)` in the program, so one bad line in the catalogue replaces every bare number
    ///   on screen with one sentence. That is not hypothetical: a truncated key sitting in
    ///   `locales/zh.toml` printed `[selftest] 全部通过 ✔` in place of the workspace count, the
    ///   context ring's numbers and every other `tf!("{}", …)` until 2026-10-28.
    #[test]
    fn every_catalogue_agrees_on_placeholders() {
        for (code, text) in [("zh", include_str!("../locales/zh.toml"))] {
            let table: std::collections::BTreeMap<String, String> =
                toml::from_str(text).unwrap_or_else(|e| panic!("locales/{code}.toml does not parse: {e}"));
            assert!(!table.is_empty(), "locales/{code}.toml is empty");
            for (key, value) in &table {
                // Something has to be left once the placeholders are taken out. `"{}"` (or `" {} "`)
                // is not a sentence to translate: it is the shape of *every* `tf!("{}", x)` in the
                // program, so translating it translates all of them at once. Punctuation is enough
                // to tell a real format string apart — `"\n  {} {}. {}"` is a numbered list line.
                assert!(
                    key.replace("{}", "").chars().any(|c| !c.is_whitespace()),
                    "locales/{code}.toml: the key {key:?} is placeholders only — it hijacks every tf!() with that shape"
                );
                let (k, v) = (placeholders(key), placeholders(value));
                assert_eq!(
                    k, v,
                    "locales/{code}.toml: {key:?} has {k} placeholder(s) but its translation has {v}: {value:?}"
                );
            }
        }
    }

    /// How many positional `{}` a string has (braces escaped as `{{` / `}}` are not placeholders).
    fn placeholders(s: &str) -> usize {
        s.replace("{{", "").replace("}}", "").matches("{}").count()
    }
}
