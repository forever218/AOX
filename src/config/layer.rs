//! The layered composition manifest (phase 5's **composition layer**).
//!
//! Reading config goes from "open a file" to "synthesise a layer stack":
//!
//! ```text
//! profile → bundle (aggregatable, overridable by a file) → patch (listed in the manifest) → config.toml (always the last layer)
//! ```
//!
//! **Only the source changes, not the model**: what comes out is still an ordinary TOML tree,
//! handed to `toml::from_str::<Config>` for parsing — not one field of `Config`'s shape changed.
//! This is how design principle P1 "a single normalised config tree" is delivered, and why this
//! route can be incremental without rework.
//!
//! Precedence: **later layers win**, and the merge rule is "if both sides are tables, merge deep;
//! otherwise replace wholesale".
//!
//! Two semantics to hold on to:
//!
//! - `config.toml` **is always the last layer**. So not one key landed in phases 1–4 stops
//!   working, and existing configs cost nothing to migrate (decision A: long-term compatibility).
//! - Without `<app root>/aox.toml`, the layer stack degenerates to a single `[config.toml]` layer —
//!   the composition result is **field-for-field identical** to before phase 5
//!   (`no_manifest_is_single_layer` pins this).
//!
//! Every failure path is **an error at startup, never a silent skip**: an unrecognised bundle, a
//! missing patch, a bundle cycle, a profile absent from the manifest — all fail outright. The
//! consequence of skipping silently is "I clearly wrote this layer, why didn't it take effect",
//! and that class of problem is the hardest to track down.

use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::Config;

// ─────────────────────────────── Layers ───────────────────────────────

/// Where a layer comes from. Decides how the source is labelled in dump / `/config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerKind {
    /// A built-in bundle (`include_str!`ed into the binary)
    Bundle,
    /// `<app root>/bundles/<name>.toml` — use it if present, **replacing the whole layer** of the
    /// same-named built-in
    BundleFile,
    /// A patch file listed in the manifest (relative to the app root)
    Patch,
    /// The one given by `--config`. **Always the last layer.**
    Config,
}

/// One layer: a name + a source category + the config tree it contributes (a TOML table).
#[derive(Debug, Clone)]
pub struct Layer {
    /// Display name: bundle name / how the patch is written in the manifest / config file name
    pub name: String,
    pub kind: LayerKind,
    /// A supplementary note (currently only "file overrides built-in" and "custom"), appended
    /// after the label
    pub note: Option<String>,
    /// The values this layer contributes — composition directives (`bundles`) were stripped before
    /// this step
    pub doc: toml::Value,
}

impl Layer {
    /// The source label shown in dump and `/config`.
    pub fn label(&self) -> String {
        let head = match self.kind {
            LayerKind::Bundle | LayerKind::BundleFile => format!("bundle:{}", self.name),
            LayerKind::Patch => format!("patch:{}", self.name),
            LayerKind::Config => self.name.clone(),
        };
        match &self.note {
            Some(n) => crate::tf!("{} ({})", head, n),
            None => head,
        }
    }

    /// How many values this layer writes itself (leaves, not intermediate tables). An empty
    /// aggregating bundle is 0.
    pub fn value_count(&self) -> usize {
        count_leaves(&self.doc)
    }
}

// ──────────────────────────── The provenance table ────────────────────────────

/// Leaf path → the label of the last layer to write it.
///
/// Paths are dot-separated (`provider.base_url` / `auth.overrides.fs_*`). **Only leaves are
/// recorded** — the "source" of an intermediate table is meaningless: one table is often written
/// key by key by several layers, e.g. `[agent]` sets the tool timeout in tools.toml and the rules
/// file in prompt-core.toml.
#[derive(Debug, Clone, Default)]
pub struct Provenance {
    map: BTreeMap<String, String>,
}

impl Provenance {
    /// Which layer last wrote this leaf; None if none did (= the code's built-in default).
    pub fn get(&self, path: &str) -> Option<&str> {
        self.map.get(path).map(String::as_str)
    }

    /// How many values the composed tree has in total.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    #[allow(dead_code)] // Paired with `len`; unused in the bin target
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    fn set(&mut self, path: &str, label: &str) {
        // An empty path = the root of the whole tree, meaningless, don't record
        if !path.is_empty() {
            self.map.insert(path.to_string(), label.to_string());
        }
    }
}

// ──────────────────────────── Composition ────────────────────────────

/// The composed result: layer stack + composed tree + provenance table.
///
/// Deliberately **decoupled from `Config`**: `compose` does not parse `Config`, so even if the
/// config itself is faulty (a required item missing, a type written wrong), `--dump-config` can
/// still print the composed tree — a diagnostic tool that only works when the config is fine is
/// half a useless tool.
#[derive(Debug)]
pub struct Composed {
    pub layers: Vec<Layer>,
    pub merged: toml::Value,
    pub provenance: Provenance,
}

impl Composed {
    /// Parse the composed tree into `Config`. The shape is exactly as before phase 5.
    pub fn to_config(&self) -> Result<Config> {
        let text = toml::to_string(&self.merged).context(crate::i18n::t("the composed config tree cannot be serialized"))?;
        let last = self.layers.last().map(Layer::label).unwrap_or_else(|| crate::i18n::t("(no layers)").into());
        toml::from_str::<Config>(&text).with_context(|| {
            crate::tf!(
                "failed to parse the composed config (the last layer is {}) — the most likely cause is a missing required field (e.g. [provider]'s base_url / model) or a wrong type; the line numbers in the error refer to the composed result, not to a line in one of your files — run `aox --dump-config` to see the composed tree and match the key",
                last
            )
        })
    }

    /// Just the layers' source labels (used by the GUI "About" window's one line).
    #[allow(dead_code)] // Only the GUI calls it; the bin target compiles this file too
    pub fn layer_labels(&self) -> Vec<String> {
        self.layers.iter().map(Layer::label).collect()
    }

    /// The short `/config` layout: which layers there are and how many values each contributes.
    pub fn layer_summary(&self) -> String {
        let mut s = String::from(crate::i18n::t("Config layers (low → high, later ones override earlier ones):\n"));
        for (i, l) in self.layers.iter().enumerate() {
            s.push_str(&crate::tf!("  {}. {} ({} values)\n", i + 1, l.label(), l.value_count()));
        }
        s.push_str(&crate::tf!(
            "{} layers in total, {} values after composition.",
            self.layers.len(),
            self.provenance.len()
        ));
        if self.layers.len() == 1 {
            s.push_str(crate::i18n::t("\n(no aox.toml under the app root — only the single config.toml is used, equivalent to before phase 5.)"));
        }
        s
    }

    /// The full `--dump-config` layout: layer list + composed tree, each value tagged with its
    /// source.
    pub fn dump_text(&self) -> String {
        let mut s = String::new();

        s.push_str(crate::i18n::t("Config layers (low → high, later ones override earlier ones; config.toml is always the last layer):\n"));
        let n = self.layers.len();
        for (i, l) in self.layers.iter().enumerate() {
            let tail = if i + 1 == n { crate::i18n::t("   ← last layer") } else { "" };
            s.push_str(&format!("  {:>2}  {}{}\n", i + 1, l.label(), tail));
        }

        // (top-level key, the "path = value" rendering, source label)
        let mut rows: Vec<(String, String, String)> = Vec::new();
        collect_rows(&self.merged, "", &mut |path, v| {
            let group = path.split('.').next().unwrap_or_default().to_string();
            let src = self.provenance.get(path).unwrap_or(crate::i18n::t("default (code)")).to_string();
            rows.push((group, format!("{} = {}", path, inline(v)), src));
        });

        s.push_str(&crate::tf!(
            "\nComposed config ({} values; the layer that last wrote each value follows the `#`):\n",
            rows.len()
        ));
        if rows.is_empty() {
            s.push_str(crate::i18n::t("\n  (empty — every key is left to the code default.)\n"));
        }
        let width = rows.iter().map(|r| r.1.chars().count()).max().unwrap_or(0).min(72);
        let mut last_group = String::new();
        for (group, display, src) in &rows {
            if *group != last_group {
                s.push_str(&format!("\n  ── {} ──\n", group));
                last_group = group.clone();
            }
            let pad = width.saturating_sub(display.chars().count()).max(1);
            s.push_str(&format!("  {}{}# {}\n", display, " ".repeat(pad), src));
        }

        s.push_str(
            crate::i18n::t("\nKeys **absent** from the composed tree = not part of composition: either they have no factory value (required fields you supply in config.toml, such as [provider]'s base_url / model), or the code falls back for them at read time (such as [theme]'s various scale defaults).\n"),
        );
        s
    }
}

/// Compose + parse into `Config` — the one path both the CLI and the GUI take.
pub struct Resolved {
    pub config: Config,
    pub composed: Composed,
}

/// Compose the layer stack and parse it into `Config`. The only config entry point for both
/// frontends (CLI / GUI).
///
/// `manifest`: an explicit override from `--manifest <path>`; `None` means look at `aox.toml` under
/// the app root.
/// `profile`: a temporary override from `--profile <name>`; `None` means use the manifest's
/// `profile = "…"`, and failing that `"default"`.
pub fn resolve(
    app_root: &Path,
    config_path: &Path,
    manifest: Option<&Path>,
    profile: Option<&str>,
) -> Result<Resolved> {
    let composed = compose(app_root, config_path, manifest, profile)?;
    let config = composed.to_config()?;
    Ok(Resolved { config, composed })
}

/// Compose only, without parsing `Config` (used by the `--dump-config` diagnostic entry point).
pub fn compose(
    app_root: &Path,
    config_path: &Path,
    manifest: Option<&Path>,
    profile: Option<&str>,
) -> Result<Composed> {
    let mut layers: Vec<Layer> = Vec::new();

    // ── Manifest: if `--manifest` was given explicitly it must exist; otherwise look at aox.toml
    //    under the app root ──
    let explicit = manifest.is_some();
    let manifest_path = match manifest {
        Some(p) => PathBuf::from(p),
        None => app_root.join("aox.toml"),
    };
    if explicit && !manifest_path.is_file() {
        bail!(crate::tf!("manifest file not found: {}", manifest_path.display()));
    }

    if manifest_path.is_file() {
        let (default_profile, profiles) = load_manifest(&manifest_path)?;
        let name = profile
            .map(str::to_string)
            .or(default_profile)
            .unwrap_or_else(|| "default".to_string());
        let decl = profiles.get(&name).ok_or_else(|| {
            let mut known: Vec<&str> = profiles.keys().map(String::as_str).collect();
            known.sort_unstable();
            if known.is_empty() {
                anyhow!(crate::tf!(
                    "manifest file {} has no [profiles.{}]. Add that section, or delete this file (without it, only the single config.toml is used, and behavior matches before).",
                    manifest_path.display(),
                    name
                ))
            } else {
                anyhow!(crate::tf!(
                    "manifest file {} has no profile \"{}\"; available: {}",
                    manifest_path.display(),
                    name,
                    known.join(", ")
                ))
            }
        })?;

        layers.append(&mut expand_bundles(app_root, &decl.bundles)?);

        for p in &decl.patches {
            let path = relative_to(app_root, p);
            if !path.is_file() {
                bail!(crate::tf!(
                    "patch file does not exist: {} (listed in profile \"{}\" of {})",
                    path.display(),
                    name,
                    manifest_path.display()
                ));
            }
            let doc = read_toml(&path, crate::i18n::t("patch file"))?;
            layers.push(Layer {
                name: p.clone(),
                kind: LayerKind::Patch,
                note: None,
                doc,
            });
        }
    }

    // ── config.toml: always the last layer ──
    let text = std::fs::read_to_string(config_path).with_context(|| {
        crate::tf!(
            "config file not found: {}. Copy config.example.toml to config.toml and fill in the API settings.",
            config_path.display()
        )
    })?;
    let doc = toml::from_str::<toml::Value>(&text)
        .with_context(|| crate::tf!("failed to parse config file {}", config_path.display()))?;
    let name = config_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| config_path.display().to_string());
    layers.push(Layer { name, kind: LayerKind::Config, note: None, doc });

    // ── Compose: later layers override earlier ones ──
    let mut merged = toml::Value::Table(toml::Table::new());
    let mut provenance = Provenance::default();
    for l in &layers {
        let label = l.label();
        merge_at(&mut merged, &l.doc, &label, "", &mut provenance);
    }

    Ok(Composed { layers, merged, provenance })
}

/// Expand bundles by name only (without reading the manifest). Both `compose` and the unit tests
/// take this path.
///
/// The expansion order is **post-order**: all child bundles first, oneself last. So an aggregator
/// (`core`), if it writes values, can override its child bundles — which also makes "a bundle
/// listed later in the profile" reliably beat one listed earlier (that is how `strict` overrides
/// the factory choice).
pub fn expand_bundles(app_root: &Path, names: &[String]) -> Result<Vec<Layer>> {
    let mut ex = Expander { app_root, out: Vec::new() };
    let mut stack: Vec<String> = Vec::new();
    for n in names {
        ex.expand(n, &mut stack)?;
    }
    Ok(ex.out)
}

// ──────────────────────────── Expansion ────────────────────────────

struct Expander<'a> {
    app_root: &'a Path,
    out: Vec<Layer>,
}

impl Expander<'_> {
    fn expand(&mut self, name: &str, stack: &mut Vec<String>) -> Result<()> {
        if stack.iter().any(|n| n == name) {
            bail!(crate::tf!("bundle cycle: {} → {}", stack.join(" → "), name));
        }
        let (text, kind, note, origin) = self.load(name)?;
        let doc = toml::from_str::<toml::Value>(&text)
            .with_context(|| crate::tf!("failed to parse bundle {} ({})", name, origin))?;
        let toml::Value::Table(mut table) = doc else {
            bail!(crate::tf!("the root of bundle {} ({}) must be a table", name, origin));
        };

        // `bundles = [...]` is a **composition directive**, not a config value — expand first,
        // then strip it from the values.
        let children = match table.get("bundles") {
            Some(toml::Value::Array(a)) => a.clone(),
            Some(_) => bail!(crate::tf!("bundle {} ({})'s `bundles` must be an array of strings", name, origin)),
            None => Vec::new(),
        };
        let child_names: Vec<String> = children
            .iter()
            .map(|c| {
                c.as_str().map(str::to_string).ok_or_else(|| {
                    anyhow!(crate::tf!("an entry of bundle {} ({})'s `bundles` is not a string", name, origin))
                })
            })
            .collect::<Result<_>>()?;

        stack.push(name.to_string());
        for c in &child_names {
            self.expand(c, stack)?;
        }
        stack.pop();

        table.remove("bundles");

        self.out.push(Layer { name: name.to_string(), kind, note, doc: toml::Value::Table(table) });
        Ok(())
    }

    /// Find one bundle layer's content: **a file under the app root wins**, then the built-in.
    ///
    /// If the file exists it **replaces the built-in layer wholesale** (not a key-by-key deep
    /// merge) — "swap out the whole set of factory choices" is the useful semantic; if you only
    /// want to change a few keys, that's what a patch is for.
    fn load(&self, name: &str) -> Result<(String, LayerKind, Option<String>, String)> {
        let file = self.app_root.join("bundles").join(format!("{name}.toml"));
        if file.is_file() {
            let text = std::fs::read_to_string(&file)
                .with_context(|| crate::tf!("cannot read bundle file {}", file.display()))?;
            let note = if builtin(name).is_some() {
                Some(crate::i18n::t("file overrides built-in").to_string())
            } else {
                Some(crate::i18n::t("custom").to_string())
            };
            let origin = file.display().to_string();
            return Ok((text, LayerKind::BundleFile, note, origin));
        }
        match builtin(name) {
            Some(text) => Ok((text.to_string(), LayerKind::Bundle, None, crate::i18n::t("built-in").to_string())),
            None => bail!(crate::tf!("unrecognized bundle \"{}\": neither built-in nor {}", name, file.display())),
        }
    }
}

/// The built-in bundle table. `include_str!` needs a **literal** path, so they can only be listed
/// one by one; adding a bundle means adding a line here (miss it and startup reports an
/// "unrecognised bundle", never silently).
fn builtin(name: &str) -> Option<&'static str> {
    Some(match name {
        "core" => include_str!("../../bundles/core.toml"),
        "llm-openai" => include_str!("../../bundles/llm-openai.toml"),
        "tools" => include_str!("../../bundles/tools.toml"),
        "store-jsonl" => include_str!("../../bundles/store-jsonl.toml"),
        "recovery" => include_str!("../../bundles/recovery.toml"),
        "loop-standard" => include_str!("../../bundles/loop-standard.toml"),
        "prompt-core" => include_str!("../../bundles/prompt-core.toml"),
        "strict" => include_str!("../../bundles/strict.toml"),
        _ => return None,
    })
}

// ──────────────────────────── Merging ────────────────────────────

/// Merge `src` into `dst`: **if both sides are tables, merge deep; otherwise replace wholesale**
/// (later layers override earlier ones).
///
/// `path` is the current node's dot-separated path in the config tree — conveniently recording,
/// for each leaf, who last wrote it into `prov`.
fn merge_at(dst: &mut toml::Value, src: &toml::Value, label: &str, path: &str, prov: &mut Provenance) {
    if let (toml::Value::Table(dt), toml::Value::Table(st)) = (&mut *dst, src) {
        for (k, v) in st {
            let child = join_path(path, k);
            match dt.get_mut(k) {
                // An existing key: keep merging downwards (a table merges deep, anything else is
                // swapped wholesale)
                Some(slot) => merge_at(slot, v, label, &child, prov),
                // A new key: move the whole subtree in, and record all its leaves as written by
                // this layer
                None => {
                    dt.insert(k.clone(), v.clone());
                    record(v, label, &child, prov);
                }
            }
        }
    } else {
        *dst = src.clone();
        record(src, label, path, prov);
    }
}

/// Record `v`'s **leaves**. Arrays are "replaced wholesale", so recording stops at the array
/// level.
fn record(v: &toml::Value, label: &str, path: &str, prov: &mut Provenance) {
    match v {
        toml::Value::Table(t) => {
            for (k, c) in t {
                record(c, label, &join_path(path, k), prov);
            }
        }
        _ => prov.set(path, label),
    }
}

fn count_leaves(v: &toml::Value) -> usize {
    match v {
        toml::Value::Table(t) => t.values().map(count_leaves).sum(),
        _ => 1,
    }
}

/// Flatten the composed tree into a stream of "leaf path → value".
fn collect_rows(v: &toml::Value, path: &str, out: &mut dyn FnMut(&str, &toml::Value)) {
    match v {
        // An empty table also counts as a leaf (otherwise an empty `[foo]` would vanish from the
        // dump out of thin air)
        toml::Value::Table(t) if !t.is_empty() => {
            for (k, c) in t {
                collect_rows(c, &join_path(path, k), out);
            }
        }
        _ => out(path, v),
    }
}

fn join_path(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

/// The one-line inline rendering (tables / arrays are compressed onto one line, for easy alignment
/// and grep).
fn inline(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => format!("{s:?}"),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => {
            if f.is_finite() && f.fract() == 0.0 {
                format!("{f:.1}")
            } else {
                f.to_string()
            }
        }
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Datetime(d) => d.to_string(),
        toml::Value::Array(a) => {
            let items: Vec<String> = a.iter().map(inline).collect();
            format!("[{}]", items.join(", "))
        }
        toml::Value::Table(t) => {
            let items: Vec<String> =
                t.iter().map(|(k, val)| format!("{k} = {}", inline(val))).collect();
            format!("{{ {} }}", items.join(", "))
        }
    }
}

// ──────────────────────────── The manifest ────────────────────────────

/// One profile in `aox.toml`.
struct ProfileDecl {
    bundles: Vec<String>,
    patches: Vec<String>,
}

/// Read the manifest: `profile = "…"` (which one to use by default) + each `[profiles.<name>]`.
fn load_manifest(path: &Path) -> Result<(Option<String>, BTreeMap<String, ProfileDecl>)> {
    let doc = read_toml(path, crate::i18n::t("manifest file"))?;
    let root = doc
        .as_table()
        .ok_or_else(|| anyhow!(crate::tf!("the root of manifest file {} must be a table", path.display())))?;

    let default_profile = root.get("profile").and_then(|v| v.as_str()).map(str::to_string);

    let mut profiles = BTreeMap::new();
    if let Some(t) = root.get("profiles").and_then(|v| v.as_table()) {
        for (name, v) in t {
            let pt =
                v.as_table().ok_or_else(|| anyhow!(crate::tf!("[profiles.{}] must be a table", name)))?;
            profiles.insert(
                name.clone(),
                ProfileDecl {
                    bundles: str_list(pt.get("bundles"), &format!("[profiles.{name}].bundles"))?,
                    patches: str_list(pt.get("patches"), &format!("[profiles.{name}].patches"))?,
                },
            );
        }
    }
    Ok((default_profile, profiles))
}

fn str_list(v: Option<&toml::Value>, what: &str) -> Result<Vec<String>> {
    match v {
        None => Ok(Vec::new()),
        Some(toml::Value::Array(a)) => a
            .iter()
            .map(|x| {
                x.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| anyhow!(crate::tf!("an entry of {} is not a string", what)))
            })
            .collect(),
        Some(_) => bail!(crate::tf!("{} must be an array of strings", what)),
    }
}

fn read_toml(path: &Path, what: &str) -> Result<toml::Value> {
    let text = std::fs::read_to_string(path)
        .with_context(|| crate::tf!("cannot read {} {}", what, path.display()))?;
    toml::from_str(&text).with_context(|| crate::tf!("failed to parse {} {}", what, path.display()))
}

/// A patch / bundle path: a relative path is resolved against the app root, an absolute one is
/// used as-is.
fn relative_to(app_root: &Path, p: &str) -> PathBuf {
    let path = PathBuf::from(p);
    if path.is_absolute() {
        path
    } else {
        app_root.join(path)
    }
}

// ──────────────────────────── Unit tests ────────────────────────────
//
// Every one of these tests builds its own "isolated root": a separate directory plus its own
// config.toml / aox.toml / patch / bundles/*.toml. It never touches the repo's real config.toml,
// so it is unaffected by the developer's machine-local config.

#[cfg(test)]
mod tests {
    use super::*;

    /// One separate directory per case, created under the system temp directory.
    ///
    /// The project's only deletion action lives here, and its scope is bounded by three things at
    /// once: it must be a directory **we named ourselves**, it must sit under the system temp
    /// directory, and its name must carry the `aox-layer-test-` prefix.
    /// (The project's "no directory-level deletion" rule guards precisely against runaway
    /// recursive deletes.)
    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aox-layer-test-{tag}"));
        assert!(dir.starts_with(std::env::temp_dir()), "refuses to clean up outside the temp directory");
        assert!(
            dir.file_name().unwrap().to_string_lossy().starts_with("aox-layer-test-"),
            "only cleans up directories it created itself"
        );
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, rel: &str, text: &str) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, text).unwrap();
    }

    /// A config with only the required fields filled in. Comparing via `format!("{:#?}")` is to
    /// compare **field by field** — `PartialEq` isn't implemented on `Config` (the roadmap says
    /// explicitly not to touch `Config` itself), while every field implements `Debug`, and equal
    /// Debug strings ⇔ equal fields.
    const MINIMAL: &str = r#"
[provider]
base_url = "https://example.com/v1"
model = "m"
api_key = "k"
"#;

    fn core_manifest(dir: &Path) {
        write(
            dir,
            "aox.toml",
            "profile = \"default\"\n\n[profiles.default]\nbundles = [\"core\"]\n",
        );
    }

    /// Without `aox.toml`, the layer stack is a single `[config.toml]` layer —
    /// the composition result is field-for-field identical to "parsing config.toml directly"
    /// (phase 5's zero-drift guarantee).
    #[test]
    fn no_manifest_is_single_layer() {
        let dir = tmpdir("single");
        write(&dir, "config.toml", MINIMAL);

        let c = compose(&dir, &dir.join("config.toml"), None, None).unwrap();
        assert_eq!(c.layers.len(), 1, "without aox.toml there should be only the config.toml layer");
        assert_eq!(c.layers[0].kind, LayerKind::Config);
        assert_eq!(c.layers[0].label(), "config.toml");
        assert_eq!(c.provenance.get("provider.base_url"), Some("config.toml"));

        let direct: Config = toml::from_str(MINIMAL).unwrap();
        let composed = c.to_config().unwrap();
        assert_eq!(
            format!("{direct:#?}"),
            format!("{composed:#?}"),
            "the composed result must be field-for-field identical to parsing config.toml directly"
        );
    }

    /// **This test carries the cost of decision B**: the factory values written in the leaf
    /// bundles must be field-for-field identical to the code defaults (the `#[serde(default)]`
    /// side). Change either one without syncing and this goes red.
    #[test]
    fn core_equals_code_defaults() {
        let dir = tmpdir("core-defaults");
        write(&dir, "config.toml", MINIMAL);
        core_manifest(&dir);

        let direct: Config = toml::from_str(MINIMAL).unwrap();
        let composed =
            compose(&dir, &dir.join("config.toml"), None, None).unwrap().to_config().unwrap();

        assert_eq!(
            format!("{direct:#?}"),
            format!("{composed:#?}"),
            "core's factory values must be field-for-field identical to the code defaults (bundles and serde defaults must not drift silently)"
        );
    }

    /// Later layers override earlier ones: patch beats bundle, config.toml beats patch; and it's a
    /// **deep table merge**.
    #[test]
    fn patch_overrides_bundle_and_config_wins_last() {
        let dir = tmpdir("order");
        write(&dir, "config.toml", MINIMAL);
        write(
            &dir,
            "aox.local.toml",
            "[provider]\nstream = false\n\n[agent]\ntool_timeout_secs = 5\n",
        );
        write(
            &dir,
            "aox.toml",
            "profile = \"default\"\n\n[profiles.default]\nbundles = [\"core\"]\npatches = [\"aox.local.toml\"]\n",
        );

        let c = compose(&dir, &dir.join("config.toml"), None, None).unwrap();
        let cfg = c.to_config().unwrap();

        assert!(!cfg.provider.stream, "the patch's stream = false should beat the bundle's true");
        assert_eq!(cfg.agent.tool_timeout_secs, 5, "the patch should beat the bundle's 60");
        assert_eq!(
            cfg.agent.max_result_chars, 20000,
            "the agent table is deep-merged: a patch that only changes the timeout must not wipe max_result_chars as well"
        );
        assert_eq!(cfg.provider.model, "m", "config.toml is still the strongest layer");

        assert_eq!(c.provenance.get("provider.stream"), Some("patch:aox.local.toml"));
        assert_eq!(c.provenance.get("agent.tool_timeout_secs"), Some("patch:aox.local.toml"));
        assert_eq!(c.provenance.get("agent.max_result_chars"), Some("bundle:tools"));
        assert_eq!(c.provenance.get("auth.meta_tools"), Some("bundle:tools"));
    }

    /// `config.toml` is always the last layer: not even a patch can override it.
    #[test]
    fn config_toml_beats_every_layer() {
        let dir = tmpdir("config-last");
        write(&dir, "aox.local.toml", "[store]\nbackend = \"nope\"\n");
        write(
            &dir,
            "config.toml",
            "[provider]\nbase_url = \"https://example.com/v1\"\nmodel = \"m\"\napi_key = \"k\"\n\n[store]\nbackend = \"jsonl\"\n",
        );
        write(
            &dir,
            "aox.toml",
            "profile = \"default\"\n\n[profiles.default]\nbundles = [\"core\"]\npatches = [\"aox.local.toml\"]\n",
        );

        let c = compose(&dir, &dir.join("config.toml"), None, None).unwrap();
        assert_eq!(c.layers.last().unwrap().kind, LayerKind::Config);
        assert_eq!(c.provenance.get("store.backend"), Some("config.toml"));
    }

    /// Both `--profile <name>` and the manifest's `profile = "…"` can select a profile.
    #[test]
    fn profile_selection_and_default() {
        let dir = tmpdir("profiles");
        write(&dir, "config.toml", MINIMAL);
        write(
            &dir,
            "aox.toml",
            "profile = \"strict\"\n\n[profiles.default]\nbundles = [\"core\"]\n\n[profiles.strict]\nbundles = [\"core\", \"strict\"]\n",
        );

        // The manifest says profile = "strict", so with no argument it takes that one
        let s = compose(&dir, &dir.join("config.toml"), None, None).unwrap();
        assert!(s.layer_labels().iter().any(|l| l == "bundle:strict"), "should use strict");
        assert_eq!(
            s.provenance.get("auth.meta_tools"),
            Some("bundle:strict"),
            "strict comes later and covers tools"
        );

        // Explicitly switch back to default
        let d = compose(&dir, &dir.join("config.toml"), None, Some("default")).unwrap();
        assert!(!d.layer_labels().iter().any(|l| l == "bundle:strict"), "default does not contain strict");
        assert_eq!(d.provenance.get("auth.meta_tools"), Some("bundle:tools"));
    }

    /// When `bundles/<name>.toml` exists under the app root it **replaces the built-in layer
    /// wholesale**, and the source says so.
    #[test]
    fn file_bundle_overrides_builtin() {
        let dir = tmpdir("file-override");
        write(&dir, "config.toml", MINIMAL);
        write(&dir, "bundles/tools.toml", "[auth]\nmeta_tools = \"deny\"\n");
        core_manifest(&dir);

        let c = compose(&dir, &dir.join("config.toml"), None, None).unwrap();
        let labels = c.layer_labels();
        assert!(
            labels.iter().any(|l| l == "bundle:tools (file overrides built-in)"),
            "this should be marked as a file override: {labels:?}"
        );

        let cfg = c.to_config().unwrap();
        assert_eq!(cfg.auth.meta_tools, "deny", "the value from the file takes effect");
        assert_eq!(
            cfg.auth.plugin_tools, "auto",
            "the file replaces the built-in layer wholesale: plugin_tools, which it does not write, falls back to the code default (the tools.toml line is no longer in the layer)"
        );
    }

    /// Arrays are **replaced wholesale**, not appended — this is also the pin for "deliberately no
    /// append semantics".
    #[test]
    fn arrays_are_replaced_whole_not_appended() {
        let dir = tmpdir("arrays");
        write(&dir, "config.toml", MINIMAL);
        write(&dir, "p1.toml", "[prompt]\norder = [\"env\"]\n");
        write(&dir, "p2.toml", "[prompt]\norder = [\"rules\"]\n");
        write(
            &dir,
            "aox.toml",
            "profile = \"default\"\n\n[profiles.default]\nbundles = [\"core\"]\npatches = [\"p1.toml\", \"p2.toml\"]\n",
        );

        let c = compose(&dir, &dir.join("config.toml"), None, None).unwrap();
        let cfg = c.to_config().unwrap();
        assert_eq!(cfg.prompt.order, vec!["rules".to_string()], "a later layer replaces the array wholesale rather than appending");
        assert_eq!(c.provenance.get("prompt.order"), Some("patch:p2.toml"));
    }

    /// An override key containing `*` must be quoted to be valid (`*` isn't in TOML's bare-key
    /// character set). This test also verifies that "composition goes through a
    /// serialise → re-parse" won't break such a key.
    #[test]
    fn wildcard_override_key_survives_roundtrip() {
        assert!(
            toml::from_str::<Config>(
                "[provider]\nbase_url = \"u\"\nmodel = \"m\"\n\n[auth.overrides]\nfs_* = \"deny\"\n"
            )
            .is_err(),
            "a bare fs_* is not a valid TOML key — this is exactly why quotes are needed in config.example.toml"
        );

        let dir = tmpdir("wildcard");
        write(
            &dir,
            "config.toml",
            "[provider]\nbase_url = \"https://example.com/v1\"\nmodel = \"m\"\napi_key = \"k\"\n\n[auth.overrides]\n\"fs_*\" = \"deny\"\n",
        );

        let c = compose(&dir, &dir.join("config.toml"), None, None).unwrap();
        let cfg = c.to_config().unwrap();
        assert_eq!(cfg.auth.overrides.get("fs_*").map(String::as_str), Some("deny"));
        assert_eq!(c.provenance.get("auth.overrides.fs_*"), Some("config.toml"));
    }

    /// The `--dump-config` body must **tag every value with its source** (acceptance criterion 1).
    #[test]
    fn dump_marks_each_value_with_its_layer() {
        let dir = tmpdir("dump");
        write(&dir, "config.toml", MINIMAL);
        core_manifest(&dir);

        let d = compose(&dir, &dir.join("config.toml"), None, None).unwrap().dump_text();
        assert!(d.contains("bundle:store-jsonl"), "{d}");
        assert!(d.contains("# bundle:store-jsonl"), "every value should be followed by its source: {d}");
        assert!(d.contains("store.backend = \"jsonl\""), "{d}");
        assert!(d.contains("provider.base_url = \"https://example.com/v1\""), "{d}");
        assert!(d.contains("# config.toml"), "{d}");
        assert!(d.contains("← last layer"), "which layer is last should be marked: {d}");
    }

    /// The short `/config` layout: the number of layers and how many values each contributes.
    #[test]
    fn layer_summary_lists_layers_and_counts() {
        let dir = tmpdir("summary");
        write(&dir, "config.toml", MINIMAL);
        core_manifest(&dir);

        let s = compose(&dir, &dir.join("config.toml"), None, None).unwrap().layer_summary();
        assert!(s.contains("bundle:loop-standard (0 values)"), "an empty bundle should show 0: {s}");
        assert!(s.contains("bundle:core (0 values)"), "an aggregator carries no values itself: {s}");
        assert!(s.contains("8 layers in total"), "six leaves + core + config.toml: {s}");
        assert!(!s.contains("no aox.toml"), "that sentence must not appear when aox.toml exists: {s}");

        let one = compose(&dir, &dir.join("config.toml"), Some(&dir.join("nope.toml")), None);
        assert!(one.is_err(), "an explicitly given non-existent manifest must error");
    }

    /// An unrecognised bundle / a missing patch / a bundle cycle / no such profile:
    /// all error, none are silently skipped.
    #[test]
    fn failure_paths_are_loud() {
        let dir = tmpdir("failures");
        write(&dir, "config.toml", MINIMAL);

        // ① An unrecognised bundle
        write(&dir, "aox.toml", "profile = \"default\"\n\n[profiles.default]\nbundles = [\"nope\"]\n");
        let e = compose(&dir, &dir.join("config.toml"), None, None).unwrap_err();
        assert!(format!("{e:#}").contains("unrecognized bundle"), "{e:#}");

        // ② A missing patch
        write(
            &dir,
            "aox.toml",
            "profile = \"default\"\n\n[profiles.default]\nbundles = [\"core\"]\npatches = [\"nope.toml\"]\n",
        );
        let e = compose(&dir, &dir.join("config.toml"), None, None).unwrap_err();
        assert!(format!("{e:#}").contains("patch file does not exist"), "{e:#}");

        // ③ A bundle cycle
        write(&dir, "bundles/loop-a.toml", "bundles = [\"loop-b\"]\n");
        write(&dir, "bundles/loop-b.toml", "bundles = [\"loop-a\"]\n");
        write(&dir, "aox.toml", "profile = \"default\"\n\n[profiles.default]\nbundles = [\"loop-a\"]\n");
        let e = compose(&dir, &dir.join("config.toml"), None, None).unwrap_err();
        assert!(format!("{e:#}").contains("cycle"), "{e:#}");

        // ④ No such profile in the manifest
        write(&dir, "aox.toml", "profile = \"default\"\n\n[profiles.default]\nbundles = [\"core\"]\n");
        let e = compose(&dir, &dir.join("config.toml"), None, Some("nope")).unwrap_err();
        assert!(format!("{e:#}").contains("has no profile"), "{e:#}");

        // ⑤ An explicitly given manifest that doesn't exist
        let e = compose(&dir, &dir.join("config.toml"), Some(Path::new("nonexistent-manifest.toml")), None)
            .unwrap_err();
        assert!(format!("{e:#}").contains("manifest file not found"), "{e:#}");

        // ⑥ The config file itself is missing
        let e = compose(&dir, &dir.join("nope-config.toml"), None, None).unwrap_err();
        assert!(format!("{e:#}").contains("config file not found"), "{e:#}");
    }

    /// The two examples shipped with the package (`config.example.toml` / `aox.example.toml`) must
    /// really work.
    ///
    /// This test is aimed at "the example looks nice but the user copies it and can't start" —
    /// phase 2's theme example tripped over exactly this once (the example used nested `[theme]`
    /// while the loader at the time only understood bare themes).
    /// The test runs under the repo root, where the **files** `bundles/*.toml` override the
    /// built-in copies, so the source-label assertions match on name only, not on the suffix.
    #[test]
    fn shipped_examples_compose_and_parse() {
        let root = Path::new(".");
        let cfg_path = root.join("config.example.toml");
        let manifest = root.join("aox.example.toml");

        // ① The example config itself must parse
        let base: Config =
            toml::from_str(&std::fs::read_to_string(&cfg_path).expect("the example config must be readable")).unwrap();

        // ② The example manifest must compose with it, and the result must parse back to Config
        let composed = compose(root, &cfg_path, Some(&manifest), None).expect("the example manifest should compose");
        let cfg = composed.to_config().expect("the example composition should parse");
        assert_eq!(cfg.provider.model, base.provider.model);
        assert_eq!(cfg.provider.base_url, base.provider.base_url);
        assert_eq!(cfg.provider.max_tokens, base.provider.max_tokens);
        assert_eq!(composed.layers.last().unwrap().kind, LayerKind::Config);

        // The factory choices really did enter the composed tree, and config.toml is on top.
        // `prompt.order` is chosen because the example config **doesn't write** [prompt] (the whole
        // section is commented out), so it can only come from a bundle — use it to verify "the
        // bundle really took part in composition".
        let prompt_order = composed.provenance.get("prompt.order").unwrap_or("");
        assert!(
            prompt_order.contains("prompt-core"),
            "the example config does not write [prompt]; it should come from a bundle — actually {prompt_order}"
        );
        assert_eq!(
            composed.provenance.get("agent.tool_timeout_secs"),
            Some("config.example.toml"),
            "the 120 seconds written in the example config must beat the factory bundle"
        );

        // ③ The second profile in the example manifest must compose too.
        //    Note we must **not** assert that strict's values win — the example config writes
        //    [auth] itself, and config.toml is always the strongest layer, so those strict keys
        //    are bound to be overridden by it. What can be verified is "the strict layer really
        //    entered the stack".
        let strict = compose(root, &cfg_path, Some(&manifest), Some("strict")).unwrap();
        assert!(
            strict.layer_labels().iter().any(|l| l.contains("strict")),
            "the layer stack should contain strict: {:?}",
            strict.layer_labels()
        );
        assert_eq!(
            strict.layers.len(),
            composed.layers.len() + 1,
            "the strict profile has exactly one more layer than default"
        );
    }
}
