//! 一句话描述「这台机器是什么」—— 给设置页的「系统」选项卡用。
//!
//! # 为什么不报「Windows 11 / 10」
//!
//! 用户看到的操作系统版本号，标准做法是 `GetVersionExW`。但那个 API 从 Windows 8.1
//! 起就对**没有在清单里声明支持新版本**的进程撒谎：一律报 6.2（Windows 8），
//! 无论真实系统是 10 还是 11。拿到真值的两条路 —— 读注册表的 `CurrentVersion`、
//! 或调 ntdll 未公开导出的 `RtlGetVersion` —— 都超出了「显示一行字」该付的代价。
//!
//! 所以这里报的是**准确且免费**的三样：系统家族、体系结构、这台机器有多少核、
//! 多少内存。它们足以回答「我当时是在什么机器上跑的这个」，而不会给出一个错的版本号
//! （那比不报更糟：用户会拿着一个假数字去提 issue）。
//!
//! 体系结构特意取 `std::env::consts::ARCH`（**编译目标**）而不是运行时判定：
//! 它回答的是「这个二进制是给哪种机器编的」，而这正是排「装错了包」时唯一有用的那个数。
//! 运行时架构由 Windows 的 `GetNativeSystemInfo` 提供，但两者不一致只发生在
//! WOW64 场景，而本项目没有 32 位构建 —— 为一个不存在的场景多引一个 API 不值。

/// 主机概况，例如 `windows x86_64 · 16 核 · 31.7 GB 内存`。
///
/// 取不到的项**直接省掉**，不编造数字：内存查询失败时那一整段不出现，
/// 而不是显示 `0.0 GB`。
pub fn host_summary() -> String {
    let mut s = format!("{} {}", std::env::consts::OS, std::env::consts::ARCH);
    if let Some(cores) = cores() {
        s.push_str(&format!(" · {cores} 核"));
    }
    if let Some(gb) = total_memory_gb() {
        s.push_str(&format!(" · {gb:.1} GB 内存"));
    }
    s
}

/// 逻辑处理器数。拿不到返回 `None`（少数受限环境会拒绝）。
pub fn cores() -> Option<usize> {
    std::thread::available_parallelism().ok().map(|n| n.get())
}

/// 物理内存总量（GiB）。取不到返回 `None`。
#[cfg(windows)]
pub fn total_memory_gb() -> Option<f64> {
    use windows_sys::Win32::System::SystemInformation::GetPhysicallyInstalledSystemMemory;
    // 这个 API 的返回值单位是 **KiB**（不是字节），名字里没写，是官方文档里的一句小字。
    let mut kb: u64 = 0;
    // SAFETY: 只写一个本函数栈上的 `u64`；调用失败时函数返回 0 且不保证写过出参，
    // 所以先把它初始化成 0 再判「返回非 0 且值非 0」。
    let ok = unsafe { GetPhysicallyInstalledSystemMemory(&mut kb) };
    if ok == 0 || kb == 0 {
        return None;
    }
    Some(kb as f64 / 1024.0 / 1024.0)
}

/// 非 Windows 平台的占位：Linux/macOS 上读 `/proc/meminfo` / `sysctl` 各有各的做法，
/// 而这个项目目前只出货 Windows 版。返回 `None` 的后果只是设置页少一段。
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

    /// 这段文本是直接显示给用户的，**不能出现「None」「0.0」这种占位**。
    /// 取不到的项该整段消失，而不是变成一个看起来像数据的 0。
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
            assert!(n >= 1 && n <= 4096, "核数不合常理: {n}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn memory_is_plausible_when_available() {
        if let Some(gb) = total_memory_gb() {
            // 任何跑得动这个程序的机器都在 1 GB 以上、1 TB 以下
            assert!(gb > 1.0 && gb < 1024.0, "内存数不合常理: {gb}");
        }
    }
}
