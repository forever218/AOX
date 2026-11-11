//! The GUI's own little state file: which workspace groups the user has folded or unfolded, plus whether...
//! Why this is not part of the space registry (`src/space.rs`): that file answers "which workspaces exist...
//! Two properties, both copied from `space.rs` because the same reasoning applies.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use aox::workplace::AOX_DIR;

/// The file's name, inside `<app root>/.aox/`
const FILE_NAME: &str = "ui.json";

/// The on-disk layout version — **rejected, not guessed at**, when it does not match. The same call
/// `space.rs` makes: a newer layout read by an older build is the hardest class of bug to trace.
const VERSION: u32 = 1;

/// Explicit group states and drawer position; only explicit choices are stored, default in App::group_is_open.
#[derive(Debug, Default, Clone)]
pub struct FoldState {
    /// Groups the user has explicitly unfolded.
    pub open: HashSet<String>,
    /// Groups the user has explicitly folded.
    pub closed: HashSet<String>,
    /// Whether the history drawer was open when this state was last written.
    pub drawer: bool,
}

/// The on-disk shape. Both lists are sorted on write, so the file is stable across saves (an unchanged tree...
#[derive(Debug, Serialize, Deserialize)]
struct Wire {
    version: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    open: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    closed: Vec<String>,
    #[serde(default)]
    drawer: bool,
}

impl FoldState {
    /// Read the fold state. **Never fails** — see the module docs
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
            drawer: wire.drawer,
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
        let wire = Wire { version: VERSION, open, closed, drawer: self.drawer };
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

    /// The two sets have to survive a round trip, or "fold a group, restart" — the whole point of this file — would...
    #[test]
    fn round_trip_keeps_the_two_sets_and_the_drawer() {
        let dir = temp_dir("rt");
        for drawer in [true, false] {
            let fs = FoldState {
                open: ["a".to_string(), "b".to_string()].into_iter().collect(),
                closed: ["c".to_string()].into_iter().collect(),
                drawer,
            };
            fs.save(&dir).unwrap();
            let back = FoldState::load(&dir);
            assert_eq!(back.open, fs.open);
            assert_eq!(back.closed, fs.closed);
            assert_eq!(back.drawer, drawer, "the drawer position must survive the round trip");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file written while the drawer was not persisted has no `drawer` key at all.
    #[test]
    fn a_file_without_a_drawer_key_reads_as_folded() {
        let dir = temp_dir("nodrawer");
        std::fs::create_dir_all(dir.join(AOX_DIR)).unwrap();
        std::fs::write(
            dir.join(AOX_DIR).join(FILE_NAME),
            br#"{"version":1,"open":["x"],"closed":["y"]}"#,
        )
        .unwrap();
        let fs = FoldState::load(&dir);
        assert_eq!(fs.open.len(), 1, "the folds still read");
        assert_eq!(fs.closed.len(), 1);
        assert!(!fs.drawer, "no key means the drawer starts folded, like those builds did");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file from before 2026-10-18 carries a `drawer` key with the very same meaning, and it is honoured —...
    #[test]
    fn a_pre_2026_10_18_drawer_key_is_honoured() {
        let dir = temp_dir("legacydrawer");
        std::fs::create_dir_all(dir.join(AOX_DIR)).unwrap();
        std::fs::write(
            dir.join(AOX_DIR).join(FILE_NAME),
            br#"{"version":1,"open":["x"],"closed":["y"],"drawer":true}"#,
        )
        .unwrap();
        let fs = FoldState::load(&dir);
        assert!(fs.drawer, "an old file that said open means open");
        assert_eq!(fs.open.len(), 1, "the rest of the file still reads");
        fs.save(&dir).unwrap();
        let back = FoldState::load(&dir);
        assert!(back.drawer, "and it survives the rewrite");
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
            drawer: true,
        };
        fs.save(&dir).unwrap();
        let first = std::fs::read_to_string(file_path(&dir)).unwrap();
        fs.save(&dir).unwrap();
        assert_eq!(first, std::fs::read_to_string(file_path(&dir)).unwrap());
        assert!(first.find("\"a\"").unwrap() < first.find("\"m\"").unwrap(), "sorted: {first}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
