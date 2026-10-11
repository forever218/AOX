//! **Moving files to the system recycle bin** — what "delete" means for the host's file tools.
//!
//! Why not `std::fs::remove_file` / `remove_dir_all` (the behaviour until 2026-10-11): those
//! *erase*. A model that mis-read a path, or a user who approved a deletion one click too fast,
//! then had no way back — and the recovery hatch was already there, free, in the OS. The host's
//! own delete tool should land in the recycle bin, not step straight over it.
//!
//! The mechanism is the **shell's own delete** (`SHFileOperationW` with `FOF_ALLOWUNDO`) rather
//! than a hand-rolled move into `C:\$Recycle.Bin`: only the shell gets the per-volume bookkeeping
//! right (`$I`/`$R` pairs, the "restore to the original path" record), and only files it moved can
//! be put back through Explorer. Hand-rolling it produces entries that *look* recycled but cannot
//! be restored — worse than not claiming to.
//!
//! ⚠️ **Known limits, deliberately not papered over** (see the call site in `tool::hostfs`):
//! - A file **too large for the recycle bin**, or on a volume that has none (a network share, some
//!   removable media), is **permanently deleted**. That is the documented behaviour of
//!   `FOF_ALLOWUNDO`, not something this module can turn into an error — the shell does the
//!   deleting, and it reports success either way. Turning it into a refusal would mean setting
//!   `FOF_WANTNUKEWARNING`, which pops an **OS modal**; a dialog appearing from a background thread
//!   over whatever the user is doing was judged worse than the silence, so `FOF_SILENT` /
//!   `FOF_NOERRORUI` stay on.
//! - The recycle bin is a **Windows** concept. Other platforms fall back to plain removal (see the
//!   `#[cfg(not(windows))]` arm) — AOX ships for Windows only, that arm exists so the crate still
//!   builds elsewhere.

use std::path::Path;

/// How many files `path` accounts for — **counted before** the move.
///
/// It exists so the UI can say "N files moved to the recycle bin": the shell neither tells us how
/// much it moved nor leaves anything behind to count afterwards.
///
/// Only **files** are counted (a directory is a container, not a file). A directory holding no
/// files at all still counts as **1**, because something did move — a toast reading "0 files" would
/// look like the call failed.
pub fn count_files(path: &Path) -> usize {
    if path.is_dir() {
        let inside: usize = std::fs::read_dir(path)
            .map(|it| it.filter_map(Result::ok).map(|e| count_files(&e.path())).sum())
            .unwrap_or(0);
        inside.max(1)
    } else {
        1
    }
}

/// The one-line notice shown to the user after a move: "1 file moved…" / "N files moved…".
///
/// Singular and plural are two separate strings on purpose — "1 files" is the kind of small
/// wrongness that makes an otherwise careful notice look sloppy. Both front ends format their
/// message through here so the two cannot drift apart.
pub fn notice(count: usize) -> String {
    if count == 1 {
        crate::i18n::t("1 file moved to the system recycle bin").to_string()
    } else {
        crate::tf!("{} files moved to the system recycle bin", count)
    }
}

/// Move `path` (a file, or a whole directory tree) to the system recycle bin.
pub async fn recycle(path: &Path) -> anyhow::Result<()> {
    // Off the async worker on purpose: over a large tree the shell takes a long while, and every
    // other thing this worker could be doing (another tool call, streaming a reply) would queue
    // behind it.
    let owned = path.to_path_buf();
    tokio::task::spawn_blocking(move || recycle_blocking(&owned)).await?
}

#[cfg(windows)]
fn recycle_blocking(path: &Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::S_OK;
    use windows_sys::Win32::System::Com::{
        CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED,
    };
    use windows_sys::Win32::UI::Shell::{
        SHFileOperationW, SHFILEOPSTRUCTW, FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION,
        FOF_NOERRORUI, FOF_SILENT,
    };

    // `pFrom` is a **double-NUL-terminated** list: one NUL closes the string, a second closes the
    // list. Dropping the second is the classic way this API quietly does nothing at all.
    let mut from: Vec<u16> = path.as_os_str().encode_wide().collect();
    from.push(0);
    from.push(0);

    // The shell's file operations need COM up on the calling thread, and a tokio worker is not
    // guaranteed to have touched COM before — so initialise it here instead of assuming.
    //
    // Only `S_OK` means **we** did the initialising, and only then may we undo it. `S_FALSE`
    // (already initialised) and `RPC_E_CHANGED_MODE` (initialised in the *other* apartment mode)
    // both belong to the previous owner, and the call below works in both cases — so neither is
    // treated as fatal, and neither is ours to clean up.
    let hr = unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };

    let mut op: SHFILEOPSTRUCTW = unsafe { std::mem::zeroed() };
    op.wFunc = FO_DELETE;
    op.pFrom = from.as_ptr();
    // `FOF_ALLOWUNDO` is the whole point — without it this is a plain erase. The other three keep
    // it headless: no confirmation, no error dialog, no progress UI (a background thread popping a
    // modal over the user's work is worse than the thing it would be reporting).
    op.fFlags = (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT) as u16;

    let rc = unsafe { SHFileOperationW(&mut op) };
    let aborted = op.fAnyOperationsAborted != 0;
    if hr == S_OK {
        unsafe { CoUninitialize() };
    }

    if rc != 0 {
        anyhow::bail!("SHFileOperationW failed (0x{:08X})", rc as u32);
    }
    if aborted {
        anyhow::bail!("the shell aborted the operation");
    }
    Ok(())
}

#[cfg(not(windows))]
fn recycle_blocking(path: &Path) -> anyhow::Result<()> {
    // No recycle bin to reach, so this is exactly the pre-2026-10-11 behaviour. Kept rather than
    // made into an error so that the crate still builds and its tests still run off-Windows; the
    // module note says so out loud rather than letting it surprise someone.
    if path.is_dir() {
        std::fs::remove_dir_all(path)?;
    } else {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("aox-recycle-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_file_counts_as_one_and_an_empty_directory_counts_as_one_too() {
        let d = tmp("count");
        let f = d.join("a.txt");
        std::fs::write(&f, "x").unwrap();
        assert_eq!(count_files(&f), 1);
        // "Something moved" is the point of the count: zero would read as a failure in the toast.
        assert_eq!(count_files(&d.join("empty")), 1);
    }

    #[test]
    fn counting_walks_nested_directories_and_counts_only_files() {
        let d = tmp("count-nested");
        std::fs::create_dir_all(d.join("sub/deeper")).unwrap();
        std::fs::write(d.join("a.txt"), "x").unwrap();
        std::fs::write(d.join("sub/b.txt"), "x").unwrap();
        std::fs::write(d.join("sub/deeper/c.txt"), "x").unwrap();
        // 3 files; the two directories themselves are containers and are not counted.
        assert_eq!(count_files(&d), 3);
    }

    /// The end-to-end half: the file really does leave where it was.
    ///
    /// ⚠️ This one **puts a file in the machine's recycle bin** — that is the only way to prove the
    /// `SHFileOperationW` path (COM init, double-NUL list, flags) actually works; the other half of
    /// the claim ("and it is restorable") cannot be asserted without reading `$Recycle.Bin`, so it
    /// is left to the human. One file per run, clearly named, is the price.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_file_really_leaves_the_working_tree() {
        let d = tmp("move");
        let f = d.join("aox-recycle-self-test.txt");
        std::fs::write(&f, "recyclable").unwrap();

        recycle(&f).await.expect("the shell should have taken the file");

        assert!(!f.exists(), "the file should be gone from where it was");
        // The directory it lived in must survive: only the file was named.
        assert!(d.exists(), "recycling a file must not take its parent with it");
    }
}
