//! What earlier sessions wrote down, listed so the model can find it.
//!
//! `.aox/memory/` was **write-only** until 2026-10-23: the prompt told the model to record notes
//! there, and then nothing on the host side ever mentioned the folder again — so a note only helped
//! if a later session happened to think "maybe I should look". The folder is a filing cabinet; this
//! module is the label on its drawer.
//!
//! # Why a table of contents and not the notes themselves
//!
//! Measured on this repository: one note, 4.6 KB ≈ 1.5k tokens. The system prompt is rebuilt into
//! message 0 and therefore travels with **every** request, so injecting the body would cost that on
//! a one-line question that has nothing to do with the note. The headings cost ~60 tokens and carry
//! enough for the model to decide whether to open the file — which is one `read_file` away.
//!
//! Relevance is the real argument, not cost: the work in front of you is often unrelated to what
//! last week's session wrote, and only the model can tell.

use std::path::Path;

/// One note, reduced to what a reader needs to decide whether to open it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// File name, e.g. `2026-10-18.md` (the date is in the name, which is why sorting by name is
    /// sorting by date).
    pub name: String,
    pub bytes: u64,
    /// The note's `## ` section titles, at most [`HEADINGS_MAX`] of them. When the note has none,
    /// the first line of content stands in — a note whose first line is missing has nothing to say.
    pub headings: Vec<String>,
}

/// How many notes to list. Three covers "yesterday, the day before, and the session that started
/// all this"; more is a list nobody reads.
pub const RECENT_MAX: usize = 3;

/// How many section titles per note.
pub const HEADINGS_MAX: usize = 3;

/// Don't read a whole note just to find its headings; a note longer than this is a note whose first
/// few KB already say what it is about.
const READ_LIMIT: u64 = 16 * 1024;

/// The most recent notes under `<root>/.aox/memory/`, newest first.
pub fn recent(root: &Path, max: usize) -> Vec<Note> {
    let dir = root.join(".aox").join("memory");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut files: Vec<(String, u64)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.to_ascii_lowercase().ends_with(".md") {
                return None;
            }
            let meta = e.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            Some((name, meta.len()))
        })
        .collect();
    // `YYYY-MM-DD.md` sorts lexically exactly like it sorts in time, and a name that does not look
    // like a date simply ends up wherever its letters put it — no date parsing, no clock.
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files.truncate(max);
    files
        .into_iter()
        .map(|(name, bytes)| Note { headings: headings_of(&dir.join(&name)), name, bytes })
        .collect()
}

/// The `## ` titles of one note (or its first content line when it has no sections).
fn headings_of(path: &Path) -> Vec<String> {
    use std::io::Read as _;
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut text = String::new();
    // Reading a prefix may cut the last line in half; that line is simply not used unless it happens
    // to be complete, which is fine for a table of contents.
    let _ = file.take(READ_LIMIT).read_to_string(&mut text);
    let mut heads = Vec::new();
    let mut first_line: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Section titles first: an `## x` line also "starts with #", so the order here decides
        // whether the table of contents is empty.
        if let Some(rest) = line.strip_prefix("## ") {
            heads.push(rest.trim().to_string());
            if heads.len() == HEADINGS_MAX {
                break;
            }
            continue;
        }
        if line.starts_with('#') {
            // The note's own `# date` title is not information (the date is in the file name).
            continue;
        }
        if first_line.is_none() {
            first_line = Some(line.to_string());
        }
    }
    if heads.is_empty() {
        heads.extend(first_line);
    }
    heads
}

/// `4.6 KB` / `812 B` — the size is what tells the model whether opening the file is cheap.
pub fn human_size(bytes: u64) -> String {
    if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aox-notes-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".aox/memory")).unwrap();
        dir
    }

    fn note(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(".aox/memory").join(name), body).unwrap();
    }

    #[test]
    fn the_newest_notes_come_first_and_only_a_few_of_them() {
        let d = tmp("order");
        for day in ["2026-10-01", "2026-10-18", "2026-10-05", "2026-09-30"] {
            note(&d, &format!("{day}.md"), "## x\n");
        }
        let got = recent(&d, 3);
        let names: Vec<&str> = got.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["2026-10-18.md", "2026-10-05.md", "2026-10-01.md"]);
    }

    #[test]
    fn an_empty_or_missing_folder_lists_nothing() {
        let d = tmp("empty");
        assert!(recent(&d, 3).is_empty());
        // A workspace that never wrote a note must produce **no segment at all** — the prompt has to
        // stay byte-identical to the version before this feature.
        assert!(recent(&d.join("nope"), 3).is_empty());
    }

    #[test]
    fn section_titles_become_the_table_of_contents() {
        let d = tmp("heads");
        note(
            &d,
            "2026-10-18.md",
            "# 2026-10-18\n\n## AOX GUI：左侧历史栏\n\n正文\n\n## 另一个小节\n\n正文\n\n## 第三个\n\n## 第四个\n",
        );
        let got = recent(&d, 1);
        assert_eq!(
            got[0].headings,
            vec!["AOX GUI：左侧历史栏", "另一个小节", "第三个"],
            "at most three titles, in file order"
        );
    }

    #[test]
    fn a_note_without_sections_falls_back_to_its_first_content_line() {
        let d = tmp("plain");
        note(&d, "2026-10-18.md", "# 2026-10-18\n\n今天把输入框改成了浮动卡片。\n\n更多细节。\n");
        let got = recent(&d, 1);
        assert_eq!(got[0].headings, vec!["今天把输入框改成了浮动卡片。"]);
    }

    #[test]
    fn other_files_are_ignored() {
        let d = tmp("only-md");
        note(&d, "2026-10-18.md", "## 有内容\n");
        std::fs::write(d.join(".aox/memory/notes.txt"), "x").unwrap();
        std::fs::create_dir_all(d.join(".aox/memory/sub.md")).unwrap();
        let got = recent(&d, 5);
        assert_eq!(got.len(), 1, "only *.md files, and only files");
        assert_eq!(got[0].name, "2026-10-18.md");
    }

    #[test]
    fn sizes_read_the_way_a_person_reads_them() {
        assert_eq!(human_size(812), "812 B");
        assert_eq!(human_size(4624), "4.5 KB");
    }

    #[test]
    fn a_note_with_no_readable_content_lists_no_headings() {
        let d = tmp("blank");
        note(&d, "2026-10-18.md", "# only a title\n\n");
        let got = recent(&d, 1);
        assert!(got[0].headings.is_empty(), "nothing to advertise, and it must not invent a title");
    }
}
