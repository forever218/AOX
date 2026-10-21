//! The built-in **shell** capability (approach ②b: a low-integrity child).
//!
//! This module tree exists for one reason: giving the model a real command line (`cmd` /
//! PowerShell / Git Bash / `git`) **without** giving it the right to write anywhere on the device.
//!
//! # Why this is not a plugin
//!
//! A shell written as a plugin could only be as disciplined as its own code — exactly the
//! argument in `hostfs`'s module docs. Here the arguments and the boundary live in the host, so
//! "the command must run inside the workspace" is not a convention the model may skip.
//!
//! # The shape of the boundary
//!
//! Reads are **not** restricted (same asymmetry as [`crate::workplace::Access`]: reading takes
//! information, writing changes the device). On Windows the "no writes outside the workspace"
//! half is enforced by the kernel through Mandatory Integrity Control:
//!
//! | Piece | Where it lives |
//! |-------|----------------|
//! | the child runs at **Low** integrity | [`lowil::lower_self`], called by our own helper process before it spawns the command |
//! | the workspace may be written by a Low process | [`lowil::label_low`] — `no-write-up` would otherwise block it too |
//! | the tree dies with the host / on timeout | a Job Object per call (`crate::sandbox::Fence`) |
//!
//! The static command scan in `crate::tool::shell` is a **guard rail**, not a boundary: it exists
//! so the model gets a readable refusal *before* a command half-runs, and so non-Windows has
//! something at all. Anything that relies on string matching is bypassable by construction.
//!
//! # Why a helper process instead of `CreateProcessAsUser`
//!
//! Creating a low-integrity child from a medium process normally means duplicating the token and
//! calling `CreateProcessAsUser`, which can fail for lack of `SE_INCREASE_QUOTA` on an ordinary
//! unprivileged account. Instead the host spawns **itself** with a hidden switch, and that helper
//! lowers *its own* token ([`lowil::lower_self`]) — lowering an integrity level needs no privilege
//! (raising one does), only `TOKEN_ADJUST_DEFAULT` on your own token. Every process the helper
//! then spawns inherits the lowered token.

pub mod bash;
pub mod helper;
pub mod lowil;
pub mod runner;

/// Turn a command's raw output bytes into text.
///
/// UTF-8 first, and that is not a guess: `git`, PowerShell 7 and most modern CLIs write UTF-8 even on
/// a Chinese Windows. `cmd` and older tools write the **ANSI code page** (CP936/GBK here), which is
/// why a strict UTF-8 decode followed by "give up and replace" would turn every Chinese error message
/// into a row of U+FFFD — the model would then be reading noise and would "fix" the wrong thing.
///
/// So: strict UTF-8, else the ANSI code page, else lossy. Decoded through the OS rather than a
/// bundled table, because the code page is a property of *this machine*, not of the build.
pub fn decode(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => ansi_to_string(bytes).unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned()),
    }
}

#[cfg(windows)]
fn ansi_to_string(bytes: &[u8]) -> Option<String> {
    use windows_sys::Win32::Globalization::MultiByteToWideChar;
    if bytes.is_empty() {
        return Some(String::new());
    }
    // 0 = CP_ACP, the system ANSI code page (936 on a Chinese install). No flags: a byte sequence
    // that is invalid in that code page should degrade to the replacement char here, not fail the
    // whole stream — losing one character beats losing the error message it sat in.
    let cp = 0u32;
    // SAFETY: the source pointer/length describe `bytes`; a null output buffer with length 0 is the
    // documented way to ask for the required size.
    let need = unsafe {
        MultiByteToWideChar(cp, 0, bytes.as_ptr(), bytes.len() as i32, std::ptr::null_mut(), 0)
    };
    if need <= 0 {
        return None;
    }
    let mut wide = vec![0u16; need as usize];
    // SAFETY: `wide` has exactly the length the call above asked for.
    let n = unsafe {
        MultiByteToWideChar(
            cp,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            wide.as_mut_ptr(),
            need,
        )
    };
    if n <= 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&wide[..n as usize]))
}

#[cfg(not(windows))]
fn ansi_to_string(_bytes: &[u8]) -> Option<String> {
    // Nothing to convert to: on other platforms the locale encoding is handled by the terminal, and
    // a lossy decode is the honest fallback.
    None
}

/// Which tool the model is told to reach for first — `bash` unless the config says `cmd`.
///
/// One reader for three call sites ([`crate::prompt`], [`crate::tool::shell::report`] and
/// [`command_line_summary`]), because "the settings row says one thing and the prompt says another" is
/// exactly the kind of drift that made `[runtimes] prefer` need a single `pick()` (see
/// `docs/ARCH-ROADMAP.md`). Anything that is not `cmd` counts as `bash` — including a typo, which
/// then behaves like the default instead of like an accidental switch.
pub fn prefers_bash(cfg: &crate::config::ShellConfig) -> bool {
    !cfg.prefer.trim().eq_ignore_ascii_case("cmd")
}

/// One line for a UI: **which command-line tool the model actually gets**, and where it came from.
///
/// Kept in the core rather than assembled in the GUI because it must agree with what `main`/`bridge`
/// register — the row is a claim about behaviour, and a front end that re-derived it could drift.
/// It is deliberately the *first* tool only: when a bash exists, `shell` is the fallback, not the
/// headline — unless `[shell] prefer = "cmd"` says the other way round.
///
/// `allow(dead_code)`: only the GUI calls it (the settings page's System tab), and the CLI bin compiles
/// its own copy of this module — the bin/lib split noted in TRAPS.md, not an unused function.
#[allow(dead_code)]
pub fn command_line_summary(cfg: &crate::config::ShellConfig, app_root: &std::path::Path) -> String {
    if !cfg.enabled {
        return crate::i18n::t("off — neither tool is registered").to_string();
    }
    let bash = bash::detect(cfg, app_root);
    if prefers_bash(cfg) {
        return match bash {
            // `flavour` distinguishes a real bash from the busybox fallback, which is the distinction
            // a reader of this row actually cares about (arrays and `[[ ]]` vs not).
            Some(b) => crate::tf!("bash · {} — {}", b.flavour, b.exe.display()),
            None => crate::i18n::t("shell · cmd / pwsh (no bash found)").to_string(),
        };
    }
    // `prefer = "cmd"`: the row names cmd first and bash as what follows it, because that is what the
    // prompt now tells the model. Nothing here is executable state — the order lives in the wording.
    match bash {
        Some(b) => crate::tf!("cmd · cmd.exe · then bash ({})", b.flavour),
        None => crate::i18n::t("cmd · cmd.exe · no bash").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::decode;

    /// The row the settings page shows must name the tool that is *first* (bash when present), and the
    /// flavour — `git-bash` and `busybox` are not interchangeable, and that word is how a reader knows.
    #[test]
    fn the_command_line_summary_names_the_first_tool() {
        let s = super::command_line_summary(
            &crate::config::ShellConfig::default(),
            &std::env::temp_dir(),
        );
        assert!(
            s.starts_with("bash") || s.starts_with("shell"),
            "unexpected summary: {s}"
        );
        // With `prefer = "cmd"` the same row has to lead with cmd: it exists to answer "which tool
        // does the model reach for first", so it may not keep reporting bash as the favourite.
        let cmd = crate::config::ShellConfig {
            prefer: "cmd".into(),
            ..Default::default()
        };
        let s = super::command_line_summary(&cmd, &std::env::temp_dir());
        assert!(s.starts_with("cmd"), "prefer=cmd must lead with cmd: {s}");
        // `off` must say so rather than silently reporting whatever is installed.
        let off = crate::config::ShellConfig { enabled: false, ..Default::default() };
        assert!(super::command_line_summary(&off, &std::env::temp_dir()).contains("off"));
    }

    /// Only the exact word `cmd` switches the order; anything else — including a typo — is bash.
    ///
    /// Deliberately strict: a mistyped value must behave like the default rather than like an
    /// accidental preference switch, which is the same rule `[runtimes] prefer` follows.
    #[test]
    fn only_cmd_flips_the_preference() {
        let with = |v: &str| crate::config::ShellConfig {
            prefer: v.into(),
            ..Default::default()
        };
        assert!(super::prefers_bash(&with("bash")));
        assert!(super::prefers_bash(&with("cnd")));
        assert!(super::prefers_bash(&with("")));
        assert!(!super::prefers_bash(&with("cmd")));
        // Case and stray spaces are forgiven: config files are written by hand.
        assert!(!super::prefers_bash(&with("  CMD ")));
    }

    #[test]
    fn utf8_survives_unchanged() {
        assert_eq!(decode("错误：找不到文件".as_bytes()), "错误：找不到文件");
        assert_eq!(decode(b""), "");
    }

    /// The exact bytes CP936 writes for "错误" — the case that motivated all of this: a `cmd`
    /// command's Chinese error message must reach the model as text, not as U+FFFD.
    #[test]
    fn ansi_codes_page_is_decoded_when_it_is_not_utf8() {
        let gbk = [0xB4u8, 0xED, 0xCE, 0xF3]; // "错误" in GBK
        assert!(std::str::from_utf8(&gbk).is_err(), "the fixture must not be valid UTF-8");
        let got = decode(&gbk);
        if cfg!(windows) {
            assert_eq!(got, "错误", "the ANSI code page must be honoured on Windows");
        } else {
            // Off Windows there is no code-page conversion to ask for; lossy is the documented floor.
            assert!(!got.is_empty());
        }
    }
}
