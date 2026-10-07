//! One-line description of "what machine this is" — for the Settings page's "System" tab.
//!
//! # Why we don't report "Windows 11 / 10"
//!
//! The standard way to read the OS version the user sees is `GetVersionExW`. But since Windows 8.1
//! that API has lied to any process that did **not declare support for the new version in its
//! manifest**: it always reports 6.2 (Windows 8), whether the real system is 10 or 11. The two
//! ways to get the real value — reading `CurrentVersion` from the registry, or calling ntdll's
//! undocumented `RtlGetVersion` export — both cost more than "printing one line".
//!
//! So what we report here is the **accurate and free** trio: OS family, architecture, and how many
//! cores / how much memory this machine has. That is enough to answer "which machine was I running
//! this on", without handing back a wrong version number (which is worse than reporting none: the
//! user would file an issue citing a bogus number).
//!
//! Architecture deliberately uses `std::env::consts::ARCH` (the **compile target**) rather than a
//! runtime check: it answers "which machine was this binary built for", exactly the number that
//! matters when triaging "wrong package installed". The runtime architecture comes from Windows'
//! `GetNativeSystemInfo`, but the two only diverge in WOW64 scenarios, and this project ships no
//! 32-bit build — pulling in another API for a non-existent scenario isn't worth it.

/// Host summary, e.g. `windows x86_64 · 16 cores · 31.7 GB memory`.
///
/// Items we can't read are **dropped entirely** rather than faked: if the memory query fails the
/// whole segment is absent, instead of showing `0.0 GB`.
pub fn host_summary() -> String {
    let mut s = format!("{} {}", std::env::consts::OS, std::env::consts::ARCH);
    if let Some(cores) = cores() {
        s.push_str(&crate::tf!(" · {} cores", cores));
    }
    if let Some(gb) = total_memory_gb() {
        s.push_str(&crate::tf!(" · {} GB memory", format!("{gb:.1}")));
    }
    s
}

/// Logical processor count. Returns `None` when unavailable (a few restricted environments refuse).
pub fn cores() -> Option<usize> {
    std::thread::available_parallelism().ok().map(|n| n.get())
}

/// Total physical memory (GiB). Returns `None` when unavailable.
#[cfg(windows)]
pub fn total_memory_gb() -> Option<f64> {
    use windows_sys::Win32::System::SystemInformation::GetPhysicallyInstalledSystemMemory;
    // This API's return value is in **KiB** (not bytes) — not stated in its name, just a small-print note in the docs.
    let mut kb: u64 = 0;
    // SAFETY: we only write a `u64` that lives on this function's stack; on failure the function
    // returns 0 and doesn't guarantee it wrote the out-param, so initialize it to 0 first and then
    // check "return != 0 AND value != 0".
    let ok = unsafe { GetPhysicallyInstalledSystemMemory(&mut kb) };
    if ok == 0 || kb == 0 {
        return None;
    }
    Some(kb as f64 / 1024.0 / 1024.0)
}

/// Placeholder for non-Windows platforms: reading `/proc/meminfo` / `sysctl` on Linux/macOS
/// each has its own approach, and this project currently ships Windows only. Returning `None` just
/// drops one segment from the Settings page.
#[cfg(not(windows))]
pub fn total_memory_gb() -> Option<f64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_names_the_platform_and_architecture() {
        let s = host_summary();
        assert!(s.starts_with(std::env::consts::OS), "{s}");
        assert!(s.contains(std::env::consts::ARCH), "{s}");
    }

    /// This text is shown directly to the user, so it must **not contain placeholders like "None"
    /// or "0.0"**. Items we can't read should vanish entirely, not turn into a 0 that looks like data.
    #[test]
    fn missing_pieces_are_omitted_not_zeroed() {
        let s = host_summary();
        assert!(!s.contains("0.0 GB"), "{s}");
        assert!(!s.contains("None"), "{s}");
        assert!(!s.contains("null"), "{s}");
    }

    #[test]
    fn cores_is_plausible_when_available() {
        if let Some(n) = cores() {
            assert!(n >= 1 && n <= 4096, "implausible core count: {n}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn memory_is_plausible_when_available() {
        if let Some(gb) = total_memory_gb() {
            // Any machine that can run this program has more than 1 GB and less than 1 TB
            assert!(gb > 1.0 && gb < 1024.0, "implausible memory size: {gb}");
        }
    }
}
