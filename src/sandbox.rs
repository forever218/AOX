//! 插件子进程的**围栏**：Job Object（沙箱方案 ①）。
//!
//! # 它管什么、不管什么
//!
//! Job Object 是 Windows 内核级的「进程组」。它能表达的只有**资源、生命周期，
//! 以及一小撮 UI 能力** —— **文件与网络它是管不了的**。所以本模块的定位是「围栏」，
//! 不是「沙箱」。把这条说在文件头一行，是为了避免后来的人误以为它已经够了。
//!
//! | 限制位 | 关掉的失败模式 |
//! |--------|----------------|
//! | `JOB_OBJECT_LIMIT_ACTIVE_PROCESS`（默认 **8 个子进程**） | 进程数失控 —— 插件连同它拉起的子孙（它们自动继承这个 Job，跑不掉）最多只有那几个。fork bomb 靠并发翻倍，封顶就起不来。`0` 可把这条彻底锁死：插件一个子进程都开不了 |
//! | `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` | 孤儿进程：宿主一退（哪怕是崩溃、被 kill），插件连同它的子孙立刻消失 |
//! | `JOB_OBJECT_LIMIT_PROCESS_MEMORY`（默认**关**） | 内存失控。会**直接杀进程**，所以默认不开，理由见 `SandboxConfig::max_memory_mb` |
//! | `JOB_OBJECT_UILIMIT_*` | 插件伸手碰宿主的窗口 / 剪贴板 / 桌面 / 全局原子 |
//!
//! ⚠️ 第一行以前是 `= 1`（「含它自己」⇒ 插件不能再开子进程），那是**过头**的：
//! 靠 `subprocess` 干活的插件（调 curl / git / 解释器）在默认配置下全废，报
//! `WinError 1816 配额不足`。现在默认放行 8 个子进程，防护靠「并发封顶」而不是
//! 「一个都不许」；想回到最严就写 `max_processes = 0`。
//!
//! # 为什么是「每个插件一个 Job」
//!
//! `ACTIVE_PROCESS` 是**整个 Job** 的额度，不是每个进程的。如果所有插件共用一个 Job，
//! 这个数字就变成「全局预算」——一个插件把额度吃光，别的插件连启动都启动不了，
//! 而且报错会指向别处，极难查。一个插件一个 Job，额度才真的是「这个插件最多开几个进程」。
//!
//! # 为什么要 `CREATE_SUSPENDED`
//!
//! 把进程塞进 Job 有两条路：出生前塞，或者出生后塞。出生后塞有一个窗口期 ——
//! `spawn()` 返回到我们调 `AssignProcessToJobObject` 之间，子进程已经在跑它自己的
//! 第一行代码了。对 python / node 来说这个窗口毫无实际意义（解释器初始化要几十毫秒），
//! 但 `CREATE_SUSPENDED` 让这个问题**不存在**：进程冻在原地，我们塞好了再放。
//!
//! 代价是要自己找回主线程句柄（`std` 与 `tokio` 都不给），所以要走一次 ToolHelp
//! 快照 + `OpenThread`。三十行，换掉一个竞态窗口 —— 值。
//!
//! # 为什么是「尽力而为」
//!
//! 宿主自己可能已经在一个 Job 里（被 IDE、终端、计划任务、或某个安装器启动）。
//! Windows 8+ 允许嵌套 Job，但**外层 Job 的策略可能不允许**。那种情况下
//! `AssignProcessToJobObject` 会失败 —— 这时**不能连插件一起打死**：照常放它跑，
//! 把失败原因如实记进 [`Fence`]，由 `/sandbox` 与启动警告告诉用户。
//! 静默降级不行，静默失败更不行；但「因为套不上围栏就不让插件跑」也不行 ——
//! 围栏是加固，不是前置条件。
//!
//! # 非 Windows
//!
//! 整个模块退化成 no-op，并且**不假装成功**：`Fence::active` 为 `false`、
//! `reason` 写明「本平台没有 Job Object」。Linux 上的正确做法是
//! `pre_exec` + namespace / rlimit，那是另一件事，不该在这里蒙混过去。

use crate::config::SandboxConfig;

/// `CREATE_SUSPENDED`（`winbase.h`）。手写常量而不是从 `windows-sys` 取，
/// 是为了让本文件在非 Windows 上也能原样编译 —— 那些路径在别的平台根本不存在。
const CREATE_SUSPENDED: u32 = 0x0000_0004;

/// 围栏的配置 + 构造入口。廉价克隆，跟着 `Runtime` 与 `PluginHandle` 走。
#[derive(Debug, Clone)]
pub struct Sandbox {
    cfg: SandboxConfig,
}

impl Sandbox {
    pub fn new(cfg: SandboxConfig) -> Self {
        Self { cfg }
    }

    /// 围栏是否启用（`[sandbox] enabled`）。关掉时一切退化成围栏之前的行为。
    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    /// 子进程的**附加**创建标志：围栏开着就要求「先冻住」。
    ///
    /// 返回值与 `CREATE_NO_WINDOW` 是或的关系，由调用点合并（见 `plugin::ProcessFlags`）。
    pub fn create_flags(&self) -> u32 {
        if self.cfg.enabled {
            CREATE_SUSPENDED
        } else {
            0
        }
    }

    /// 把刚 `spawn` 出来的子进程收进围栏，并放它继续跑。
    ///
    /// **无论成功失败，进程都会被放行** —— 这里没有「围不上就报错」的分支。
    ///
    /// ⚠️ 围栏关掉时必须**连 Job 都不建**（不只是不加 `CREATE_SUSPENDED`）。
    /// 这个坑是 selftest 的 A/B 对照组抓出来的：最初这里只关了「先冻住」那一半，
    /// 于是「关掉围栏」的对照实验仍然拿到 `WinError 1816`（配额不足）——
    /// 插件照样开不了子进程。也就是说 `enabled = false` 当时是**假的**。
    pub fn contain(&self, child: &tokio::process::Child) -> Fence {
        if !self.cfg.enabled {
            return Fence::off("围栏已关闭（config.toml 的 [sandbox] enabled = false）");
        }
        #[cfg(windows)]
        {
            self.contain_windows(child)
        }
        #[cfg(not(windows))]
        {
            let _ = child;
            Fence::off("本平台没有 Job Object（围栏目前只有 Windows 实现）")
        }
    }
}

/// 一个插件进程的围栏。
///
/// **必须活到进程结束**：它一 `Drop`，Job 句柄关闭，`KILL_ON_JOB_CLOSE` 立刻把这个
/// 插件连同它的子孙一起带走。所以它被放在 [`crate::plugin::PluginHandle`] 里，
/// 生命周期与进程本身绑定 —— 这也顺带成了「插件停止」的第二道保险。
#[derive(Debug)]
pub struct Fence {
    /// Job 内核句柄。存成 `isize` 而不是 `HANDLE`（`*mut c_void`）：
    /// 本结构要跟着 `PluginHandle` 跨线程走，而指针不是 `Send`、整数是。
    /// 句柄本身是内核对象、与线程无关，所以这么存名正言顺，也省掉一个 `unsafe impl`。
    /// `0` = 没有 Job（沙箱关闭 / 平台不支持 / 建 Job 失败）。
    job: isize,
    /// 围栏**真的**套上了吗。`false` 时进程照跑，只是不受限。
    pub active: bool,
    /// 没套上的原因。`/sandbox` 与启动警告都读它。
    pub reason: Option<String>,
}

impl Fence {
    /// 一个「没套上」的围栏，用于沙箱被关掉、平台不支持、或建 Job 失败。
    pub fn off(reason: &str) -> Self {
        Self { job: 0, active: false, reason: Some(reason.to_string()) }
    }

    /// `/plugins` 那一行里用的短状态。
    pub fn status_text(&self) -> String {
        match (self.active, &self.reason) {
            (true, _) => "围栏 ✓".to_string(),
            (false, Some(r)) => format!("围栏 ✗（{r}）"),
            (false, None) => "围栏 ✗".to_string(),
        }
    }
}

#[cfg(windows)]
impl Drop for Fence {
    fn drop(&mut self) {
        if self.job != 0 {
            // SAFETY: `job` 只可能来自 `CreateJobObjectW` 的成功返回，且只在这里关一次。
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.job as _) };
        }
    }
}

// ─────────────────────────── Windows 实现 ───────────────────────────

#[cfg(windows)]
impl Sandbox {
    fn contain_windows(&self, child: &tokio::process::Child) -> Fence {
        let Some(hproc) = child.raw_handle() else {
            return Fence::off("子进程已经退出，来不及套围栏");
        };
        let Some(pid) = child.id() else {
            return Fence::off("拿不到子进程 pid，来不及套围栏");
        };

        // ① 建 Job 并把限制写进去。这一步失败就没什么可做的了 —— 但进程仍然要放行。
        let job = match unsafe { self.build_job() } {
            Ok(j) => j,
            Err(e) => {
                unsafe { resume_main_thread(pid) };
                return Fence::off(&format!("建立围栏失败：{e}"));
            }
        };

        // ② 把（还冻着的）进程塞进去。
        // SAFETY: 两个句柄都是有效的内核句柄（job 刚建、hproc 来自 tokio 的 Child）。
        let assigned = unsafe {
            windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(job as _, hproc as _) != 0
        };

        // ③ 无论成败，放行。**这一步绝不能省** —— 进程是 `CREATE_SUSPENDED` 出来的，
        //    不恢复的话它会永远冻着，表现为「插件 initialize 握手超时」，查起来毫无头绪。
        let resumed = unsafe { resume_main_thread(pid) };

        if !resumed {
            // 恢复不了 = 这个进程已经废了。干脆结束掉，别留一个冻结的僵尸进程占着句柄。
            unsafe {
                windows_sys::Win32::System::Threading::TerminateProcess(hproc as _, 1);
            }
            unsafe { windows_sys::Win32::Foundation::CloseHandle(job as _) };
            return Fence::off("无法恢复子进程主线程（已终止该进程）");
        }

        if !assigned {
            let e = last_error_text();
            unsafe { windows_sys::Win32::Foundation::CloseHandle(job as _) };
            return Fence::off(&format!("这个环境不允许把进程加入 Job（{e}），该插件未受限"));
        }

        Fence { job, active: true, reason: None }
    }

    /// 建一个 Job 并写入本实例的限制。
    /// SAFETY: 调用方保证返回的句柄最终被 `CloseHandle`。
    unsafe fn build_job(&self) -> Result<isize, String> {
        use windows_sys::Win32::System::JobObjects as jo;

        let job = jo::CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(format!("CreateJobObjectW 失败（{}）", last_error_text()));
        }

        let mut ext: jo::JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        ext.BasicLimitInformation.LimitFlags =
            jo::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | jo::JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        // `+1` 收在 `SandboxConfig::job_process_limit` 里：`ACTIVE_PROCESS` 数的是
        // **整个 Job** 的进程数，插件自己就占一个名额。以前这里直接把配置值当总数写进去，
        // 于是默认值 1 的意思变成「Job 里只准有 1 个进程」= 插件自己刚好占满 ——
        // 想调 curl / git / 解释器的插件一律 `WinError 1816`（配额不足），
        // 表现是「插件莫名其妙干不了活」。现在配置里那个数字才真的是「能再开几个子进程」。
        ext.BasicLimitInformation.ActiveProcessLimit = self.cfg.job_process_limit();
        if self.cfg.max_memory_mb > 0 {
            ext.BasicLimitInformation.LimitFlags |= jo::JOB_OBJECT_LIMIT_PROCESS_MEMORY;
            ext.ProcessMemoryLimit = self.cfg.max_memory_mb * 1024 * 1024;
        }

        let rc = jo::SetInformationJobObject(
            job,
            jo::JobObjectExtendedLimitInformation,
            &ext as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<jo::JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if rc == 0 {
            let e = last_error_text();
            windows_sys::Win32::Foundation::CloseHandle(job);
            return Err(format!("写入资源限制失败（{e}）"));
        }

        if self.cfg.ui_limits {
            let ui = jo::JOBOBJECT_BASIC_UI_RESTRICTIONS {
                UIRestrictionsClass: jo::JOB_OBJECT_UILIMIT_HANDLES
                    | jo::JOB_OBJECT_UILIMIT_READCLIPBOARD
                    | jo::JOB_OBJECT_UILIMIT_WRITECLIPBOARD
                    | jo::JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
                    | jo::JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
                    | jo::JOB_OBJECT_UILIMIT_GLOBALATOMS
                    | jo::JOB_OBJECT_UILIMIT_DESKTOP
                    | jo::JOB_OBJECT_UILIMIT_EXITWINDOWS,
            };
            // UI 限制设不上**不致命**（老系统上未必支持全部位），所以忽略返回值。
            // 但资源限制那一条必须成功 —— 它在上面已经 `?` 过了。
            let _ = jo::SetInformationJobObject(
                job,
                jo::JobObjectBasicUIRestrictions,
                &ui as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<jo::JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
            );
        }

        Ok(job as isize)
    }
}

/// 找回某进程的主线程并恢复它。返回是否成功。
///
/// `CREATE_SUSPENDED` 的进程**只有这一个线程**（还没来得及创建别的），所以
/// 「快照里第一个属于该 pid 的线程」就是主线程 —— 不需要更聪明的判据。
///
/// SAFETY: 只读快照 + 一个自开自关的线程句柄。
#[cfg(windows)]
unsafe fn resume_main_thread(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp as th;
    use windows_sys::Win32::System::Threading as threading;

    let snap = th::CreateToolhelp32Snapshot(th::TH32CS_SNAPTHREAD, 0);
    if snap == INVALID_HANDLE_VALUE {
        return false;
    }
    let mut te: th::THREADENTRY32 = std::mem::zeroed();
    te.dwSize = std::mem::size_of::<th::THREADENTRY32>() as u32;
    let mut tid = None;
    if th::Thread32First(snap, &mut te) != 0 {
        loop {
            if te.th32OwnerProcessID == pid {
                tid = Some(th32_thread_id(&te));
                break;
            }
            if th::Thread32Next(snap, &mut te) == 0 {
                break;
            }
        }
    }
    CloseHandle(snap);

    let Some(tid) = tid else { return false };
    let h = threading::OpenThread(threading::THREAD_SUSPEND_RESUME, 0, tid);
    if h.is_null() {
        return false;
    }
    let rc = threading::ResumeThread(h);
    CloseHandle(h);
    rc != u32::MAX
}

/// 从 `THREADENTRY32` 里取线程 id。
///
/// 单独抽一行是因为 `0.59` 与更早版本的字段名不同（`th32ThreadID` / `th32ThreadId`），
/// 万一将来升版，只需要改这一处。
#[cfg(windows)]
fn th32_thread_id(te: &windows_sys::Win32::System::Diagnostics::ToolHelp::THREADENTRY32) -> u32 {
    te.th32ThreadID
}

#[cfg(windows)]
fn last_error_text() -> String {
    format!("错误码 {}", unsafe { windows_sys::Win32::Foundation::GetLastError() })
}

// ─────────────────────────── 报告 ───────────────────────────

/// 列出**没套上围栏**的已加载插件，供 CLI 与 GUI 在启动时出声。
///
/// 放在这里而不是各前端里各写一遍，是因为这段判定的措辞与条件必须一致：
/// 「以为有保护、其实没有」是围栏最坏的失败形态，两边各写一份迟早说岔。
///
/// 沙箱**本身被关掉**时返回空 —— 那是配置的明确选择（`enabled = false`），
/// 不该在启动横幅里刷屏，`/sandbox` 里会说清楚。
pub async fn unfenced(rt: &crate::runtime::Runtime) -> Vec<String> {
    if !rt.sandbox.enabled() {
        return Vec::new();
    }
    let plugins = rt.plugins.lock().await;
    let mut out: Vec<String> = plugins
        .values()
        .filter(|h| !h.fence.lock().map(|f| f.active).unwrap_or(true))
        .map(|h| format!("{} · {}", h.name, h.fence_status()))
        .collect();
    out.sort();
    out
}

/// `/sandbox` 的正文：当前限制 + **它管不了什么**。
///
/// 后半段是刻意写的。围栏最常见的误用就是「以为它挡住了文件操作」——
/// 把那句话直接印在命令输出里，比写在文档里有效得多。
pub fn report(rt: &crate::runtime::Runtime) -> String {
    let c = &rt.config.sandbox;
    let platform = if cfg!(windows) { "Windows（Job Object）" } else { "非 Windows（本平台无围栏实现）" };
    let mut out = format!("插件进程围栏 · {platform}\n");
    if !c.enabled {
        out.push_str("\n  状态：已关闭（config.toml 的 [sandbox] enabled = false）\n");
        return out;
    }
    out.push_str(&format!(
        "\n  ✓ 每个插件一个独立 Job；它能再开 {} 个子进程（0 = 一个都不许）\n  \
         ✓ 宿主退出即全灭（KILL_ON_JOB_CLOSE；崩溃、被 kill 也算）\n  \
         {} 内存上限：{}\n  \
         {} USER 对象隔离（窗口 / 剪贴板 / 桌面 / 全局原子）\n",
        c.max_processes,
        if c.max_memory_mb > 0 { "✓" } else { "✗" },
        if c.max_memory_mb > 0 { format!("{} MB", c.max_memory_mb) } else { "未设（默认关：超限是直接杀进程，见配置注释）".into() },
        if c.ui_limits { "✓" } else { "✗" },
    ));
    out.push_str(
        "\n  ✗ 文件读写与删除 —— Job Object 管不了，围栏挡不住插件删任何它有权限删的文件\n  \
         ✗ 网络访问 —— 同上\n  \
         ✗ 读取越界 —— 同上\n\n  \
         围栏是加固，不是沙箱。要挡住文件/网络，需要 AppContainer 或换执行体（WASM），\
         见 docs 里的沙箱方案对照。\n",
    );
    out
}
