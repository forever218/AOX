//! One-line description of "what machine this is" — for the Settings page's "System" tab.
//! # Why we don't report "Windows 11 / 10".
//! The standard way to read the OS version the user sees is `GetVersionExW`. But since Windows 8.1 that API has...

/// Host summary, e.g. `windows x86_64 · 16 cores · 31.7 GB memory`.
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
    // SAFETY: we only write a `u64` that lives on this function's stack; on failure the function returns 0...
    let ok = unsafe { GetPhysicallyInstalledSystemMemory(&mut kb) };
    if ok == 0 || kb == 0 {
        return None;
    }
    Some(kb as f64 / 1024.0 / 1024.0)
}

/// Placeholder for non-Windows platforms: reading `/proc/meminfo` / `sysctl` on Linux/macOS each has its own...
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
