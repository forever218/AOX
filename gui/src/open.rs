//! Where a clickable run of text leads, and how to actually go there.
//!
//! The model writes absolute paths and URLs into its replies constantly ("generated
//! `E:\rustapp\dist\aox.exe`"). Before this module the only way to use one was: select it, copy it,
//! switch to Explorer, paste it — three steps for something the text already *is*.
//!
//! Every branch here starts a **program on the user's machine**, so the scope is deliberately
//! narrow:
//!
//! - Only real URLs go to the browser; only absolute local paths go to the file manager. Nothing
//!   else is even classified (a relative `src/main.rs` stays inert — see `markdown::scan`).
//! - A path is never handed to a shell. Explorer is the one exception to "one argument per `arg`":
//!   its parser needs the literal shape `/select,"<path>"`, so that string is built by hand and
//!   passed with `raw_arg`. The quotes inside it cannot be broken out of — Windows file names may
//!   not contain `"` — see `select_arg` for why the obvious `arg` version is wrong.
//! - **A click never executes anything.** For a file the action is "show it in its folder"
//!   (`explorer /select,<file>`), not "run it". That is the conservative behaviour chosen on
//!   2026-10-23: a `.exe` in a model reply is exactly what a mis-aimed click must not launch.

use std::path::{Path, PathBuf};
use std::time::Instant;

/// What a clickable run of text points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Handed to the default browser.
    Url(String),
    /// An absolute path on this machine.
    Path(PathBuf),
}

impl Target {
    /// The text as written, for the tooltip. (Not the resolved path: what the user hovered is what
    /// they should see confirmed.)
    pub fn text(&self) -> String {
        match self {
            Target::Url(u) => u.clone(),
            Target::Path(p) => p.display().to_string(),
        }
    }
}

/// Classify a literal as a target, or `None` when it is not one.
///
/// Two callers with two very different tolerances share this:
///
/// - the **scanner** (`markdown::scan`) only ever hands over things it already knows are a URL or an
///   absolute path, so here it is a no-op filter;
/// - a **markdown link** `[text](href)` hands over whatever the model wrote, which is why this must
///   accept a general `scheme:` — a `mailto:` link should still be clickable.
///
/// The drive-letter case has to be decided **before** the scheme test: `C:\x` looks exactly like a
/// URL with the scheme `c`, and getting that wrong would send a file path to the browser.
pub fn classify(s: &str) -> Option<Target> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if is_absolute_path(s) {
        return Some(Target::Path(PathBuf::from(s)));
    }
    if has_scheme(s) {
        return Some(Target::Url(s.to_string()));
    }
    None
}

/// `X:\…`, `X:/…`, or `\\server\share`.
///
/// Only absolute: a bare `aox.exe` or `src/main.rs` is far more likely to be prose about a file than
/// a promise that it exists at that place right now, and underlining all of them would turn every
/// reply into a wall of links.
fn is_absolute_path(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() >= 3
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b[2] == b'\\' || b[2] == b'/')
    {
        return true;
    }
    s.starts_with(r"\\")
}

/// Does it start with `scheme:`? A drive letter is rejected by the `i > 1` test — `C:` has only one
/// character before the colon, a URL scheme has at least two.
fn has_scheme(s: &str) -> bool {
    let mut first = true;
    for (i, c) in s.char_indices() {
        if first {
            if !c.is_ascii_alphabetic() {
                return false;
            }
            first = false;
            continue;
        }
        if c == ':' {
            return i > 1 && s.len() > i + 1;
        }
        if !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
            return false;
        }
    }
    false
}

/// Is there something to open right now? Used only to phrase the hover hint — a path that does not
/// exist is a normal state here (the model may be describing a file it is about to write).
pub fn exists(t: &Target) -> bool {
    match t {
        Target::Url(_) => true,
        Target::Path(p) => p.exists(),
    }
}

/// Act on a click. Returns `false` when there was nothing to do (a path that no longer exists).
///
/// Never blocks: both branches hand off to a long-lived foreign process.
pub fn open(ctx: &egui::Context, t: &Target) -> bool {
    match t {
        Target::Url(url) => {
            // The command travels out through egui's platform output and is executed by the winit
            // backend (`egui-winit`'s `links` feature, which eframe turns on).
            ctx.open_url(egui::OpenUrl::new_tab(url.clone()));
            true
        }
        Target::Path(p) => reveal(p),
    }
}

/// Show `path` in the OS file manager — the file **selected inside its folder**, a directory opened
/// as itself.
///
/// Selecting rather than opening the folder plainly is what makes "which of these 40 files did the
/// model mean" answerable; opening the file's parent on its own would also mean clicking two files
/// in the same folder yields two identical windows.
pub fn reveal(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    reveal_impl(path)
}

#[cfg(windows)]
fn reveal_impl(path: &Path) -> bool {
    use std::os::windows::process::CommandExt;

    let native = native_path(path);
    let mut cmd = std::process::Command::new("explorer");
    if path.is_dir() {
        // A bare path: `arg` quoting it (`"E:\aox probe dir"`) is exactly what Explorer expects.
        cmd.arg(&native);
    } else {
        cmd.raw_arg(select_arg(&native));
    }
    // `explorer` reports a non-zero exit status even when it opened the window, and waiting for it
    // would block the UI thread until the user closed the window. So: spawn, do not wait, do not
    // read the status. A failed spawn (no `explorer` at all) is the only observable failure.
    cmd.spawn().is_ok()
}

/// Explorer's `/select,` argument for an already-`native_path` path.
///
/// **Why this is built by hand.** The obvious version is
/// `Command::arg(format!("/select,{path}"))`, and it is wrong in a way no type checker sees: `arg`
/// quotes according to the MSVC rules, so a path containing a space turns the *whole* string into
/// `"/select,E:\aox probe dir\a.txt"`. Explorer parses its own command line and does not recognise a
/// switch that sits inside the quotes — it ignores everything and opens **Documents**. The user sees
/// "clicking a path always opens the Documents folder" while right-click → *open with default app*
/// (a different code path, `ShellExecuteW`) keeps working. Verified on 2026-10-23 against a path with
/// a space, then fixed by quoting only the path.
///
/// Interpolating into a command line is safe *here*: Windows file names cannot contain `"`, so the
/// quoted run cannot be broken out of.
#[cfg(windows)]
fn select_arg(native: &str) -> String {
    format!("/select,\"{native}\"")
}

/// The path in the form Explorer understands: backslashes, no trailing separator.
///
/// Two separate reasons, both confirmed by driving `explorer` from a probe on 2026-10-23:
///
/// - **Forward slashes are not understood at all.** `/select,E:/rustapp/Cargo.toml` opened Desktop,
///   and `explorer E:/some/dir` opened nothing — the model writes plenty of `E:/…` paths, so this is
///   not a theoretical case.
/// - **A trailing separator inside quotes** reads as an escaped quote (`"E:\dir\"`), so it is
///   dropped — except on a drive root, where `E:\` *is* the whole path.
#[cfg(windows)]
fn native_path(path: &Path) -> String {
    let mut s = path.to_string_lossy().replace('/', "\\");
    while s.len() > 3 && s.ends_with('\\') {
        s.pop();
    }
    s
}

#[cfg(target_os = "macos")]
fn reveal_impl(path: &Path) -> bool {
    let mut cmd = std::process::Command::new("open");
    if path.is_dir() {
        cmd.arg(path);
    } else {
        cmd.arg("-R").arg(path);
    }
    cmd.spawn().is_ok()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn reveal_impl(path: &Path) -> bool {
    // No portable "select this file": the best available is opening the containing directory.
    let dir = if path.is_dir() { path } else { path.parent().unwrap_or(path) };
    std::process::Command::new("xdg-open").arg(dir).spawn().is_ok()
}

/// Open `path` **with its associated program** — the deliberate, explicit half of the pair whose
/// conservative half is [`reveal`].
///
/// Only reachable from the context menu, never from a plain click: a reply full of `.exe` paths is
/// exactly where a single mis-aimed left click would run something.
pub fn open_with_default(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    open_with_default_impl(path)
}

#[cfg(windows)]
fn open_with_default_impl(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }
    let file = wide(path.as_os_str());
    let verb: Vec<u16> = wide(std::ffi::OsStr::new("open"));
    // The return value is a pseudo-HINSTANCE: anything above 32 means "handed off", 32 and below are
    // the old shell error codes. There is nothing more to report — the program that starts is not
    // ours and we never wait for it.
    let rc = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    rc as isize > 32
}

#[cfg(not(windows))]
fn open_with_default_impl(path: &Path) -> bool {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    std::process::Command::new(opener).arg(path).spawn().is_ok()
}

// ──────────────────────── Workspace-relative paths ────────────────────────

thread_local! {
    /// What a **relative** path in a reply is relative to: the conversation's workspace root.
    ///
    /// A thread-local rather than a parameter because the two consumers are leaf functions reached
    /// through five layers of painting code (`markdown::scan`, `app::step_row`), and threading a
    /// `&Path` through all of them would be changes for the sake of a lookup nobody varies.
    ///
    /// It is refreshed once per frame by `App::transcript` — the same function that draws every
    /// consumer — so it cannot go stale while the transcript is on screen. Before the first frame
    /// (and in tests) it falls back to the app root, which is what the CLI's system prompt calls the
    /// same thing.
    static ROOT: std::cell::RefCell<PathBuf> = std::cell::RefCell::new(aox::paths::app_root());

    /// `resolve_relative` results: `path -> (exists, when)`.
    ///
    /// The cache is not an optimisation, it is what keeps this affordable: `scan` runs on **every
    /// frame**, per candidate, so an uncached `exists` would stat the disk 60+ times a second for a
    /// reply that mentions a few files — and a workspace on a network share would make scrolling
    /// stutter. Two seconds is short enough that "the model just created that file" still turns
    /// clickable almost immediately.
    static CACHE: std::cell::RefCell<std::collections::HashMap<String, (bool, Instant)>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Cache lifetime for [`resolve_relative`]. See [`CACHE`].
const CACHE_TTL: f32 = 2.0;

/// Upper bound on the cache, so a long session cannot accumulate one entry per path ever mentioned.
const CACHE_MAX: usize = 512;

/// Point relative-path resolution at `root`. Called once per frame by `App::transcript`.
pub fn set_workspace_root(root: &Path) {
    ROOT.with(|r| {
        let mut r = r.borrow_mut();
        if r.as_path() != root {
            *r = root.to_path_buf();
        }
    });
}

/// The current resolution root. See [`ROOT`].
pub fn workspace_root() -> PathBuf {
    ROOT.with(|r| r.borrow().clone())
}

/// Resolve a **relative** path mentioned in a reply, if it names something that exists.
///
/// The existence check *is* the filter. A relative path in prose is indistinguishable from ordinary
/// text — `src/main.rs`, `and/or`, `TCP/IP`, `1.5/2` — so the only rule that does not spray
/// underlines across every reply is "it must actually be there". Absolute paths need no such proof
/// (a drive letter is not an accident), which is why they are classified without touching the disk.
///
/// Only inline-code spans are offered here: a model that means a file writes it in backticks, and
/// scanning prose for relative paths is where the false positives live.
pub fn resolve_relative(s: &str) -> Option<PathBuf> {
    let s = s.trim();
    // No separator at all (`Cargo.toml`, `true`, `pip`) is a value, not a place: skip the stat.
    if s.is_empty() || s.len() > 400 || !s.contains(['/', '\\']) {
        return None;
    }
    if is_absolute_path(s) {
        return None;
    }
    let joined = workspace_root().join(s);
    let key = joined.to_string_lossy().to_string();
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        let now = Instant::now();
        if let Some((yes, at)) = c.get(&key) {
            if now.duration_since(*at).as_secs_f32() < CACHE_TTL {
                return yes.then(|| joined.clone());
            }
        }
        if c.len() >= CACHE_MAX {
            c.clear();
        }
        let yes = joined.exists();
        c.insert(key, (yes, now));
        yes.then(|| joined.clone())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drive_letter_is_a_path_not_a_url_scheme() {
        // The one classification bug that would be dangerous: `C:\x` must never reach the browser.
        assert_eq!(classify(r"C:\Windows"), Some(Target::Path(PathBuf::from(r"C:\Windows"))));
        assert_eq!(classify("E:/rustapp/dist"), Some(Target::Path(PathBuf::from("E:/rustapp/dist"))));
        assert!(matches!(classify("https://example.com"), Some(Target::Url(_))));
    }

    #[test]
    fn a_markdown_link_may_carry_any_scheme() {
        // `[text](href)`: whatever the model wrote. A mailto must survive, prose must not.
        assert_eq!(classify("mailto:a@b.c"), Some(Target::Url("mailto:a@b.c".into())));
        assert_eq!(classify("src/main.rs"), None);
        assert_eq!(classify("aox.exe"), None);
        assert_eq!(classify(""), None);
    }

    #[test]
    fn a_missing_path_is_refused_before_anything_is_spawned() {
        // The whole point of the guard: no `explorer` window for a path that is not there. Kept as a
        // test because the failure mode (a stray window, or worse) only shows up on a live desktop.
        let missing = std::env::temp_dir().join("aox-definitely-not-here-8f3a2/");
        assert!(!missing.exists());
        assert!(!reveal(&missing));
        assert!(!open_with_default(&missing));
    }

    #[test]
    fn a_relative_path_is_a_target_only_when_it_exists() {
        // The existence check is the whole filter for relative paths — without it every `and/or`,
        // `TCP/IP` and `src/gone.rs` in a reply would be underlined. The thread-local root makes this
        // test independent of any other test in the crate (each test thread gets its own).
        let dir = std::env::temp_dir().join("aox-rel-root-9f2");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("main.rs"), "fn main() {}").unwrap();
        set_workspace_root(&dir);

        let got = resolve_relative("src/main.rs").expect("the file is there");
        assert_eq!(got.file_name().unwrap(), "main.rs");
        assert!(got.parent().unwrap().ends_with("src"));

        assert_eq!(resolve_relative("src/gone.rs"), None, "nothing there ⇒ plain text");
        assert_eq!(resolve_relative("Cargo.toml"), None, "no separator ⇒ never stat'ed");
        assert_eq!(resolve_relative(r"src\main.rs").map(|p| p.exists()), Some(true), "backslashes work too");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Pins the 2026-10-23 bug: `Command::arg` on the whole `/select,…` string quotes all of it when
    /// the path has a space, Explorer stops recognising the switch, and every click opens Documents.
    #[cfg(windows)]
    #[test]
    fn only_the_path_is_quoted_inside_the_select_argument() {
        assert_eq!(select_arg(r"E:\aox probe dir\a.txt"), r#"/select,"E:\aox probe dir\a.txt""#);
        assert_eq!(select_arg(r"E:\rustapp\Cargo.toml"), r#"/select,"E:\rustapp\Cargo.toml""#);
    }

    #[cfg(windows)]
    #[test]
    fn explorer_is_never_handed_a_forward_slash_path() {
        // `explorer /select,E:/rustapp/Cargo.toml` opens Desktop, not the folder — verified against a
        // live Explorer, which is why this is a test and not a comment.
        assert_eq!(native_path(Path::new("E:/rustapp/dist")), r"E:\rustapp\dist");
        assert_eq!(native_path(Path::new(r"E:\dir\")), r"E:\dir", "a dangling separator reads as an escaped quote");
        assert_eq!(native_path(Path::new("E:/")), r"E:\", "the drive root is the whole path");
        assert_eq!(native_path(Path::new(r"\\srv\share\")), r"\\srv\share");
    }
}
