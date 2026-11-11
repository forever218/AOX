//! Mandatory Integrity Control, used on the shell child only.
//! Four functions, each doing one thing.
//! Lower_self — drop *this* process's token to Low.

/// RID of the Low integrity level (`S-1-16-4096`)
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

// Windows

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


    /// `SE_GROUP_INTEGRITY` (`winnt.h`).
    const SE_GROUP_INTEGRITY: u32 = 0x0000_0020;

    /// The SID `S-1-16-4096`, hand-built: revision 1, one sub-authority, authority 16, RID 4096.
    const LOW_IL_SID: [u8; 12] = [1, 1, 0, 0, 0, 0, 0, 16, 0x00, 0x10, 0x00, 0x00];

    /// `TOKEN_MANDATORY_LABEL` plus its SID in one aligned allocation.
    #[repr(C)]
    struct LabelBuf {
        tml: TOKEN_MANDATORY_LABEL,
        sid: [u8; 12],
    }

    /// Lower this process to Low integrity.
    pub fn lower_self() -> Result<u32, String> {
        let token = open_own_token(TOKEN_QUERY | TOKEN_ADJUST_DEFAULT)?;

        // SAFETY: `LabelBuf` is a plain old data struct; all-zero is a valid `TOKEN_MANDATORY_LABEL`
        // (null SID, zero attributes) and the SID bytes are overwritten on the next line.
        let mut lb: LabelBuf = unsafe { std::mem::zeroed() };
        lb.sid = LOW_IL_SID;
        let sid_ptr = lb.sid.as_mut_ptr() as *mut c_void;
        lb.tml.Label.Sid = sid_ptr;
        lb.tml.Label.Attributes = SE_GROUP_INTEGRITY;

        // SAFETY: `token` is a live handle from `OpenProcessToken`; the buffer and its length describe...
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

    /// The RID and the attribute bits of the integrity SID.
    pub fn current_il_attrs() -> Result<(u32, u32), String> {
        let token = open_own_token(TOKEN_QUERY)?;
        // `[u64; 16]` rather than `[u8; 128]`: the buffer is later viewed as a `TOKEN_MANDATORY_LABEL`, which needs...
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
        // SAFETY: closed exactly once
        unsafe { CloseHandle(token) };
        if rc == 0 {
            return Err(format!("GetTokenInformation(TokenIntegrityLevel) failed: {err}"));
        }
        // The buffer is a `TOKEN_MANDATORY_LABEL` (a SID pointer + attributes) followed by the SID itself; the SID must...
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
        // A SID is [revision, count, 6-byte big-endian authority, count × u32 LE]; an integrity SID (S-1-16-x) has...
        let off = 8 + (count - 1) * 4;
        // SAFETY: `count` came from the SID itself, so `off .. off + 4` lies inside its
        // sub-authority array.
        let rid = unsafe { std::ptr::read_unaligned(sid.add(off) as *const u32) };
        Ok((rid, attrs))
    }

    /// Move `dir` and everything under it down to Low, so a Low child can write inside it.
    #[allow(dead_code)]
    /// `(OI)(CI)` makes the label inheritable (new files created later by anyone inherit Low), and `/T` walks...
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

// `unused_imports` silenced because the CLI bin compiles its own copy of every module and only uses part...
#[cfg(windows)]
#[allow(unused_imports)]
pub use imp::{current_il, current_il_attrs, label_low, label_medium, lower_self};

// Other platforms

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



