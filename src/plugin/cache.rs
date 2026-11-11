//! The on-disk metadata cache for a plugin slot (`plugins/<name>/plugin.cache.json`).
//! # Why this file exists.
//! A plugin's tool list only exists inside the plugin process: the host cannot know it without starting...

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::plugin::{Contributes, ToolDef};
use crate::workplace::Access;

/// The cache file's name inside a plugin's directory.
pub const CACHE_FILE: &str = "plugin.cache.json";

/// The layout version of the file.
const CACHE_FORMAT: u32 = 1;

/// Which files the cached answer was derived from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    /// `plugin.toml` (name / language / entry / `[contributes]`)
    pub manifest: u64,
    /// The entry file — the script, or the compiled artifact for a rust plugin.
    pub entry: u64,
}

/// FNV-1a, 64-bit. A hash here only has to notice "this file changed"; it is not a security
/// boundary, so a three-line function beats adding a dependency to the tree.
fn hash64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Hash a file; a missing file hashes to 0 rather than erroring.
fn hash_file(p: &Path) -> u64 {
    std::fs::read(p).map(|b| hash64(&b)).unwrap_or(0)
}

/// Compute the fingerprint of a plugin directory.
pub fn fingerprint(dir: &Path, entry: &str) -> Fingerprint {
    Fingerprint { manifest: hash_file(&dir.join("plugin.toml")), entry: hash_file(&dir.join(entry)) }
}

/// One cached tool declaration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedTool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub parameters: Value,
    /// `"read"` or `"write"`. Anything else reads back as `write` (the conservative tier) — the same
    /// rule the live path uses, see `plugin::parse_access`.
    #[serde(default)]
    pub access: String,
}

impl CachedTool {
    fn from_def(d: &ToolDef) -> Self {
        Self {
            name: d.name.clone(),
            description: d.description.clone(),
            parameters: d.parameters.clone(),
            access: match d.access {
                Access::Read => "read".to_string(),
                Access::Write => "write".to_string(),
            },
        }
    }

    fn to_def(&self) -> ToolDef {
        ToolDef {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
            access: if self.access.eq_ignore_ascii_case("read") { Access::Read } else { Access::Write },
        }
    }
}

/// The whole cache record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cached {
    pub cache_format: u32,
    pub fingerprint: Fingerprint,
    #[serde(default)]
    pub tools: Vec<CachedTool>,
    #[serde(default)]
    pub contributes: Contributes,
    /// `BTreeMap` so the file is written in a stable order: a rewrite that changed nothing but the key order would...
    #[serde(default)]
    pub prompt_segments: BTreeMap<String, String>,
    /// The `theme/patch` result, kept as raw JSON (validated into `ThemeConfig` when it is read
    /// back, exactly as the live path validates it).
    #[serde(default)]
    pub theme_patch: Option<Value>,
}

impl Cached {
    /// Snapshot a live `initialize` answer (plus what was fetched with it) into a cache record.
    pub fn capture(
        dir: &Path,
        entry: &str,
        tools: &[ToolDef],
        contributes: Contributes,
        prompt_segments: BTreeMap<String, String>,
        theme_patch: Option<Value>,
    ) -> Self {
        Self {
            cache_format: CACHE_FORMAT,
            fingerprint: fingerprint(dir, entry),
            tools: tools.iter().map(CachedTool::from_def).collect(),
            contributes,
            prompt_segments,
            theme_patch,
        }
    }

    /// The cached tool table, as live [`ToolDef`]s.
    pub fn tool_defs(&self) -> Vec<ToolDef> {
        self.tools.iter().map(CachedTool::to_def).collect()
    }
}

/// Read the cache for a plugin directory.
pub fn load(dir: &Path, entry: &str) -> Option<Cached> {
    let text = std::fs::read_to_string(dir.join(CACHE_FILE)).ok()?;
    let c: Cached = serde_json::from_str(&text).ok()?;
    if c.cache_format != CACHE_FORMAT {
        return None;
    }
    (c.fingerprint == fingerprint(dir, entry)).then_some(c)
}

/// Write the cache for a plugin directory.
pub fn save(dir: &Path, c: &Cached) -> Result<()> {
    let text = serde_json::to_string_pretty(c)
        .context(crate::i18n::t("failed to serialize the plugin metadata cache"))?;
    std::fs::write(dir.join(CACHE_FILE), text)
        .with_context(|| crate::tf!("cannot write {}", dir.join(CACHE_FILE).display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A scratch plugin directory, unique per test (tests in one binary share a temp dir).
    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("aox-plugin-cache-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.toml"), "name = \"p\"\nlanguage = \"python\"\ndescription = \"d\"\nentry = \"main.py\"\n").unwrap();
        std::fs::write(dir.join("main.py"), "print('hi')\n").unwrap();
        dir
    }

    fn def(name: &str, access: Access) -> ToolDef {
        ToolDef { name: name.into(), description: format!("{name} does things"), parameters: json!({"type": "object"}), access }
    }

    /// The cache round-trips, including the `access` bit — losing that bit would silently downgrade every read-only...
    #[test]
    fn cache_round_trips_tools_and_contributes() {
        let dir = scratch("roundtrip");
        let mut segs = BTreeMap::new();
        segs.insert("note".to_string(), "body".to_string());
        let contributes = Contributes { hooks: vec!["before_tool".into()], caps: vec!["fs_read".into()], ..Default::default() };

        let c = Cached::capture(
            &dir,
            "main.py",
            &[def("a", Access::Read), def("b", Access::Write)],
            contributes.clone(),
            segs.clone(),
            Some(json!({"accent": "#fff"})),
        );
        save(&dir, &c).unwrap();

        let back = load(&dir, "main.py").expect("the cache just written must load back");
        let defs = back.tool_defs();
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].access, Access::Read, "the read intent must survive the file");
        assert_eq!(defs[1].access, Access::Write);
        assert_eq!(back.contributes.hooks, vec!["before_tool"]);
        assert_eq!(back.prompt_segments.get("note").map(String::as_str), Some("body"));
        assert_eq!(back.theme_patch, Some(json!({"accent": "#fff"})));
        assert!(
            crate::plugin::same_tools(&back.tool_defs(), &defs),
            "a cache must match the very tools it was built from"
        );
    }

    /// Editing a plugin invalidates **its own** cache and nobody else's — this is the whole reason
    /// the fingerprint exists.
    #[test]
    fn editing_the_source_invalidates_the_cache() {
        let dir = scratch("invalidate");
        let c = Cached::capture(&dir, "main.py", &[def("a", Access::Write)], Contributes::default(), BTreeMap::new(), None);
        save(&dir, &c).unwrap();
        assert!(load(&dir, "main.py").is_some());

        // Touch the entry only: the manifest hash is unchanged, the entry hash is not.
        std::fs::write(dir.join("main.py"), "print('hi')\nprint('again')\n").unwrap();
        assert!(load(&dir, "main.py").is_none(), "a changed entry file must invalidate the cache");

        // And the manifest side too.
        let c = Cached::capture(&dir, "main.py", &[def("a", Access::Write)], Contributes::default(), BTreeMap::new(), None);
        save(&dir, &c).unwrap();
        std::fs::write(dir.join("plugin.toml"), "name = \"p\"\nlanguage = \"python\"\ndescription = \"d2\"\nentry = \"main.py\"\n").unwrap();
        assert!(load(&dir, "main.py").is_none(), "a changed manifest must invalidate the cache");
    }

    /// A file from a future/different layout is ignored rather than misread.
    #[test]
    fn a_different_cache_format_is_ignored() {
        let dir = scratch("format");
        let c = Cached::capture(&dir, "main.py", &[def("a", Access::Write)], Contributes::default(), BTreeMap::new(), None);
        let mut v = serde_json::to_value(&c).unwrap();
        v["cache_format"] = json!(CACHE_FORMAT + 1);
        std::fs::write(dir.join(CACHE_FILE), serde_json::to_string_pretty(&v).unwrap()).unwrap();
        assert!(load(&dir, "main.py").is_none());
    }

    /// Garbage in the file is a miss, never a panic — the host may be looking at a half-written
    /// file, or one a user edited by hand.
    #[test]
    fn a_corrupt_cache_is_a_miss_not_a_crash() {
        let dir = scratch("corrupt");
        std::fs::write(dir.join(CACHE_FILE), "{ not json at all").unwrap();
        assert!(load(&dir, "main.py").is_none());
        std::fs::write(dir.join(CACHE_FILE), "").unwrap();
        assert!(load(&dir, "main.py").is_none());
    }

    /// The stale-detection is about the tool's shape, not just its name: a tool that kept its name but changed...
    #[test]
    fn a_redefined_tool_counts_as_stale() {
        let dir = scratch("stale");
        let before = def("a", Access::Write);
        let c = Cached::capture(&dir, "main.py", &[before.clone()], Contributes::default(), BTreeMap::new(), None);
        let cached = c.tool_defs();
        assert!(crate::plugin::same_tools(&cached, &[before.clone()]));
        assert!(!crate::plugin::same_tools(&cached, &[]), "an empty live table only matches an empty cache");

        let renamed = def("b", Access::Write);
        assert!(!crate::plugin::same_tools(&cached, &[renamed]), "a renamed tool must be noticed");

        let reshaped = ToolDef { parameters: json!({"type": "object", "properties": {"x": {"type": "string"}}}), ..before.clone() };
        assert!(!crate::plugin::same_tools(&cached, &[reshaped]), "a changed schema must be noticed");

        let downgraded = def("a", Access::Read);
        assert!(!crate::plugin::same_tools(&cached, &[downgraded]), "a changed access intent must be noticed");

        // Order is not a difference.
        let two = vec![def("a", Access::Write), def("b", Access::Write)];
        let c2 = Cached::capture(&dir, "main.py", &two, Contributes::default(), BTreeMap::new(), None);
        let cached2 = c2.tool_defs();
        assert!(
            crate::plugin::same_tools(&cached2, &[two[1].clone(), two[0].clone()]),
            "listing the same tools in another order is not staleness"
        );
    }

    /// A declared read survives the file; anything else falls to write.
    #[test]
    fn access_text_is_read_only_when_it_says_so() {
        let make = |s: &str| CachedTool { name: "t".into(), description: String::new(), parameters: json!({}), access: s.into() };
        assert_eq!(make("read").to_def().access, Access::Read);
        assert_eq!(make("READ").to_def().access, Access::Read);
        assert_eq!(make("write").to_def().access, Access::Write);
        assert_eq!(make("").to_def().access, Access::Write);
        assert_eq!(make("readonly").to_def().access, Access::Write);
    }

    /// A missing entry file hashes to something stable and different from a present one — the ghost
    /// plugin directory case, which must read as "broken plugin", not as a crash.
    #[test]
    fn a_missing_entry_is_a_normal_fingerprint_not_a_panic() {
        let dir = scratch("missing-entry");
        let present = fingerprint(&dir, "main.py");
        let absent = fingerprint(&dir, "nope.py");
        assert_ne!(present, absent);
        assert_eq!(absent, fingerprint(&dir, "nope.py"), "a missing file must hash deterministically");
    }
}
