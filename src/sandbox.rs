//! The plugin subprocess fence: Job Object (sandbox approach ).
//! # What it manages, and what it does not.
//! A Job Object is a Windows kernel-level "process group". All it can express is resources, lifetime, and a small...

use crate::config::SandboxConfig;

/// `CREATE_SUSPENDED` (`winbase.h`).
const CREATE_SUSPENDED: u32 = 0x0000_0004;

/// The fence's config + construction entry point. Cheap to clone; travels with
/// `Runtime` and `PluginHandle`.
#[derive(Debug, Clone)]
pub struct Sandbox {
    cfg: SandboxConfig,
}

impl Sandbox {
    pub fn new(cfg: SandboxConfig) -> Self {
        Self { cfg }
    }

    /// Whether the fence is enabled (`[sandbox] enabled`). When off, everything
    /// degrades to the behaviour before the fence existed.
    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    /// A fence for one shell call: process allowance + kill-on-close, no UI limits.
    pub fn for_shell(max_processes: u32) -> Self {
        Self {
            cfg: SandboxConfig { enabled: true, max_processes, max_memory_mb: 0, ui_limits: false },
        }
    }

    /// The additional creation flags for the subprocess: with the fence on, require "freeze first".
    pub fn create_flags(&self) -> u32 {
        if self.cfg.enabled {
            CREATE_SUSPENDED
        } else {
            0
        }
    }

    /// Take a freshly `spawn`ed child into the fence and let it continue running.
    pub fn contain(&self, child: &tokio::process::Child) -> Fence {
        if !self.cfg.enabled {
            return Fence::off(crate::i18n::t("fence is off (config.toml's [sandbox] enabled = false)"));
        }
        #[cfg(windows)]
        {
            self.contain_windows(child)
        }
        #[cfg(not(windows))]
        {
            let _ = child;
            Fence::off(crate::i18n::t("this platform has no Job Object (the fence is Windows-only for now)"))
        }
    }
}

/// One plugin process's fence.
/// Must live until the process ends: the moment it `Drop`s, the Job handle closes and `KILL_ON_JOB_CLOSE`...
#[derive(Debug)]
pub struct Fence {
    /// The Job kernel handle. Stored as `isize` rather than `HANDLE` (`*mut c_void`): this struct travels across...
    job: isize,
    /// Whether the fence is **really** fitted. When `false` the process still runs,
    /// just unrestricted.
    pub active: bool,
    /// Why it wasn't fitted. Both `/sandbox` and the startup warning read it.
    pub reason: Option<String>,
}

impl Fence {
    /// A "not fitted" fence, for when the sandbox is off, the platform is
    /// unsupported, or Job creation failed.
    pub fn off(reason: &str) -> Self {
        Self { job: 0, active: false, reason: Some(reason.to_string()) }
    }

    /// The short status used on the `/plugins` line
    pub fn status_text(&self) -> String {
        match (self.active, &self.reason) {
            (true, _) => crate::i18n::t("fence ✓").to_string(),
            (false, Some(r)) => crate::tf!("fence ✗ ({})", r),
            (false, None) => crate::i18n::t("fence ✗").to_string(),
        }
    }

    /// Kill every process in this fence, now.
    pub fn terminate(&self) {
        #[cfg(windows)]
        {
            if self.job != 0 {
                // SAFETY: `job` is a live handle owned by this struct (closed exactly once, in Drop).
                unsafe {
                    windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job as _, 1)
                };
            }
        }
    }

    /// How many processes are alive in this Job right now (including the process that holds it).
    pub fn active_processes(&self) -> Option<u32> {
        #[cfg(windows)]
        {
            if self.job == 0 {
                return None;
            }
            use windows_sys::Win32::System::JobObjects as jo;
            let mut info: jo::JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: `job` is a live handle owned by this struct; `info` is correctly sized.
            let rc = unsafe {
                jo::QueryInformationJobObject(
                    self.job as _,
                    jo::JobObjectBasicAccountingInformation,
                    &mut info as *mut _ as *mut core::ffi::c_void,
                    std::mem::size_of::<jo::JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    std::ptr::null_mut(),
                )
            };
            (rc != 0).then_some(info.ActiveProcesses)
        }
        #[cfg(not(windows))]
        {
            None
        }
    }
}

#[cfg(windows)]
impl Drop for Fence {
    fn drop(&mut self) {
        if self.job != 0 {
            // SAFETY: `job` can only come from a successful `CreateJobObjectW`, and
            // is closed exactly once, here.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.job as _) };
        }
    }
}

// Windows implementation

#[cfg(windows)]
impl Sandbox {
    fn contain_windows(&self, child: &tokio::process::Child) -> Fence {
        let Some(hproc) = child.raw_handle() else {
            return Fence::off(crate::i18n::t("the child process already exited; too late to fit the fence"));
        };
        let Some(pid) = child.id() else {
            return Fence::off(crate::i18n::t("could not get the child process pid; too late to fit the fence"));
        };

        // ① Create the Job and write the limits into it. If this step fails there's
        //    nothing more to do — but the process must still be released.
        let job = match unsafe { self.build_job() } {
            Ok(j) => j,
            Err(e) => {
                unsafe { resume_main_thread(pid) };
                return Fence::off(&crate::tf!("failed to build the fence: {}", e));
            }
        };

        // ② Put the (still frozen) process into it.
        // SAFETY: both handles are valid kernel handles (job just created, hproc from tokio's Child).
        let assigned = unsafe {
            windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(job as _, hproc as _) != 0
        };

        // Release it whether it succeeded or not.
        let resumed = unsafe { resume_main_thread(pid) };

        if !resumed {
            // Can't resume = this process is already dead. Just terminate it; don't leave a frozen
            // zombie process holding the handle.
            unsafe {
                windows_sys::Win32::System::Threading::TerminateProcess(hproc as _, 1);
            }
            unsafe { windows_sys::Win32::Foundation::CloseHandle(job as _) };
            return Fence::off(crate::i18n::t("could not resume the child process's main thread (the process was terminated)"));
        }

        if !assigned {
            let e = last_error_text();
            unsafe { windows_sys::Win32::Foundation::CloseHandle(job as _) };
            return Fence::off(&crate::tf!("this environment does not allow adding the process to a Job ({}); this plugin is unrestricted", e));
        }

        Fence { job, active: true, reason: None }
    }

    /// Create a Job and write this instance's limits into it.
    /// SAFETY: the caller guarantees the returned handle is eventually `CloseHandle`d.
    unsafe fn build_job(&self) -> Result<isize, String> {
        use windows_sys::Win32::System::JobObjects as jo;

        let job = jo::CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(crate::tf!("CreateJobObjectW failed ({})", last_error_text()));
        }

        let mut ext: jo::JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        ext.BasicLimitInformation.LimitFlags =
            jo::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | jo::JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        // `+1` is folded into `SandboxConfig::job_process_limit`: `ACTIVE_PROCESS` counts the process count of the entire...
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
            return Err(crate::tf!("failed to write the resource limits ({})", e));
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
            // Failing to set UI restrictions is not fatal (older systems may not support all the bits), so ignore the return...
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

/// Find the main thread of a process and resume it.
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

/// Get the thread id from a `THREADENTRY32`.
#[cfg(windows)]
fn th32_thread_id(te: &windows_sys::Win32::System::Diagnostics::ToolHelp::THREADENTRY32) -> u32 {
    te.th32ThreadID
}

#[cfg(windows)]
fn last_error_text() -> String {
    crate::tf!("error code {}", unsafe { windows_sys::Win32::Foundation::GetLastError() })
}

// Reporting

/// List the loaded plugins whose fence is not fitted, for the CLI and GUI to speak up about at startup.
pub async fn unfenced(rt: &crate::runtime::Runtime) -> Vec<String> {
    if !rt.sandbox.enabled() {
        return Vec::new();
    }
    // Snapshot the slots, then drop the lock: `stat` takes each slot's own lock and may await.
    let handles: Vec<std::sync::Arc<crate::plugin::PluginHandle>> =
        rt.plugins.lock().await.values().cloned().collect();
    let mut out: Vec<String> = Vec::new();
    for h in handles {
        let st = h.stat().await;
        if st.running && !st.fence_active {
            out.push(format!("{} · {}", h.name, st.fence_text));
        }
    }
    out.sort();
    out
}

/// The body of `/sandbox`: the current limits + what it cannot manage.
pub fn report(rt: &crate::runtime::Runtime) -> String {
    let c = &rt.config.sandbox;
    let platform = if cfg!(windows) { crate::i18n::t("Windows (Job Object)") } else { crate::i18n::t("non-Windows (no fence implementation on this platform)") };
    let mut out = crate::tf!("plugin process fence · {}\n", platform);
    if !c.enabled {
        out.push_str(crate::i18n::t("\n  status: off (config.toml's [sandbox] enabled = false)\n"));
        return out;
    }
    out.push_str(&crate::tf!(
        "\n  ✓ one independent Job per plugin; it may spawn {} more child processes (0 = not even one)\n  \
         ✓ the whole tree dies with the host (KILL_ON_JOB_CLOSE; a crash or being killed counts too)\n  \
         {} memory limit: {}\n  \
         {} USER object isolation (windows / clipboard / desktop / global atoms)\n",
        c.max_processes,
        if c.max_memory_mb > 0 { "✓" } else { "✗" },
        if c.max_memory_mb > 0 { format!("{} MB", c.max_memory_mb) } else { crate::i18n::t("unset (off by default: exceeding it kills the process outright, see the config comments)").into() },
        if c.ui_limits { "✓" } else { "✗" },
    ));
    out.push_str(crate::i18n::t(
        "\n  ✗ file read/write/delete — Job Object cannot manage this; the fence cannot stop a plugin from deleting any file it has permission to delete\n  \
         ✗ network access — ditto\n  \
         ✗ out-of-bounds reads — ditto\n\n  \
         The fence is hardening, not a sandbox. To block files/network you need AppContainer or a different execution engine (WASM), \
         see the sandbox options comparison in docs.\n",
    ));
    out
}
