//! Internationalisation (i18n) — the language is fixed at compile time: one distribution speaks exactly...
//! This module serves both front ends (`src/main.rs`'s CLI and `gui/`), so it lives in the lib.
//! # Three design decisions.

use std::collections::HashMap;
use std::fmt;
use std::sync::OnceLock;

/// The registry `build.rs` generates: which language this binary speaks, and which catalogue it
/// embedded.
mod langs {
    include!(concat!(env!("OUT_DIR"), "/langs.rs"));
}

// Language

/// The interface language.
/// Adding a language: drop a catalogue in `locales/`; add a variant here and one line each in Lang::from_code...
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

    /// The language's self-designation, written in itself and never translated, so diagnostics stay recognizable.
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
fn is_source() -> bool {
    cfg!(test) || lang() == SOURCE
}

// Lookup

static MAP: OnceLock<HashMap<&'static str, String>> = OnceLock::new();

/// The keys are `Box::leak`ed into `'static`: this table is built exactly once per process and lives forever...
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
pub fn t(src: &'static str) -> &'static str {
    if is_source() {
        return src;
    }
    map().get(src).map(String::as_str).unwrap_or(src)
}

/// Positional fill: replace the `{}` in `tpl` with `args` in order.
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

/// T + fill combined: `tf!("Collapsed {} tool calls", n)`.
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
        // No placeholders: the whole string is returned
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

    /// "An embedded catalogue" and "is this the source language" must agree — this single test pins both `build.rs`...
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

    /// Match source placeholder counts; placeholders-only keys hijack bare tf!("{}") lookups, so a test pins both.
    #[test]
    fn every_catalogue_agrees_on_placeholders() {
        for (code, text) in [("zh", include_str!("../locales/zh.toml"))] {
            let table: std::collections::BTreeMap<String, String> =
                toml::from_str(text).unwrap_or_else(|e| panic!("locales/{code}.toml does not parse: {e}"));
            assert!(!table.is_empty(), "locales/{code}.toml is empty");
            for (key, value) in &table {
                // Something has to be left once the placeholders are taken out. `"{}"` (or `" {} "`) is not a sentence...
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
