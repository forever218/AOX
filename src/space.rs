//! The **space registry**: which workspaces exist, what they are called, and which conversation
//! lives in which one.
//!
//! Why a registry rather than "group by the conversation's own workspace record": the two answer
//! different questions. `kind:"workplace"` in a `.jsonl` records **where writes are allowed** — a
//! security boundary, decided by the tier. A space is a **place in the UI**, and the user may want a
//! conversation to sit somewhere other than where its boundary points. Keeping them apart means
//! re-organising the list never touches a permission boundary, and it means **the `.jsonl` format
//! stays untouched** (design decision of 2026-10-14).
//!
//! The file lives at `<app root>/.aox/spaces.json` — deliberately in the app root and not per
//! workspace: "which workspaces exist" cannot itself be scattered across those workspaces, or
//! nothing could ever enumerate them.
//!
//! Three properties this module holds to:
//!
//! - **Reading never fails.** A missing / unreadable / half-written file yields the two built-ins
//!   and nothing else. The history list must survive a broken registry the same way it survives a
//!   broken config (see `store::default_store` for the same reasoning).
//! - **An unknown version is rejected, not guessed at.** A newer layout read by an older build is
//!   the hardest class of bug to trace, so the version is checked rather than ignored — the same
//!   call as `store::backend` and `config`'s "unrecognised value errors out" rule.
//! - **The two built-ins always exist.** `Default` (the app root) is the one place a conversation
//!   lands without anybody choosing anything, so it can never be absent; `Full access` exists to
//!   give the unrestricted tier somewhere to live (it has no root at all).

// Both halves now have consumers: the GUI's history overlay reads [`group`] and writes through
// [`Registry::add_workspace`] / [`Registry::remove_workspace`].
//
// There is no `allow(dead_code)` in this file on purpose -- and the CLI binary has to silence this
// module at its own `mod` declaration instead (see `src/main.rs`). The two targets genuinely differ:
// the API exists for the GUI, and the CLI links this module only for `Registry::load`, so "never
// used" there is a fact about the target, not about the code.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::history::Summary;
use crate::workplace::{AOX_DIR, WORKPLACE_DIR};

/// The registry file's name, inside `<app root>/.aox/`.
const FILE_NAME: &str = "spaces.json";

/// The on-disk layout version. See the module docs: a mismatch is **rejected**, not migrated by guesswork.
const VERSION: u32 = 1;

/// Ids of the built-ins. Prefixed `builtin:` so "is this ours" is a test on the id itself and cannot
/// drift out of sync with a separate flag.
const ID_DEFAULT: &str = "builtin:default";
const ID_FULL: &str = "builtin:full";

/// The two spaces that are not the user's to create or delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Builtin {
    /// The app root — where a conversation lands when nobody picked anything.
    Default,
    /// The unrestricted tier, which has no root: it cannot charitably be folded into any workspace,
    /// so it gets a group of its own (an explicit decision on 2026-10-14).
    Full,
}

impl Builtin {
    /// The name shown when the space has no name of its own.
    ///
    /// Goes through `t!` like every other user-visible string, so a translated build shows a
    /// translated group name; a **user-named** space is shown verbatim and never translated.
    pub fn label(self) -> &'static str {
        match self {
            Builtin::Default => crate::i18n::t("Default"),
            Builtin::Full => crate::i18n::t("Full access"),
        }
    }
}

/// One space.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Space {
    /// Stable identity. `builtin:*` for the two built-ins, otherwise a generated id.
    pub id: String,
    /// The user-given name. `None` = show the built-in's label (or the root's folder name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builtin: Option<Builtin>,
    /// The directory this space is bound to. `None` for `Full access` (it has none) and for a space
    /// that is a pure label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<PathBuf>,
    /// Unix seconds.
    #[serde(default)]
    pub created: u64,
    /// Conversations **explicitly** placed here. A conversation that is not listed in any space
    /// still shows up — it falls back to a group derived from its own workspace record (see [`group`]).
    ///
    /// Stored on the space rather than as a `conversation -> space` map so that "remove this space"
    /// is one `retain`, with no second structure to keep in sync.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conversations: Vec<String>,
}

impl Space {
    /// The name to display: the user's if there is one, otherwise the built-in's label, otherwise the
    /// root's folder name.
    pub fn display_name(&self) -> String {
        if let Some(n) = self.name.as_ref().filter(|n| !n.trim().is_empty()) {
            return n.clone();
        }
        if let Some(b) = self.builtin {
            return b.label().to_string();
        }
        folder_name(self.root.as_deref()).unwrap_or_else(|| crate::i18n::t("(unnamed)").to_string())
    }
}

/// The on-disk shape. Split from [`Registry`] because the in-memory type wants the built-ins
/// **guaranteed** present, while the file may legitimately lack them (it may not exist at all).
#[derive(Debug, Serialize, Deserialize)]
struct Wire {
    version: u32,
    #[serde(default)]
    spaces: Vec<Space>,
    /// Roots the user removed from AOX. While a root is here it is **neither listed nor scanned**.
    ///
    /// Without this the removal would undo itself: the fallback rule below creates a group for any
    /// workspace root it meets, so a removed workspace would reappear on the very next refresh.
    /// Keeping the mark instead means "add the same folder again" is a real, working undo — the
    /// conversations are still on disk, and dropping the mark brings them all back.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    hidden: Vec<PathBuf>,
}

/// The in-memory registry: the built-ins are always present, in a fixed order.
#[derive(Debug, Clone)]
pub struct Registry {
    spaces: Vec<Space>,
    hidden: Vec<PathBuf>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            spaces: vec![
                Space { id: ID_DEFAULT.into(), builtin: Some(Builtin::Default), ..Default::default() },
                Space { id: ID_FULL.into(), builtin: Some(Builtin::Full), ..Default::default() },
            ],
            hidden: Vec::new(),
        }
    }
}

impl Registry {
    /// Read the registry. **Never fails** — see the module docs.
    pub fn load(app_root: &Path) -> Self {
        let path = file_path(app_root);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        let Ok(wire) = serde_json::from_str::<Wire>(&text) else {
            // A corrupt file yields the built-ins rather than an error. Refusing to start (or
            // refusing to list history) over a UI-organisation file would be a poor trade; the one
            // thing we must not do is *write* over it half-read, and nothing does that.
            return Self::default();
        };
        if wire.version != VERSION {
            return Self::default();
        }
        let mut reg = Self { spaces: wire.spaces, hidden: wire.hidden };
        reg.ensure_builtins();
        reg
    }

    /// Write the registry back, **atomically**.
    ///
    /// Write-then-rename rather than truncate-then-write: the GUI can be killed at any moment, and a
    /// half-written registry is a file the next launch silently discards — losing every grouping the
    /// user had made, for a reason they cannot see. A rename is atomic on both Windows and Unix, so
    /// the file on disk is only ever "the old one" or "the new one".
    pub fn save(&self, app_root: &Path) -> anyhow::Result<()> {
        let dir = app_root.join(AOX_DIR);
        std::fs::create_dir_all(&dir)?;
        let wire = Wire { version: VERSION, spaces: self.spaces.clone(), hidden: self.hidden.clone() };
        let tmp = dir.join(format!("{FILE_NAME}.tmp"));
        std::fs::write(&tmp, serde_json::to_string_pretty(&wire)?)?;
        // `rename` over an existing file is the atomic step; on Windows it fails if the target is
        // open, which is why nothing keeps this file open between calls.
        std::fs::rename(&tmp, file_path(app_root))?;
        Ok(())
    }

    pub fn spaces(&self) -> &[Space] {
        &self.spaces
    }

    pub fn hidden(&self) -> &[PathBuf] {
        &self.hidden
    }

    /// The workspace roots that should be **listed and scanned**: the app root plus every named
    /// space with a root, minus the removed ones.
    ///
    /// This is what the history layer is handed (see `Store::set_workspaces`) — that is the whole
    /// coupling between the two, so the store never has to learn about spaces.
    pub fn workspace_roots(&self, app_root: &Path) -> Vec<PathBuf> {
        let mut out = vec![app_root.to_path_buf()];
        for s in &self.spaces {
            let Some(root) = s.root.as_ref() else { continue };
            if self.is_hidden(root) {
                continue;
            }
            if !out.iter().any(|p| path_key(p) == path_key(root)) {
                out.push(root.clone());
            }
        }
        out
    }

    pub fn is_hidden(&self, root: &Path) -> bool {
        let k = path_key(root);
        self.hidden.iter().any(|h| path_key(h) == k)
    }

    /// Remove a workspace from AOX: **the folder itself is left completely alone** — only the mark
    /// goes in. See [`Wire::hidden`] for why the mark is needed at all.
    ///
    /// Returns the conversations that were registered under it, so the caller can offer to send them
    /// to the recycle bin (the default is to keep them: they are still on disk, and re-adding the
    /// folder brings them back).
    pub fn remove_workspace(&mut self, root: &Path) -> Vec<String> {
        let k = path_key(root);
        let mut dropped = Vec::new();
        self.spaces.retain(|s| {
            let hit = s.root.as_ref().is_some_and(|r| path_key(r) == k);
            if hit {
                dropped.extend(s.conversations.iter().cloned());
            }
            !hit
        });
        if !self.is_hidden(root) {
            self.hidden.push(root.to_path_buf());
        }
        dropped
    }

    /// Undo [`Registry::remove_workspace`] for a folder being added again.
    pub fn unhide(&mut self, root: &Path) {
        let k = path_key(root);
        self.hidden.retain(|h| path_key(h) != k);
    }

    pub fn push_space(&mut self, space: Space) {
        self.spaces.push(space);
    }

    /// Register a workspace root under a user-chosen name, and **undo a previous removal** of the
    /// same folder in the same stroke. Returns the space's id, so the caller can unfold the new group
    /// immediately instead of leaving the user to hunt for it.
    ///
    /// The re-add is matched **by root, not by name**: a folder the user removed and then brings back
    /// is the same workspace, so it keeps its id rather than arriving as a second, empty twin beside
    /// a still-hidden original. That is also why the id is derived from the root (see
    /// [`path_key`]) — the identity has to survive the round trip through the registry file, and the
    /// root is the only thing that does.
    pub fn add_workspace(&mut self, root: &Path, name: Option<String>) -> String {
        self.unhide(root);
        // An empty or all-whitespace name is treated as "no name given" rather than stored as-is:
        // `display_name` falls back to the folder name, which is always better than a blank header.
        let name = name.filter(|n| !n.trim().is_empty());
        if let Some(i) = self.find_root_space(root) {
            // Already known, so this is a re-add: only a name that was actually given is applied --
            // `None` means "keep whatever it was called".
            if name.is_some() {
                self.spaces[i].name = name;
            }
            return self.spaces[i].id.clone();
        }
        let id = format!("s:{}", path_key(root));
        self.spaces.push(Space {
            id: id.clone(),
            name,
            builtin: None,
            root: Some(root.to_path_buf()),
            created: crate::history::unix_now(),
            conversations: Vec::new(),
        });
        id
    }

    /// Rename a workspace — **the label only**; the folder on disk is never touched.
    ///
    /// The registry has always kept the two apart (`Space::name` is what the tree shows, `root` is
    /// the directory), and this is the operation that says so out loud. The alternative — renaming
    /// the folder — cannot be done from here at all: a conversation records the folder it was
    /// written to, so moving it would strand every conversation under a root the registry no longer
    /// knows, and each one would come back as a brand-new auto group while this group sat there
    /// empty. The label has no such reach.
    ///
    /// An empty name is **"go back to the folder name"**, not a refusal: `display_name` already falls
    /// back to it, so clearing the field is the working way to undo a rename. A name that *equals*
    /// the folder's own name is normalised to `None` for the same reason — `Some("notes")` and `None`
    /// on a folder called `notes` are the same row, and only one of them keeps the file honest.
    ///
    /// Returns the space's id, so the caller can unfold the group it just renamed: an **auto** group
    /// (a root the registry has never heard of) is registered by this call, and the tree has to show
    /// that it happened.
    pub fn rename_workspace(&mut self, root: &Path, name: Option<String>) -> String {
        let name = name
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .filter(|n| Some(n.as_str()) != folder_name(Some(root)).as_deref());
        match self.find_root_space(root) {
            Some(i) => {
                self.spaces[i].name = name;
                self.spaces[i].id.clone()
            }
            // Never registered — an auto group the user is naming for the first time. Registering it
            // is what makes the label survive a restart, and it is the same gesture `add_workspace`
            // already performs, so it is delegated rather than written a second time.
            None => self.add_workspace(root, name),
        }
    }

    /// Re-file a conversation into another space — **a placement, not a move**.
    ///
    /// The conversation's file stays exactly where it is, and the `kind:"workplace"` record inside it
    /// keeps describing where writes are allowed. What changes is only which group the tree draws it
    /// under, which is precisely the split this module exists to keep (see the module docs): the
    /// registry can say "show this over there" without the conversation's own permission boundary
    /// moving an inch.
    ///
    /// One consequence follows from that split, and it is worth stating plainly: the store lists
    /// conversations by **where the file is**, never by where it is placed. Placing a conversation that
    /// lives in workspace A into workspace B therefore does not make it outlive A being removed from
    /// AOX — only the file's own location does. That is the price of "placement ≠ boundary", and it is
    /// the right price: the only alternative is rewriting the boundary, which is a security decision
    /// nobody made by picking a row in a menu.
    ///
    /// The conversation is removed from **every** space's list before it is added to the target, so it
    /// is placed in at most one space and the call is idempotent. Undo is therefore just another call
    /// naming the original group — there is deliberately no separate "unplace".
    ///
    /// A target the registry has never heard of (an `auto:` group drawn for an unregistered root) is
    /// **registered** by this call, the same gesture [`Registry::rename_workspace`] performs: an auto
    /// group has no row to hold a placement, so the only way to place something in it is to make it a
    /// real space. Returns the id the conversation ended up under — normally the target's own id,
    /// except in that one case, where a fresh `s:<root>` id replaces the `auto:<root>` one.
    pub fn place_conversation(
        &mut self,
        conv_id: &str,
        target_id: &str,
        target_root: Option<&Path>,
    ) -> String {
        // Resolve the target **first**: draining the conversation from every other list is only safe
        // once there is somewhere to put it back. An unresolvable target leaves the registry exactly
        // as it was, rather than silently un-placing the conversation as a side effect.
        let id = match self.index_of(target_id) {
            Some(i) => self.spaces[i].id.clone(),
            None => match target_root {
                Some(root) => self.add_workspace(root, None),
                None => return target_id.to_string(),
            },
        };
        for s in &mut self.spaces {
            s.conversations.retain(|c| c != conv_id);
        }
        if let Some(i) = self.index_of(&id) {
            self.spaces[i].conversations.push(conv_id.to_string());
        }
        id
    }

    fn find_root_space(&self, root: &Path) -> Option<usize> {
        let k = path_key(root);
        self.spaces.iter().position(|s| s.root.as_ref().is_some_and(|r| path_key(r) == k))
    }

    fn index_of(&self, id: &str) -> Option<usize> {
        self.spaces.iter().position(|s| s.id == id)
    }

    /// Guarantee the two built-ins exist and sit first — the file may be missing them (hand-edited,
    /// or written by a future build that dropped one), and every grouping path assumes they are there.
    fn ensure_builtins(&mut self) {
        for (id, b) in [(ID_DEFAULT, Builtin::Default), (ID_FULL, Builtin::Full)] {
            if !self.spaces.iter().any(|s| s.id == id) {
                self.spaces.insert(0, Space { id: id.into(), builtin: Some(b), ..Default::default() });
            }
        }
        // Fixed order: the two built-ins always lead, so the list's top never shuffles.
        self.spaces.sort_by_key(|s| match s.builtin {
            Some(Builtin::Default) => 0,
            Some(Builtin::Full) => 1,
            None => 2,
        });
    }
}

/// One rendered group of the history tree.
#[derive(Debug, Clone)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub builtin: Option<Builtin>,
    pub root: Option<PathBuf>,
    /// Newest first, same order the flat list used.
    pub items: Vec<Summary>,
    /// Position of this group's newest item **in the flat source list**, used to order the groups.
    ///
    /// Not a timestamp: the source list is already newest-first, so its position *is* the recency
    /// order — and `Summary::time` is a display string that must never be compared as one.
    newest: Option<usize>,
}

/// Group a flat, already-sorted list of conversations into spaces.
///
/// Three sources, in priority order:
///
/// 1. **An explicit placement** — the conversation id is listed in some space.
/// 2. **The workspace record** — an unrestricted conversation goes to `Full access`; a conversation
///    whose root is the app root, or sits under `<app root>/workplace/`, goes to `Default` (the
///    dated `workplace/<timestamp>/` plots made before 2026-10-08 are folded in here on purpose:
///    they were the *default* tier's own directories, not places the user chose).
/// 3. **An auto group** — any other root the registry has never heard of becomes a space named after
///    its folder. This is what makes the tree look right on first run: the workspaces the user
///    genuinely works in are on disk already, so they must show up without being registered first.
///
/// Conversations under a **removed** root are dropped from the list while that mark is in place.
///
/// `Default` is always present even when empty (it is where a conversation lands when nobody chooses);
/// so is every **registered** space, because the user naming a folder is the one statement that beats
/// "an empty group is noise" — an empty one they did not ask for is dropped instead, along with
/// `Full access` when nothing is in it (an empty group that can never be filled by the user is just
/// noise).
pub fn group(app_root: &Path, reg: &Registry, items: Vec<Summary>) -> Vec<Group> {
    // conversation id -> space index, built once from every space's list.
    let mut placed: HashMap<&str, usize> = HashMap::new();
    for (i, s) in reg.spaces.iter().enumerate() {
        for c in &s.conversations {
            placed.insert(c.as_str(), i);
        }
    }
    let default_idx = reg.index_of(ID_DEFAULT).unwrap_or(0);
    let full_idx = reg.index_of(ID_FULL);

    // Groups are created on first use rather than pre-seeded with empty placeholders that would
    // then have to be cleaned up again.
    let mut groups: Vec<Group> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();

    // Where a group comes from: the registry, or invented for a root nobody registered. Carried as
    // one value so both branches feed the same `slot` call below.
    struct Key {
        id: String,
        name: String,
        builtin: Option<Builtin>,
        root: Option<PathBuf>,
    }
    let key_of = |sp: &Space| Key {
        id: sp.id.clone(),
        name: sp.display_name(),
        builtin: sp.builtin,
        root: sp.root.clone(),
    };

    let slot = |groups: &mut Vec<Group>, index: &mut HashMap<String, usize>, k: Key| -> usize {
        if let Some(&i) = index.get(&k.id) {
            return i;
        }
        groups.push(Group {
            id: k.id.clone(),
            name: k.name,
            builtin: k.builtin,
            root: k.root,
            items: Vec::new(),
            newest: None,
        });
        index.insert(k.id, groups.len() - 1);
        groups.len() - 1
    };

    for (pos, s) in items.into_iter().enumerate() {
        let target: Option<Key> = if let Some(&si) = placed.get(s.id.as_str()) {
            Some(key_of(&reg.spaces[si]))
        } else if s.ws_full {
            Some(key_of(&reg.spaces[full_idx.unwrap_or(default_idx)]))
        } else {
            match s.ws_root.as_deref() {
                // No workspace record at all (an older file), or the app root: both mean "the place a
                // conversation lands when nobody chose anything".
                None => Some(key_of(&reg.spaces[default_idx])),
                Some(r) => match reg.find_root_space(r) {
                    // A removed workspace is dropped before anything else can claim it: the mark is
                    // the authoritative "not listed", and letting a stale space row win here would
                    // undo the removal on the very next refresh.
                    _ if reg.is_hidden(r) => None,
                    // A root the registry already knows keeps the name the user gave it, even when
                    // the conversation in hand was never explicitly placed there. Checked **before**
                    // the workplace rule below, which is what makes a workspace created through the
                    // history panel's "new workspace" work at all: its root lives under
                    // `<app root>/workplace/` by construction, so the dated-plot rule would otherwise
                    // swallow it into Default the moment the tree was redrawn.
                    Some(i) => Some(key_of(&reg.spaces[i])),
                    // The app root, and every unregistered plot under `workplace/`, fold into
                    // Default: `workplace/<timestamp>/` directories made before 2026-10-08 were the
                    // *default* tier's own plots, not places the user chose.
                    None if same_path(r, app_root) || is_inside_workplace(r, app_root) => {
                        Some(key_of(&reg.spaces[default_idx]))
                    }
                    // Any other root the registry has never heard of becomes a space named after its
                    // folder -- that is what makes the tree look right on first run.
                    None => Some(Key {
                        id: format!("auto:{}", path_key(r)),
                        name: folder_name(Some(r))
                            .unwrap_or_else(|| crate::i18n::t("(unnamed)").to_string()),
                        builtin: None,
                        root: Some(r.to_path_buf()),
                    }),
                },
            }
        };
        let Some(k) = target else { continue };
        let i = slot(&mut groups, &mut index, k);
        if let Some(g) = groups.get_mut(i) {
            g.newest.get_or_insert(pos);
            g.items.push(s);
        }
    }

    // `Default` is unconditional: it is where a conversation lands when nobody chose anything, so the
    // group has to exist before the first conversation does.
    if !index.contains_key(ID_DEFAULT) {
        slot(&mut groups, &mut index, key_of(&reg.spaces[default_idx]));
    }
    // The sweep: an auto group invented for a stray root, or `Full access` with nothing in it, is
    // noise that would otherwise sit there empty forever.
    groups.retain(|g| g.builtin == Some(Builtin::Default) || !g.items.is_empty());
    // `retain` renumbered the vector, so the id -> index map has to be rebuilt before `slot` is
    // trusted again; a stale index would hand back an out-of-range or wrong position.
    index.clear();
    for (i, g) in groups.iter().enumerate() {
        index.insert(g.id.clone(), i);
    }

    // ...and then every **registered** space is put back by name. These are the spaces that exist
    // because a person said so rather than because a conversation landed in them, which is exactly
    // what the sweep above cannot tell apart from an empty stray. A workspace created through "new
    // workspace" is empty by definition on the frame it is made, so without this it would be swept
    // away immediately and the button would look broken.
    //
    // A root that is no longer on disk is skipped, so the row follows the disk: delete the folder
    // outside AOX and the group stops being drawn (the registry entry simply goes dormant, and the
    // folder coming back brings it back).
    for s in &reg.spaces {
        let Some(root) = s.root.as_ref() else { continue };
        if reg.is_hidden(root) || !root.is_dir() {
            continue;
        }
        if !index.contains_key(&s.id) {
            slot(&mut groups, &mut index, key_of(s));
        }
    }

    // Built-ins first, then by "when this group was last active" — derived from the source list's
    // position, so no extra timestamp has to be carried around.
    groups.sort_by_key(|g| {
        let builtin_rank = match g.builtin {
            Some(Builtin::Default) => 0,
            Some(Builtin::Full) => 1,
            None => 2,
        };
        (builtin_rank, g.newest.unwrap_or(usize::MAX))
    });
    groups
}

/// The folder name of a workspace root, for naming an auto group.
fn folder_name(root: Option<&Path>) -> Option<String> {
    let p = root?;
    p.file_name().map(|n| n.to_string_lossy().to_string())
}

/// Whether `p` is the app root itself or lives under `<app root>/workplace/`.
fn is_inside_workplace(p: &Path, app_root: &Path) -> bool {
    let base = path_key(&app_root.join(WORKPLACE_DIR));
    let k = path_key(p);
    k == base || k.starts_with(&format!("{base}/"))
}

fn same_path(a: &Path, b: &Path) -> bool {
    path_key(a) == path_key(b)
}

/// A comparison key for a directory: separators normalised, trailing separator dropped, and
/// **case-folded on Windows only** — on Unix two paths differing in case are two different
/// directories, and folding them there would silently merge two workspaces into one group.
fn path_key(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let s = s.trim_end_matches('/').to_string();
    #[cfg(windows)]
    let s = s.to_lowercase();
    s
}

fn file_path(app_root: &Path) -> PathBuf {
    app_root.join(AOX_DIR).join(FILE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn summary(id: &str, ws_root: Option<&str>, ws_full: bool) -> Summary {
        Summary {
            id: id.into(),
            path: PathBuf::from(format!("{id}.jsonl")),
            title: id.into(),
            messages: 1,
            time: "10-14 10:00".into(),
            ws_root: ws_root.map(PathBuf::from),
            ws_full,
        }
    }

    fn ids(g: &Group) -> Vec<&str> {
        g.items.iter().map(|i| i.id.as_str()).collect()
    }

    /// The app root is always there, even with nothing in it: it is where a conversation lands when
    /// nobody chose anything, so the group must exist before the first conversation does.
    #[test]
    fn default_group_is_always_present() {
        let app = PathBuf::from("E:/app");
        let groups = group(&app, &Registry::default(), Vec::new());
        assert_eq!(groups.len(), 1, "only Default, and no empty Full access group");
        assert_eq!(groups[0].builtin, Some(Builtin::Default));
    }

    /// A conversation bound to the app root, one bound to a dated plot, and one with no record at all
    /// all land in Default; the unrestricted one goes to its own group rather than being folded in.
    #[test]
    fn fallback_folds_default_tier_shapes_together() {
        let app = PathBuf::from("E:/app");
        let items = vec![
            summary("a", Some("E:/app"), false),
            summary("b", Some("E:/app/workplace/20260101000000"), false),
            summary("c", None, false),
            summary("d", None, true),
        ];
        let groups = group(&app, &Registry::default(), items);
        let default = groups.iter().find(|g| g.builtin == Some(Builtin::Default)).unwrap();
        assert_eq!(ids(default), vec!["a", "b", "c"]);
        let full = groups.iter().find(|g| g.builtin == Some(Builtin::Full)).unwrap();
        assert_eq!(ids(full), vec!["d"]);
    }

    /// A root nobody registered becomes a group named after its folder — otherwise the first run
    /// would show every real workspace as "unplaced".
    #[test]
    fn unknown_root_becomes_a_named_group() {
        let app = PathBuf::from("E:/app");
        let items = vec![summary("a", Some("E:/rust22"), false)];
        let groups = group(&app, &Registry::default(), items);
        assert_eq!(groups.len(), 2, "Default plus the auto group");
        assert_eq!(groups[1].name, "rust22");
        assert_eq!(groups[1].root.as_deref(), Some(Path::new("E:/rust22")));
    }

    /// An explicit placement outranks the workspace record: a conversation whose boundary points at
    /// the app root can still be shown inside a user-created space.
    #[test]
    fn explicit_placement_wins_over_the_record() {
        let app = PathBuf::from("E:/app");
        let mut reg = Registry::default();
        reg.push_space(Space {
            id: "s-1".into(),
            name: Some("Ideas".into()),
            root: None,
            conversations: vec!["a".into()],
            ..Default::default()
        });
        let groups = group(&app, &reg, vec![summary("a", Some("E:/app"), false)]);
        let ideas = groups.iter().find(|g| g.name == "Ideas").unwrap();
        assert_eq!(ids(ideas), vec!["a"]);
        let default = groups.iter().find(|g| g.builtin == Some(Builtin::Default)).unwrap();
        assert!(default.items.is_empty());
    }

    /// A removed workspace is neither listed nor turned into an auto group: if it were, the removal
    /// would be undone on the very next refresh by the fallback rule.
    #[test]
    fn removed_workspace_disappears_and_can_come_back() {
        let app = PathBuf::from("E:/app");
        let mut reg = Registry::default();
        reg.hidden.push(PathBuf::from("E:/rust22"));
        let groups = group(&app, &reg, vec![summary("a", Some("E:/rust22"), false)]);
        assert_eq!(groups.len(), 1, "Default only — the removed workspace is invisible");
        assert!(groups[0].items.is_empty());

        reg.unhide(Path::new("E:/rust22"));
        let groups = group(&app, &reg, vec![summary("a", Some("E:/rust22"), false)]);
        assert_eq!(groups.len(), 2);
        assert_eq!(ids(&groups[1]), vec!["a"]);
    }

    /// Case differences must not split one workspace into two groups on Windows; on Unix they are
    /// genuinely two directories, so this only asserts the Windows behaviour.
    #[test]
    #[cfg(windows)]
    fn path_comparison_ignores_case_on_windows() {
        assert!(same_path(Path::new("E:/RustApp"), Path::new("e:\\rustapp")));
    }

    /// The boot wiring, end to end and against a **real file**: save a registry naming a second
    /// workspace, read it back, hand its roots to the *default* store (the same trait object the
    /// bridge and the GUI use), and the store must then list the conversations living in that
    /// workspace.
    ///
    /// This covers the one seam no other test touches — `Registry::load` → `Store::set_workspaces`
    /// → `Store::list` — and it is deliberately written through the trait object rather than through
    /// `JsonlStore` directly, because "the call went to the wrong impl" is exactly the kind of bug a
    /// concrete-type test would hide.
    #[test]
    fn registry_on_disk_widens_what_the_default_store_lists() {
        let app = std::env::temp_dir().join(format!("aox-space-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&app);
        let ws = app.join("project1");
        let store = crate::store::default_store(&app, 0);

        // One conversation in the default workspace, one in the second.
        //
        // Each gets a `kind:"workplace"` record, because that line is what `Summary::ws_root` is read
        // from — a conversation without one always groups under Default (see the "no record at all"
        // branch in [`group`]). Writing only the message would make this test assert nothing about
        // the workspace path at all.
        {
            let mut log = store.create(None, None).unwrap();
            log.append(&crate::message::Message::user("in the default workspace"), None);
            log.append_workplace(&crate::workplace::Record {
                mode: "default".into(),
                root: Some(app.clone()),
                own: None,
            });
        }
        {
            let mut log = store.create(None, Some(&ws)).unwrap();
            log.append(&crate::message::Message::user("in project1"), None);
            log.append_workplace(&crate::workplace::Record {
                mode: "fixed".into(),
                root: Some(ws.clone()),
                own: None,
            });
        }
        assert_eq!(store.list(10).len(), 1, "before registering, only the default workspace is scanned");

        let mut reg = Registry::default();
        reg.push_space(Space {
            id: "s-1".into(),
            name: Some("Playground".into()),
            root: Some(ws.clone()),
            ..Default::default()
        });
        reg.save(&app).unwrap();
        let reg = Registry::load(&app);
        store.set_workspaces(&reg.workspace_roots(&app));

        let items = store.list(10);
        assert_eq!(
            items.len(),
            2,
            "the second workspace's conversation must be listed: {:?}",
            items.iter().map(|s| s.id.clone()).collect::<Vec<_>>()
        );
        let groups = group(&app, &reg, items);
        assert!(
            groups.iter().any(|g| g.name == "Playground"),
            "and it must land in its named group: {:?}",
            groups.iter().map(|g| g.name.clone()).collect::<Vec<_>>()
        );

        let _ = std::fs::remove_dir_all(&app);
    }

    /// The registry file must round-trip: the built-ins injected in memory are written out only when
    /// they carry something worth keeping, and reading back restores the same grouping.
    #[test]
    fn round_trip_keeps_a_named_space() {
        let mut reg = Registry::default();
        reg.push_space(Space {
            id: "s-2".into(),
            name: Some("Rust".into()),
            root: Some(PathBuf::from("E:/rust22")),
            conversations: vec!["a".into()],
            ..Default::default()
        });
        let wire = Wire { version: VERSION, spaces: reg.spaces.clone(), hidden: reg.hidden.clone() };
        let text = serde_json::to_string(&wire).unwrap();
        let back: Wire = serde_json::from_str(&text).unwrap();
        let mut back = Registry { spaces: back.spaces, hidden: back.hidden };
        back.ensure_builtins();
        assert_eq!(back.workspace_roots(Path::new("E:/app")), vec![
            PathBuf::from("E:/app"),
            PathBuf::from("E:/rust22")
        ]);
    }

    /// A workspace registered under `<app root>/workplace/` is **its own group**, not part of Default
    /// — even though it sits exactly where the dated-plot rule folds things in. That placement is a
    /// consequence of where "new workspace" puts a new folder, so if the fallback rule ran first the
    /// space would be registered and then never be visible as anything but Default.
    #[test]
    fn registered_root_under_workplace_is_its_own_group() {
        let base = std::env::temp_dir().join(format!("aox-space-ws-{}", std::process::id()));
        let app = base.join("app");
        let ws = app.join(crate::workplace::WORKPLACE_DIR).join("notes");
        std::fs::create_dir_all(&ws).unwrap();

        let mut reg = Registry::default();
        let id = reg.add_workspace(&ws, Some("Notes".into()));
        assert_eq!(id, format!("s:{}", path_key(&ws)), "the id is derived from the root");

        let root = ws.to_string_lossy().to_string();
        let groups = group(&app, &reg, vec![summary("a", Some(&root), false)]);
        let notes = groups.iter().find(|g| g.name == "Notes").expect("its own group");
        assert_eq!(ids(notes), vec!["a"]);
        assert!(
            groups.iter().find(|g| g.builtin == Some(Builtin::Default)).unwrap().items.is_empty(),
            "and nothing leaked into Default"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A registered workspace is listed **before** it has a conversation in it: that is the frame the
    /// "new workspace" button lands on, and without it the group would be swept out as "empty and
    /// unwanted", so creating a workspace would look like it did nothing.
    #[test]
    fn a_registered_space_shows_while_still_empty() {
        let base = std::env::temp_dir().join(format!("aox-space-empty-{}", std::process::id()));
        let app = base.join("app");
        let ws = app.join(crate::workplace::WORKPLACE_DIR).join("fresh");
        std::fs::create_dir_all(&ws).unwrap();

        let mut reg = Registry::default();
        reg.add_workspace(&ws, Some("Fresh".into()));
        let names = |g: &[Group]| g.iter().map(|x| x.name.clone()).collect::<Vec<_>>();
        assert!(group(&app, &reg, Vec::new()).iter().any(|g| g.name == "Fresh"));

        // ...but only while the folder is really there. A registered root deleted outside AOX stops
        // being drawn rather than sitting there as a permanently empty row (the registry entry simply
        // goes dormant, and the folder coming back brings it back).
        std::fs::remove_dir_all(&ws).unwrap();
        assert!(!group(&app, &reg, Vec::new()).iter().any(|g| g.name == "Fresh"), "{:?}", names(&group(&app, &reg, Vec::new())));

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Removing and re-adding a folder is **one workspace, not two**: it keeps its id, and the
    /// removal mark is dropped so the conversations that stayed on disk reappear.
    ///
    /// The id surviving matters beyond tidiness: the two fold-state sets in the GUI are keyed by it,
    /// so a group the user had unfolded stays unfolded across a remove and re-add.
    #[test]
    fn add_workspace_revives_the_same_space() {
        let base = std::env::temp_dir().join(format!("aox-space-revive-{}", std::process::id()));
        let ws = base.join("proj");
        std::fs::create_dir_all(&ws).unwrap();

        let mut reg = Registry::default();
        let id = reg.add_workspace(&ws, Some("Proj".into()));
        assert_eq!(reg.spaces().iter().find(|s| s.id == id).unwrap().display_name(), "Proj");

        reg.remove_workspace(&ws);
        assert!(reg.is_hidden(&ws), "the folder is marked as removed");
        assert!(!reg.spaces().iter().any(|s| s.id == id), "and the space itself is gone");

        let again = reg.add_workspace(&ws, Some("Proj again".into()));
        assert_eq!(again, id, "the id is derived from the root, so re-adding lands on the same space");
        assert!(!reg.is_hidden(&ws), "the removal mark is dropped");
        assert_eq!(reg.spaces().iter().find(|s| s.id == id).unwrap().display_name(), "Proj again");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Renaming moves **the label, never the folder**: the root is left exactly where it was, so the
    /// conversations that record it keep landing in the same group — which is the whole reason this
    /// is not a directory move.
    ///
    /// It also has to give an unregistered root a home. A root the registry has never heard of is
    /// drawn as an auto group, and nothing about that group can hold a name; renaming is what turns
    /// it into a registered space, i.e. the only reason the new name is still there next launch.
    #[test]
    fn rename_moves_the_label_and_leaves_the_root_alone() {
        let base = std::env::temp_dir().join(format!("aox-space-rename-{}", std::process::id()));
        let app = base.join("app");
        let ws = app.join(crate::workplace::WORKPLACE_DIR).join("notes");
        std::fs::create_dir_all(&ws).unwrap();
        let root = ws.to_string_lossy().to_string();

        // Never registered, so this call is also the registration.
        let mut reg = Registry::default();
        let id = reg.rename_workspace(&ws, Some("ClientX".into()));
        assert_eq!(id, format!("s:{}", path_key(&ws)), "the id is still derived from the root");
        let names = |g: Vec<Group>| g.into_iter().map(|x| x.name).collect::<Vec<_>>();
        let g = group(&app, &reg, vec![summary("a", Some(&root), false)]);
        assert!(g.iter().any(|x| x.name == "ClientX"), "{:?}", names(g));

        // The folder is untouched, and the space still points at it.
        assert!(ws.is_dir(), "the directory is not moved");
        let sp = reg.spaces().iter().find(|s| s.id == id).unwrap();
        assert_eq!(sp.root.as_deref(), Some(ws.as_path()));

        // Naming it after the folder, and clearing the name, both mean "show the folder name" — so
        // neither writes anything: `Some("notes")` and `None` would draw the same row.
        reg.rename_workspace(&ws, Some("notes".into()));
        assert!(reg.spaces().iter().find(|s| s.id == id).unwrap().name.is_none(), "{:?}", reg.spaces());
        reg.rename_workspace(&ws, None);
        let sp = reg.spaces().iter().find(|s| s.id == id).unwrap();
        assert!(sp.name.is_none());
        assert_eq!(sp.display_name(), "notes");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A conversation can be re-filed into another space, and it lands in **exactly one** place: the
    /// call drains it from wherever it was first, so repeated moves never leave it listed twice.
    ///
    /// The placement also has to **outrank the conversation's own workspace record** — that is the
    /// whole point of a separate registry — so the assertion is not just "it moved" but "it is drawn in
    /// B while its file still says A".
    #[test]
    fn place_conversation_moves_it_and_keeps_one_copy() {
        let app = PathBuf::from("E:/app");
        let mut reg = Registry::default();
        let a = reg.add_workspace(Path::new("E:/a"), Some("A".into()));
        let b = reg.add_workspace(Path::new("E:/b"), Some("B".into()));
        // The record says the file lives under E:/a, so the fallback draws it in A on its own.
        let item = || summary("c1", Some("E:/a"), false);
        let named = |g: Vec<Group>, n: &str| -> Vec<String> {
            g.into_iter()
                .find(|x| x.name == n)
                .map(|x| x.items.iter().map(|i| i.id.clone()).collect())
                .unwrap_or_default()
        };
        assert_eq!(named(group(&app, &reg, vec![item()]), "A"), vec!["c1"]);

        // Placed in B, which outranks the record...
        reg.place_conversation("c1", &b, None);
        let g = group(&app, &reg, vec![item()]);
        assert_eq!(named(g.clone(), "B"), vec!["c1"]);
        assert!(named(g, "A").is_empty(), "and it is not drawn in both");

        // ...and moving it back is just another call: there is no separate "unplace".
        reg.place_conversation("c1", &a, None);
        let g = group(&app, &reg, vec![item()]);
        assert_eq!(named(g.clone(), "A"), vec!["c1"]);
        assert!(named(g, "B").is_empty());
    }

    /// Placing into a group the registry has never heard of — an `auto:` group drawn for an
    /// unregistered root — **registers** that root, because an auto group carries no row to hold a
    /// placement. The conversation then lives under the new `s:<root>` id.
    #[test]
    fn place_conversation_registers_an_auto_group() {
        let app = PathBuf::from("E:/app");
        let mut reg = Registry::default();
        let items = || vec![summary("c1", Some("E:/app"), false), summary("c2", Some("E:/a"), false)];

        // E:/a is drawn as an auto group, since nothing has registered it.
        let auto = group(&app, &reg, items());
        let auto_a = auto.iter().find(|g| g.root.as_deref() == Some(Path::new("E:/a"))).unwrap();
        assert!(auto_a.id.starts_with("auto:"), "drawn as auto: {}", auto_a.id);

        let id = reg.place_conversation("c1", &auto_a.id, auto_a.root.as_deref());
        assert!(id.starts_with("s:"), "the auto group is registered: {id}");

        // ...and c1 now sits in that group alongside c2, which was drawn there all along.
        let g = group(&app, &reg, items());
        let a = g.iter().find(|x| x.name == "a").expect("a group named after the folder");
        assert_eq!(ids(a), vec!["c1", "c2"]);
    }
}
