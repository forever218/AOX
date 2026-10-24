//! Which command tells this project that it still works.
//!
//! The host cannot run a project's tests on its own — it may take minutes, need the network, or have
//! side effects — but it can say **which command** would answer the question, and that turns "you
//! should verify this" from advice into an instruction the model can follow in one call.
//!
//! Detection is deliberately shallow and honest: a handful of marker files in the workspace root and
//! its immediate subdirectories, first match wins. It is a **hint**, not a command line — a wrong
//! guess costs nothing (the model is free to ignore it), while a config knob for it would add one more
//! "written but not in effect" trap. Projects that want something else can say so in `RULES.md`.

use std::path::{Path, PathBuf};

/// A detected check command and the file that gave it away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// e.g. `cargo test`. Commands are written the way a user would type them.
    pub command: String,
    /// The marker that produced it (`Cargo.toml`, `package.json`, …) — shown next to the command so a
    /// wrong guess is obvious at a glance instead of mysterious.
    pub from: String,
}

impl Check {
    /// The words that must all appear in a shell command for it to count as "the check ran".
    ///
    /// Flags are dropped (`pytest -q` → `pytest`) and so are version/size suffixes: what is left is
    /// the program name and, when there is one, the subcommand — `cargo test` is not satisfied by
    /// `cargo build`, which compiles the code but proves nothing about its behaviour.
    pub fn words(&self) -> Vec<&str> {
        self.command
            .split_whitespace()
            .filter(|w| !w.starts_with('-'))
            .take(2)
            .collect()
    }

    /// Did a shell command run this check?
    ///
    /// Matching is "contains every word", not "equals": `cargo test --lib --all-targets` counts, and
    /// so does `cargo test 2>&1 | tail -40`.
    pub fn ran_in(&self, command_line: &str) -> bool {
        let words = self.words();
        !words.is_empty() && words.iter().all(|w| command_line.contains(w))
    }
}

/// Which directories to look in: the root first, then its immediate children in name order.
///
/// One level down matters in practice — this very repository keeps a second `Cargo.toml` under
/// `gui/`, and plenty of projects put the app in `frontend/` or `server/`. Going deeper would start
/// to cost real time on big trees and would find `node_modules` before it found anything useful.
fn candidates(root: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![root.to_path_buf()];
    if let Ok(entries) = std::fs::read_dir(root) {
        let mut subs: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.path())
            .filter(|p| {
                // Directories nobody's project root lives in, and the two that would make this slow.
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                !matches!(name, ".git" | ".aox" | "target" | "node_modules" | "dist" | "build" | ".venv")
            })
            .collect();
        subs.sort();
        dirs.extend(subs);
    }
    dirs
}

/// The project's check command, if one can be recognised.
pub fn detect(root: &Path) -> Option<Check> {
    for dir in candidates(root) {
        if let Some(c) = at(&dir) {
            return Some(c);
        }
    }
    None
}

fn at(dir: &Path) -> Option<Check> {
    if dir.join("Cargo.toml").is_file() {
        return Some(Check { command: "cargo test".into(), from: "Cargo.toml".into() });
    }
    if let Some(c) = npm(dir) {
        return Some(c);
    }
    if dir.join("go.mod").is_file() {
        return Some(Check { command: "go test ./...".into(), from: "go.mod".into() });
    }
    if dir.join("pytest.ini").is_file()
        || dir.join("pyproject.toml").is_file()
        || dir.join("tests").is_dir()
    {
        let from = if dir.join("pyproject.toml").is_file() {
            "pyproject.toml"
        } else if dir.join("pytest.ini").is_file() {
            "pytest.ini"
        } else {
            "tests/"
        };
        return Some(Check { command: "pytest -q".into(), from: from.into() });
    }
    None
}

/// `npm test` only when `package.json` actually defines one — Node projects without a `test` script
/// are common, and suggesting a command that fails immediately is worse than saying nothing.
fn npm(dir: &Path) -> Option<Check> {
    let text = std::fs::read_to_string(dir.join("package.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let script = v.get("scripts")?.get("test")?.as_str()?.trim();
    if script.is_empty() {
        return None;
    }
    Some(Check { command: "npm test".into(), from: "package.json".into() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aox-checks-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn a_cargo_project_is_cargo_test() {
        let d = tmp("cargo");
        touch(&d, "Cargo.toml", "[package]\nname = \"x\"\n");
        let c = detect(&d).expect("a Cargo.toml is enough");
        assert_eq!(c.command, "cargo test");
        assert_eq!(c.from, "Cargo.toml");
    }

    #[test]
    fn an_empty_directory_has_nothing_to_say() {
        let d = tmp("empty");
        assert_eq!(detect(&d), None, "guessing here would be worse than silence");
    }

    #[test]
    fn a_package_json_without_a_test_script_is_not_npm_test() {
        let d = tmp("npm-no-test");
        touch(&d, "package.json", r#"{"scripts":{"build":"tsc"}}"#);
        assert_eq!(detect(&d), None, "a command that fails immediately is worse than no command");
    }

    #[test]
    fn a_package_json_with_a_test_script_is() {
        let d = tmp("npm-test");
        touch(&d, "package.json", r#"{"scripts":{"test":"jest"}}"#);
        assert_eq!(detect(&d).unwrap().command, "npm test");
    }

    #[test]
    fn python_and_go_are_recognised_too() {
        let d = tmp("py");
        touch(&d, "pytest.ini", "[pytest]\n");
        assert_eq!(detect(&d).unwrap().command, "pytest -q");

        let g = tmp("go");
        touch(&g, "go.mod", "module x\n");
        assert_eq!(detect(&g).unwrap().command, "go test ./...");
    }

    #[test]
    fn the_project_root_one_level_down_is_still_found() {
        // `gui/Cargo.toml` next to a root that has none: exactly this repository's shape.
        let d = tmp("nested");
        touch(&d, "gui/Cargo.toml", "[package]\nname = \"gui\"\n");
        touch(&d, "README.md", "hi");
        assert_eq!(detect(&d).unwrap().command, "cargo test");
    }

    #[test]
    fn the_root_wins_over_a_subdirectory() {
        let d = tmp("precedence");
        touch(&d, "Cargo.toml", "");
        touch(&d, "aaa/package.json", r#"{"scripts":{"test":"jest"}}"#);
        assert_eq!(detect(&d).unwrap().command, "cargo test", "root first, children in name order after");
    }

    #[test]
    fn a_build_is_not_the_check() {
        let c = Check { command: "cargo test".into(), from: "Cargo.toml".into() };
        assert!(c.ran_in("cargo test --lib"));
        assert!(c.ran_in("cd gui && cargo test 2>&1 | tail"));
        assert!(!c.ran_in("cargo build --release"), "compiling proves it compiles, not that it works");
        assert!(!c.ran_in("cargo check"), "ditto");
        assert!(!c.ran_in("ls"));
    }

    #[test]
    fn a_single_word_check_ignores_its_flags() {
        let c = Check { command: "pytest -q".into(), from: "pyproject.toml".into() };
        assert_eq!(c.words(), vec!["pytest"]);
        assert!(c.ran_in("python -m pytest tests/"));
    }
}
