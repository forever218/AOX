//! Moving files to the system recycle bin — what "delete" means for the host's file tools.
//! Why not `std::fs::remove_file` / `remove_dir_all`: those *erase*. A model that mis-read a path, or a user who...
//! The mechanism is the shell's own delete (`SHFileOperationW` with `FOF_ALLOWUNDO`) rather than a hand-rolled...

use std::path::Path;

/// How many files `path` accounts for — counted before the move.
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
pub fn notice(count: usize) -> String {
    if count == 1 {
        crate::i18n::t("1 file moved to the system recycle bin").to_string()
    } else {
        crate::tf!("{} files moved to the system recycle bin", count)
    }
}

/// Move `path` (a file, or a whole directory tree) to the system recycle bin.
pub async fn recycle(path: &Path) -> anyhow::Result<()> {
    // Off the async worker on purpose: over a large tree the shell takes a long while, and every other thing...
    let owned = path.to_path_buf();
    tokio::task::spawn_blocking(move || recycle_blocking(&owned)).await?
}

#[cfg(windows)]
pub(crate) fn recycle_blocking(path: &Path) -> anyhow::Result<()> {
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

    // The shell's file operations need COM up on the calling thread, and a tokio worker is not guaranteed to have...
    let hr = unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };

    let mut op: SHFILEOPSTRUCTW = unsafe { std::mem::zeroed() };
    op.wFunc = FO_DELETE;
    op.pFrom = from.as_ptr();
    // `FOF_ALLOWUNDO` is the whole point — without it this is a plain erase.
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
pub(crate) fn recycle_blocking(path: &Path) -> anyhow::Result<()> {
    // No recycle bin to reach, so this is exactly the pre-2026-10-11 behaviour.
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
