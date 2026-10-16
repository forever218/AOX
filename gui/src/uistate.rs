//! The GUI's own little state file: **which workspace groups the user has folded or unfolded**.
//!
//! Why this is not part of the space registry (`src/space.rs`): that file answers "which workspaces
//! exist, and what lives in them" — identity and membership. This one answers "what does the tree
//! look like" — a view. They also differ in what they can name: a space is a fact about the disk,
//! while a fold is a preference about a row, and a fold may legitimately point at a group the
//! registry has never heard of (the `auto:` groups drawn for roots nobody registered). Keeping the
//! two apart means the registry's on-disk version never has to be bumped for a UI tweak, and no
//! viewer has to reason about a `Space` field that is not about spaces.
//!
//! Two properties, both copied from `space.rs` because the same reasoning applies:
//!
//! - **Reading never fails.** A missing / unreadable / half-written file yields "nothing folded",
//!   which is exactly the default the tree already runs on — so a broken file is indistinguishable
//!   from a fresh install, never an error the user has to resolve.
//! - **Writing is atomic** (temp file + rename). The GUI can be killed at any moment, and a
//!   half-written file is one the next launch silently discards — taking every fold with it.
//!
//! The file lives at `<app root>/.aox/ui.json` — beside `spaces.json`, for the same reason: it
//! describes the one window, not any single workspace, so it cannot be scattered across them.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use aox::workplace::AOX_DIR;

/// The file's name, inside `<app root>/.aox/`.
const FILE_NAME: &str = "ui.json";

/// The on-disk layout version — **rejected, not guessed at**, when it does not match. The same call
/// `space.rs` makes: a newer layout read by an older build is the hardest class of bug to trace.
const VERSION: u32 = 1;

/// Which groups are **explicitly** open / closed, by [`aox::space::Group::id`].
///
/// The two sets mirror `App::hist_open` / `App::hist_closed` one-to-one: an id in neither set has no
/// explicit choice and falls back to the tree's default (drawn open iff it holds the conversation
/// being chatted in). **Only explicit choices are written**, so that default stays defined in exactly
/// one place (`App::group_is_open`) instead of being frozen into the file.
#[derive(Debug, Default, Clone)]
pub struct FoldState {
    /// Groups the user has explicitly unfolded.
    pub open: HashSet<String>,
    /// Groups the user has explicitly folded.
    pub closed: HashSet<String>,
}

/// The on-disk shape. Both lists are sorted on write, so the file is stable across saves (an
/// unchanged tree rewrites the same bytes rather than shuffling `HashSet` order into the diff).
#[derive(Debug, Serialize, Deserialize)]
struct Wire {
    version: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    open: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    closed: Vec<String>,
}

impl FoldState {
    /// Read the fold state. **Never fails** — see the module docs.
    pub fn load(app_root: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(file_path(app_root)) else {
            return Self::default();
        };
        let Ok(wire) = serde_json::from_str::<Wire>(&text) else {
            return Self::default();
        };
        if wire.version != VERSION {
            return Self::default();
        }
        Self {
            open: wire.open.into_iter().collect(),
            closed: wire.closed.into_iter().collect(),
        }
    }

    /// Write the fold state back, **atomically**. See the module docs for why a rename rather than a
    /// truncate-and-write.
    pub fn save(&self, app_root: &Path) -> anyhow::Result<()> {
        let dir = app_root.join(AOX_DIR);
        std::fs::create_dir_all(&dir)?;
        let mut open: Vec<String> = self.open.iter().cloned().collect();
        let mut closed: Vec<String> = self.closed.iter().cloned().collect();
        open.sort();
        closed.sort();
        let wire = Wire { version: VERSION, open, closed };
        let tmp = dir.join(format!("{FILE_NAME}.tmp"));
        std::fs::write(&tmp, serde_json::to_string_pretty(&wire)?)?;
        // The rename over an existing file is the atomic step; nothing keeps this file open between
        // calls, which is what makes it work on Windows too.
        std::fs::rename(&tmp, file_path(app_root))?;
        Ok(())
    }
}

fn file_path(app_root: &Path) -> PathBuf {
    app_root.join(AOX_DIR).join(FILE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aox-uistate-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// The two sets have to survive a round trip, or "fold a group, restart" — the whole point of
    /// this file — would quietly do nothing.
    #[test]
    fn round_trip_keeps_the_two_sets() {
        let dir = temp_dir("rt");
        let fs = FoldState {
            open: ["a".to_string(), "b".to_string()].into_iter().collect(),
            closed: ["c".to_string()].into_iter().collect(),
        };
        fs.save(&dir).unwrap();
        let back = FoldState::load(&dir);
        assert_eq!(back.open, fs.open);
        assert_eq!(back.closed, fs.closed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A missing file is "nothing folded", not an error: the tree is usable on a fresh install with
    /// no special-casing at the call site.
    #[test]
    fn a_missing_file_reads_as_nothing_folded() {
        let dir = temp_dir("missing");
        let fs = FoldState::load(&dir);
        assert!(fs.open.is_empty() && fs.closed.is_empty(), "missing dir is not an error");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A corrupt file, and a version this build does not know, both fall back to "nothing folded"
    /// rather than being half-read.
    #[test]
    fn a_broken_or_future_file_reads_as_nothing_folded() {
        let dir = temp_dir("bad");
        std::fs::create_dir_all(dir.join(AOX_DIR)).unwrap();
        std::fs::write(dir.join(AOX_DIR).join(FILE_NAME), b"not json at all").unwrap();
        assert!(FoldState::load(&dir).open.is_empty(), "corrupt file -> default");

        std::fs::write(dir.join(AOX_DIR).join(FILE_NAME), br#"{"version":999,"open":["x"]}"#)
            .unwrap();
        assert!(FoldState::load(&dir).open.is_empty(), "unknown version -> default");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The written file must be **stable**: an unchanged tree rewrites the same bytes, so a
    /// `HashSet`'s iteration order never leaks into the file (and a human diffing it sees nothing).
    #[test]
    fn the_written_file_is_deterministic() {
        let dir = temp_dir("stable");
        let fs = FoldState {
            open: ["z".to_string(), "a".to_string(), "m".to_string()].into_iter().collect(),
            closed: HashSet::new(),
        };
        fs.save(&dir).unwrap();
        let first = std::fs::read_to_string(file_path(&dir)).unwrap();
        fs.save(&dir).unwrap();
        assert_eq!(first, std::fs::read_to_string(file_path(&dir)).unwrap());
        assert!(first.find("\"a\"").unwrap() < first.find("\"m\"").unwrap(), "sorted: {first}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
