//! Mandatory Integrity Control, used on the **shell child only**.
//!
//! Four functions, each doing one thing:
//!
//! - [`lower_self`] — drop *this* process's token to Low. Called by the helper entry point
//!   (see [`crate::shell`] for why a helper is needed at all), never by the host itself.
//! - [`current_il`] — read back the token's integrity RID, so "did the lowering really happen" is
//!   an observation rather than a belief.
//! - [`label_low`] / [`label_medium`] — move the workspace tree down to Low (and back). Without
//!   this a Low process cannot write *inside* the workspace either: mandatory policy blocks writes
//!   to anything labelled **higher** than the caller, and ordinary user files are Medium.
//! - [`il_name`] — turn a RID into a word, for reports.
//!
//! # What is deliberately *not* here
//!
//! No attempt to *read* an object's label back. Reading a SACL needs `SE_SECURITY_PRIVILEGE`, so
//! an unprivileged host cannot ask "is this directory already Low?" — measured on 2026-10-20:
//! `Get-Acl -Audit` fails with `PrivilegeNotHeld`, and `icacls <path>` does not print the
//! `Mandatory Label` line either. The callers therefore treat the **exit code of the labelling
//! command** as the record, and remember what they did rather than re-deriving it.
//!
//! # Non-Windows
//!
//! Everything degrades to `Err` with a sentence saying so — the same rule as `crate::sandbox`:
//! never pretend a boundary exists. The Linux equivalent (namespaces + bind mounts) is a different
//! piece of work and is not faked here.

/// RID of the Low integrity level (`S-1-16-4096`).
pub const LOW_IL_RID: u32 = 0x1000;
/// RID of the Medium integrity level (`S-1-16-8192`) — what an ordinary user process and an
/// ordinary user file carry.
pub const MEDIUM_IL_RID: u32 = 0x2000;

/// The word for an integrity RID, for humans (reports, warnings, error text).
pub fn il_name(rid: u32) -> &'static str {
    match rid {
        LOW_IL_RID => "Low",
        MEDIUM_IL_RID => "Medium",
        0x3000 => "High",
        0x4000 => "System",
        0x5000 => "Protected",
        _ => "unknown",
    }
}

// ─────────────────────────────── Windows ───────────────────────────────

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::path::Path;
    use std::process::Command;

    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::{
        GetTokenInformation, SetTokenInformation, TokenIntegrityLevel, TOKEN_ADJUST_DEFAULT,
        TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};


    /// `SE_GROUP_INTEGRITY` (`winnt.h`). Written out because `windows-sys` 0.59 does not export it
    /// from `Win32::Security`; the value is the documented "this SID is the mandatory label" bit,
    /// and a wrong one makes `SetTokenInformation` fail outright rather than silently pass.
    const SE_GROUP_INTEGRITY: u32 = 0x0000_0020;

    /// The SID `S-1-16-4096`, hand-built: revision 1, one sub-authority, authority 16, RID 4096.
    ///
    /// Written out as bytes instead of going through `ConvertStringSidToSidW` so this file needs
    /// one `windows-sys` feature less, and so the layout stays visible: a SID is
    /// `[revision, sub-authority count, 6-byte big-endian authority, sub-authorities as u32 LE]`.
    const LOW_IL_SID: [u8; 12] = [1, 1, 0, 0, 0, 0, 0, 16, 0x00, 0x10, 0x00, 0x00];

    /// `TOKEN_MANDATORY_LABEL` plus its SID **in one aligned allocation**.
    ///
    /// The SID has to sit somewhere the kernel can read for the duration of the call; keeping both
    /// in one `#[repr(C)]` struct makes that structural rather than a comment. `Vec<u8> + offset`
    /// would not do: `Vec<u8>` is only 1-byte aligned, and this struct needs 8.
    #[repr(C)]
    struct LabelBuf {
        tml: TOKEN_MANDATORY_LABEL,
        sid: [u8; 12],
    }

    /// Lower **this process** to Low integrity. Returns the integrity RID the token now carries.
    ///
    /// Lowering needs no privilege (`SeRelabelPrivilege` is only required to go *up*), just
    /// `TOKEN_ADJUST_DEFAULT` on your own token — which every process has for its own token.
    ///
    /// ⚠️ Irreversible in-process: after this the whole process and everything it spawns is Low.
    /// That is the point, and the reason it only ever runs inside the helper.
    pub fn lower_self() -> Result<u32, String> {
        let token = open_own_token(TOKEN_QUERY | TOKEN_ADJUST_DEFAULT)?;

        // SAFETY: `LabelBuf` is a plain old data struct; all-zero is a valid `TOKEN_MANDATORY_LABEL`
        // (null SID, zero attributes) and the SID bytes are overwritten on the next line.
        let mut lb: LabelBuf = unsafe { std::mem::zeroed() };
        lb.sid = LOW_IL_SID;
        let sid_ptr = lb.sid.as_mut_ptr() as *mut c_void;
        lb.tml.Label.Sid = sid_ptr;
        lb.tml.Label.Attributes = SE_GROUP_INTEGRITY;

        // SAFETY: `token` is a live handle from `OpenProcessToken`; the buffer and its length
        // describe the same allocation; `TokenIntegrityLevel` is the documented class for this
        // structure.
        let rc = unsafe {
            SetTokenInformation(
                token,
                TokenIntegrityLevel,
                &lb as *const LabelBuf as *const c_void,
                std::mem::size_of::<LabelBuf>() as u32,
            )
        };
        let err = last_error();
        // SAFETY: closed exactly once, and the token handle is not used afterwards.
        unsafe { CloseHandle(token) };
        if rc == 0 {
            return Err(format!("SetTokenInformation(TokenIntegrityLevel) failed: {err}"));
        }
        current_il()
    }

    /// The integrity RID this process's token currently carries.
    pub fn current_il() -> Result<u32, String> {
        current_il_attrs().map(|(rid, _)| rid)
    }

    /// The RID **and the attribute bits** of the integrity SID.
    ///
    /// The attributes matter for diagnosis: `SE_GROUP_INTEGRITY` (0x20) is what makes the kernel treat
    /// the SID as "this token's integrity level". A SID that is present but carries 0 is, as far as
    /// access checks are concerned, a level of *Untrusted* — while still reading back as 0x1000 to
    /// anyone who only looks at the RID.
    pub fn current_il_attrs() -> Result<(u32, u32), String> {
        let token = open_own_token(TOKEN_QUERY)?;
        // `[u64; 16]` rather than `[u8; 128]`: the buffer is later viewed as a
        // `TOKEN_MANDATORY_LABEL`, which needs 8-byte alignment. A `[u8; …]` only guarantees 1,
        // and dereferencing a misaligned pointer is undefined behaviour (it aborted the spike).
        let mut buf = [0u64; 16];
        let mut needed: u32 = 0;
        // SAFETY: `buf` is a valid writable region of the declared length, 8-byte aligned.
        let rc = unsafe {
            GetTokenInformation(
                token,
                TokenIntegrityLevel,
                buf.as_mut_ptr() as *mut c_void,
                std::mem::size_of_val(&buf) as u32,
                &mut needed,
            )
        };
        let err = last_error();
        // SAFETY: closed exactly once.
        unsafe { CloseHandle(token) };
        if rc == 0 {
            return Err(format!("GetTokenInformation(TokenIntegrityLevel) failed: {err}"));
        }
        // The buffer is a `TOKEN_MANDATORY_LABEL` (a SID pointer + attributes) followed by the SID
        // itself; the SID must be reached **through the pointer**, not at a fixed offset — reading
        // the buffer from byte 0 would be reading the pointer's own bytes (measured: that bug
        // reported "the SID does not fit the buffer").
        // SAFETY: the call above succeeded, so the buffer holds a TOKEN_MANDATORY_LABEL whose
        // `Label.Sid` points at a SID inside the same allocation.
        let (sid, count, attrs) = unsafe {
            let tml = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
            let sid = tml.Label.Sid as *const u8;
            if sid.is_null() {
                return Err("the token's integrity label carries a null SID".to_string());
            }
            (sid, *sid.add(1) as usize, tml.Label.Attributes)
        };
        if count == 0 {
            return Err("the token's integrity SID carries no sub-authority".to_string());
        }
        // A SID is [revision, count, 6-byte big-endian authority, count × u32 LE]; an integrity SID
        // (S-1-16-x) has exactly one sub-authority, so the RID starts at offset 8. Read the **last**
        // one anyway, so a longer SID cannot silently answer with the authority-relative part.
        let off = 8 + (count - 1) * 4;
        // SAFETY: `count` came from the SID itself, so `off .. off + 4` lies inside its
        // sub-authority array.
        let rid = unsafe { std::ptr::read_unaligned(sid.add(off) as *const u32) };
        Ok((rid, attrs))
    }

    /// Move `dir` **and everything under it** down to Low, so a Low child can write inside it.
    ///
    /// Kept although the shell no longer uses it (the boundary target is *Untrusted* now): the S0 probe
    /// example drives it, and it is the shortest way to reproduce the "a Low child cannot write a Low
    /// directory" measurement. `allow(dead_code)` because the CLI bin compiles its own copy of these
    /// modules and never calls it — the bin/lib split noted in TRAPS.md, not a dead end.
    #[allow(dead_code)]
    ///
    /// `(OI)(CI)` makes the label inheritable (new files created later by anyone inherit Low), and
    /// `/T` walks the existing tree — without `/T` only the directory itself would change and every
    /// pre-existing file in the project would stay Medium, i.e. unwritable by the shell.
    ///
    /// Returns `Ok` only when `icacls` exits 0. Nothing is read back (see the module docs: a SACL
    /// cannot be read unprivileged), so the caller has to treat this as "I asked and it worked".
    pub fn label_low(dir: &Path) -> Result<(), String> {
        label(dir, "L")
    }

    /// Undo [`label_low`]: bring the tree back to Medium. Also the `/shell unlabel` action.
    #[allow(dead_code)]
    pub fn label_medium(dir: &Path) -> Result<(), String> {
        label(dir, "M")
    }

    fn label(dir: &Path, level: &str) -> Result<(), String> {
        use std::os::windows::process::CommandExt;
        // `CREATE_NO_WINDOW`: this runs from a GUI build too, where a console window flashing up
        // for a labelling pass would be pure noise.
        let out = Command::new("icacls")
            .arg(dir)
            .arg("/setintegritylevel")
            .arg(format!("(OI)(CI){level}"))
            .arg("/T")
            .arg("/C")
            .creation_flags(0x0800_0000)
            .output()
            .map_err(|e| format!("could not run icacls: {e}"))?;
        if out.status.success() {
            return Ok(());
        }
        // The message is in the console code page (GBK on this machine) and only useful to a
        // human reading a log, so decode leniently rather than pretending to understand it.
        Err(format!(
            "icacls /setintegritylevel {level} failed ({}): {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).trim()
        ))
    }

    fn open_own_token(access: u32) -> Result<*mut c_void, String> {
        let mut token: *mut c_void = std::ptr::null_mut();
        // SAFETY: `GetCurrentProcess` returns a pseudo-handle (always valid, never closed);
        // `token` is a valid out-parameter.
        let rc = unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut token) };
        if rc == 0 {
            return Err(format!("OpenProcessToken failed: {}", last_error()));
        }
        Ok(token)
    }

    fn last_error() -> String {
        format!("error code {}", unsafe { windows_sys::Win32::Foundation::GetLastError() })
    }
}

// `unused_imports` silenced because the CLI bin compiles its own copy of every module and only uses
// part of this surface (`current_il` is read by the probe example, `label_medium` by `/shell
// unlabel`) — the same bin/lib split noted in TRAPS.md, not a sign the functions are dead.
#[cfg(windows)]
#[allow(unused_imports)]
pub use imp::{current_il, current_il_attrs, label_low, label_medium, lower_self};

// ─────────────────────────────── Other platforms ───────────────────────────────

#[cfg(not(windows))]
pub fn lower_self() -> Result<u32, String> {
    Err("Mandatory Integrity Control is a Windows mechanism; this platform has no equivalent here"
        .to_string())
}

#[cfg(not(windows))]
pub fn current_il() -> Result<u32, String> {
    Err("Mandatory Integrity Control is a Windows mechanism".to_string())
}

#[cfg(not(windows))]
pub fn label_low(_dir: &std::path::Path) -> Result<(), String> {
    Err("integrity labels are a Windows mechanism; nothing was labelled".to_string())
}

#[cfg(not(windows))]
pub fn label_medium(_dir: &std::path::Path) -> Result<(), String> {
    Err("integrity labels are a Windows mechanism; nothing was labelled".to_string())
}



