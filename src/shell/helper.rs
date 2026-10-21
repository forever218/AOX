//! The hidden entry point that actually starts the command.
//!
//! # Why this is a second process
//!
//! Windows refuses to let a Medium process *raise* an integrity level but has no objection to it
//! *lowering its own*: dropping to Low needs no privilege (only `TOKEN_ADJUST_DEFAULT` on your own
//! token), whereas going up needs `SeRelabelPrivilege`. So instead of asking the kernel to build a
//! Low child from the outside — which may need `SE_INCREASE_QUOTA` that an ordinary account does not
//! hold — the host spawns **itself** with [`FLAG`], and this file lowers the token from the inside
//! and then starts the real command. Everything it spawns inherits the lowered token, which is
//! exactly what [`crate::shell`] needs and what the S0 spike verified end to end.
//!
//! # The contract with the host
//!
//! | Channel | Meaning |
//! |---------|---------|
//! | `AOX_SHELL_SPEC` (env, JSON) | the [`HelperSpec`] — program, args, cwd, extra env, `lowil` |
//! | stdin | null (a shell command must not block on a prompt nobody can answer) |
//! | stdout / stderr | the command's own, inherited straight through |
//! | exit code | the command's own, except for the two signals below |
//!
//! The two signals ride on **stderr** with a `\u{1}` prefix and a dedicated exit code, because "the
//! command failed" and "the boundary could not be applied" must never be confused: the first is
//! reported to the model as output, the second is a `Failure` the tool turns into a policy decision
//! (`auto` retries without the kernel boundary, `kernel` refuses the call).

use serde::{Deserialize, Serialize};

/// The argv switch that turns this process into a helper.
pub const FLAG: &str = "--aox-shell-exec";
/// The env var carrying the JSON [`HelperSpec`].
pub const SPEC_ENV: &str = "AOX_SHELL_SPEC";
/// Exit code: the integrity boundary could not be applied (nothing was run).
pub const EXIT_LOWIL_FAIL: i32 = 97;
/// Exit code: the command could not be started at all.
pub const EXIT_SPAWN_FAIL: i32 = 98;
/// Prefix no ordinary program produces, so a command that happens to print "AOX-…" cannot forge a
/// signal.
const MARK: char = '\u{1}';
const LOWIL_FAIL: &str = "\u{1}AOX-LOWIL-FAIL: ";
const SPAWN_FAIL: &str = "\u{1}AOX-SPAWN-FAIL: ";

/// What the host hands over. One struct, used by both sides (serialised here, deserialised there),
/// so the two can never drift.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelperSpec {
    pub program: String,
    pub args: Vec<String>,
    /// Appended to the command line **verbatim** (no quoting). Only `cmd` uses it: its command must
    /// arrive inside one pair of quotes that `/S` strips, because Rust's MSVC-style argument quoting
    /// (`"` → `\"`) is not something `cmd` parses.
    #[serde(default)]
    pub raw_tail: Option<String>,
    pub cwd: String,
    /// Extra environment entries. A `Vec` and not a map: the order is stable and a duplicate key
    /// cannot silently win or lose depending on hashing.
    pub env: Vec<(String, String)>,
    /// Lower this process's token to Low before spawning (the kernel boundary).
    pub lowil: bool,
}

/// A signal recovered from the helper's stderr.
#[derive(Debug, Clone)]
pub enum Signal {
    LowIl(String),
    Spawn(String),
}

/// If this process was started as the helper, run the command and return the exit code.
///
/// Called first thing by both binaries (`src/main.rs`, `gui/src/main.rs`). `None` = an ordinary
/// start, so the caller carries on with its own argument parsing.
pub fn run_if_requested() -> Option<i32> {
    let mut args = std::env::args();
    let _ = args.next();
    if args.next().as_deref() != Some(FLAG) {
        return None;
    }
    Some(exec())
}

fn exec() -> i32 {
    let raw = match std::env::var(SPEC_ENV) {
        Ok(v) => v,
        Err(_) => return fail(EXIT_SPAWN_FAIL, SPAWN_FAIL, "the helper was started without a spec"),
    };
    let spec: HelperSpec = match serde_json::from_str(&raw) {
        Ok(s) => s,
        Err(e) => return fail(EXIT_SPAWN_FAIL, SPAWN_FAIL, &crate::tf!("bad helper spec: {}", e)),
    };

    if spec.lowil {
        // From here on **this process and everything it starts** is Low. There is no way back in
        // this process, which is why the lowering lives in a helper and not in the host.
        match crate::shell::lowil::lower_self() {
            Ok(rid) if rid == crate::shell::lowil::LOW_IL_RID => {}
            Ok(rid) => {
                return fail(
                    EXIT_LOWIL_FAIL,
                    LOWIL_FAIL,
                    &crate::tf!(
                        "after lowering, the token is {} instead of Low",
                        crate::shell::lowil::il_name(rid)
                    ),
                )
            }
            Err(e) => return fail(EXIT_LOWIL_FAIL, LOWIL_FAIL, &e),
        }
    }

    let mut cmd = std::process::Command::new(&spec.program);
    cmd.args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(std::process::Stdio::null())
        // Inherit, which here means "the pipes the host gave *this* process" — the command's output
        // therefore reaches the host unchanged, with no intermediate copy to decode.
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .env_remove(SPEC_ENV);
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        if let Some(tail) = &spec.raw_tail {
            cmd.raw_arg(tail);
        }
        // 0x08000000 = CREATE_NO_WINDOW: no console flash when the host is the GUI build.
        cmd.creation_flags(0x0800_0000);
    }

    match cmd.status() {
        Ok(status) => status.code().unwrap_or(120),
        Err(e) => fail(
            EXIT_SPAWN_FAIL,
            SPAWN_FAIL,
            &crate::tf!("could not start {}: {}", spec.program, e),
        ),
    }
}

fn fail(code: i32, prefix: &str, why: &str) -> i32 {
    // Written as one line so `strip_signals` can drop exactly it.
    eprintln!("{prefix}{}", why.replace(['\r', '\n'], " "));
    code
}

/// Remove signal lines from captured stderr and hand back the first one found.
///
/// A command's own stderr is passed to the model as-is; the helper's private markers must not leak
/// into it, and if one is present the caller has to act on it (see [`Signal`]).
pub fn strip_signals(text: &str) -> (String, Option<Signal>) {
    let mut found = None;
    let mut kept = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if let Some(rest) = trimmed.strip_prefix(LOWIL_FAIL) {
            found.get_or_insert_with(|| Signal::LowIl(rest.to_string()));
        } else if let Some(rest) = trimmed.strip_prefix(SPAWN_FAIL) {
            found.get_or_insert_with(|| Signal::Spawn(rest.to_string()));
        } else if !trimmed.starts_with(MARK) {
            kept.push_str(line);
        }
    }
    (kept, found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signals_are_extracted_and_removed() {
        let text = format!("real output\n{LOWIL_FAIL}SetTokenInformation failed\nmore output\n");
        let (kept, sig) = strip_signals(&text);
        assert!(!kept.contains("AOX-LOWIL-FAIL"), "the marker must not reach the model: {kept}");
        assert!(kept.contains("real output") && kept.contains("more output"));
        assert!(matches!(sig, Some(Signal::LowIl(r)) if r.contains("SetTokenInformation")));
    }

    #[test]
    fn spawn_failure_is_a_spawn_signal() {
        let (kept, sig) = strip_signals(&format!("{SPAWN_FAIL}could not start pwsh.exe"));
        assert!(kept.trim().is_empty());
        assert!(matches!(sig, Some(Signal::Spawn(_))));
    }

    #[test]
    fn ordinary_output_is_untouched() {
        let text = "error: could not compile `aox`\n";
        let (kept, sig) = strip_signals(text);
        assert_eq!(kept, text);
        assert!(sig.is_none());
    }

    /// A command that prints something *like* a marker must not be able to forge one: only the
    /// `\u{1}` prefix counts.
    #[test]
    fn a_forged_marker_without_the_prefix_is_ignored() {
        let (kept, sig) = strip_signals("AOX-LOWIL-FAIL: not really\n");
        assert_eq!(kept, "AOX-LOWIL-FAIL: not really\n");
        assert!(sig.is_none());
    }
}
