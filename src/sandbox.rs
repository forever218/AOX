//! The plugin subprocess **fence**: Job Object (sandbox approach ①).
//!
//! # What it manages, and what it does not
//!
//! A Job Object is a Windows kernel-level "process group". All it can express is
//! **resources, lifetime, and a small handful of UI capabilities** — **it cannot
//! manage files or the network**. So this module is positioned as a "fence", not a
//! "sandbox". Saying this in the first lines of the file is to keep later readers
//! from assuming it is already enough.
//!
//! | Limit bit | Failure mode it shuts off |
//! |--------|----------------|
//! | `JOB_OBJECT_LIMIT_ACTIVE_PROCESS` (**8 subprocesses** by default) | Runaway process count — the plugin plus the descendants it spawns (they inherit this Job automatically and can't escape) can total no more than that. A fork bomb doubles via concurrency; capping it makes it fail to start. `0` locks this down completely: the plugin can't spawn a single child process |
//! | `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` | Orphan processes: the moment the host exits (even by crash or being killed), the plugin and its descendants vanish at once |
//! | `JOB_OBJECT_LIMIT_PROCESS_MEMORY` (**off** by default) | Runaway memory. It **terminates the process outright**, so it is off by default; see `SandboxConfig::max_memory_mb` for the reason |
//! | `JOB_OBJECT_UILIMIT_*` | The plugin reaching for the host's windows / clipboard / desktop / global atoms |
//!
//! ⚠️ The first line used to be `= 1` ("including itself" ⇒ the plugin can't spawn
//! another process), which was **too much**: plugins that do their work via
//! `subprocess` (calling curl / git / an interpreter) all broke under the default
//! config, reporting `WinError 1816 insufficient quota`. Now 8 subprocesses are
//! allowed by default, and protection comes from "capping concurrency" rather than
//! "not one allowed"; to go back to the strictest, write `max_processes = 0`.
//!
//! # Why "one Job per plugin"
//!
//! `ACTIVE_PROCESS` is a quota for **the entire Job**, not per process. If all
//! plugins shared one Job, that number would become a "global budget" — one plugin
//! eating the quota would leave others unable to even start, and the error would
//! point elsewhere, extremely hard to diagnose. One Job per plugin makes the quota
//! truly mean "how many processes this plugin may open at most".
//!
//! # Why `CREATE_SUSPENDED`
//!
//! There are two ways to put a process into a Job: before birth, or after. Putting
//! it in after birth has a window — between `spawn()` returning and our call to
//! `AssignProcessToJobObject`, the child is already running its own first line of
//! code. For python / node that window is practically meaningless (interpreter init
//! takes tens of milliseconds), but `CREATE_SUSPENDED` makes the problem **not
//! exist**: the process freezes in place, we put it in, then let it go.
//!
//! The cost is having to find the main thread handle ourselves (neither `std` nor
//! `tokio` provides it), so it takes a ToolHelp snapshot + `OpenThread`. Thirty
//! lines to eliminate a race window — worth it.
//!
//! # Why "best-effort"
//!
//! The host itself may already be inside a Job (started by an IDE, a terminal, a
//! scheduled task, or some installer). Windows 8+ allows nested Jobs, but **the
//! outer Job's policy may not**. In that case `AssignProcessToJobObject` fails —
//! and then we **must not kill the plugin along with it**: let it run as usual,
//! record the failure reason faithfully in [`Fence`], and let `/sandbox` and the
//! startup warning tell the user. Silent downgrade is unacceptable, silent failure
//! is worse; but "don't let the plugin run because the fence won't fit" is also
//! unacceptable — the fence is a hardening, not a precondition.
//!
//! # Non-Windows
//!
//! The whole module degrades to a no-op, and **does not pretend to succeed**:
//! `Fence::active` is `false` and `reason` states "this platform has no Job
//! Object". The right approach on Linux is `pre_exec` + namespace / rlimit, which
//! is a different matter and shouldn't be fudged here.

use crate::config::SandboxConfig;

/// `CREATE_SUSPENDED` (`winbase.h`). Hand-written as a constant rather than taken
/// from `windows-sys`, so this file compiles as-is on non-Windows too — those paths
/// don't exist on other platforms at all.
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

    /// A fence for **one shell call**: process allowance + kill-on-close, no UI limits.
    ///
    /// Deliberately independent of `[sandbox] enabled`: killing the whole tree when a command times
    /// out is a *correctness* requirement of the shell tool (a `cargo build` is a tree, not a
    /// process), not a hardening switch the user may turn off. UI limits are dropped because a
    /// command line may legitimately start a GUI program (`start`, `explorer`) and Job UI
    /// restrictions would break it in ways that look like a random failure.
    pub fn for_shell(max_processes: u32) -> Self {
        Self {
            cfg: SandboxConfig { enabled: true, max_processes, max_memory_mb: 0, ui_limits: false },
        }
    }

    /// The **additional** creation flags for the subprocess: with the fence on,
    /// require "freeze first".
    ///
    /// The return value is OR-ed with `CREATE_NO_WINDOW`, merged by the call site
    /// (see `plugin::ProcessFlags`).
    pub fn create_flags(&self) -> u32 {
        if self.cfg.enabled {
            CREATE_SUSPENDED
        } else {
            0
        }
    }

    /// Take a freshly `spawn`ed child into the fence and let it continue running.
    ///
    /// **Whether it succeeds or fails, the process is released** — there is no
    /// "error out if it won't fit" branch here.
    ///
    /// ⚠️ When the fence is off, **not even a Job may be created** (not just
    /// omitting `CREATE_SUSPENDED`). This trap was caught by selftest's A/B control
    /// group: at first only the "freeze first" half was off here, so the "fence
    /// off" control still got `WinError 1816` (insufficient quota) — plugins still
    /// couldn't spawn child processes. In other words, `enabled = false` was
    /// **fake** at the time.
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
///
/// **Must live until the process ends**: the moment it `Drop`s, the Job handle
/// closes and `KILL_ON_JOB_CLOSE` immediately takes this plugin and its
/// descendants with it. So it lives inside [`crate::plugin::PluginHandle`], with
/// its lifetime bound to the process itself — which incidentally becomes a second
/// safety catch for "stopping a plugin".
#[derive(Debug)]
pub struct Fence {
    /// The Job kernel handle. Stored as `isize` rather than `HANDLE`
    /// (`*mut c_void`): this struct travels across threads with `PluginHandle`, and
    /// pointers are not `Send` while integers are. The handle itself is a kernel
    /// object, independent of threads, so storing it this way is legitimate and
    /// saves an `unsafe impl`. `0` = no Job (sandbox off / platform unsupported /
    /// Job creation failed).
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

    /// The short status used on the `/plugins` line.
    pub fn status_text(&self) -> String {
        match (self.active, &self.reason) {
            (true, _) => crate::i18n::t("fence ✓").to_string(),
            (false, Some(r)) => crate::tf!("fence ✗ ({})", r),
            (false, None) => crate::i18n::t("fence ✗").to_string(),
        }
    }

    /// Kill **every process in this fence**, now.
    ///
    /// This is how a timed-out shell call dies. Killing the child we hold a handle to would leave
    /// its own children alive (`cargo` → several `rustc`): they are in the Job, but
    /// `KILL_ON_JOB_CLOSE` only fires when the Job handle is dropped, which happens whenever the
    /// struct happens to go out of scope — long after the tool call has returned its error.
    /// `TerminateJobObject` takes the whole tree in one call, which is what "the command timed out"
    /// has to mean.
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
    ///
    /// `None` when there is no Job or the query failed. Two callers need the distinction between
    /// "the shell helper exited" and "the tree is really empty":
    /// - a one-shot call can tell the model it left background descendants behind;
    /// - a managed background job can outlive its shell wrapper and still report `running`.
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

// ─────────────────────────── Windows implementation ───────────────────────────

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

        // ③ Release it whether it succeeded or not. **This step must never be
        //    skipped** — the process was created with `CREATE_SUSPENDED`, and
        //    without resuming it would stay frozen forever, showing up as "plugin
        //    initialize handshake timed out", with no clue where to look.
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
        // `+1` is folded into `SandboxConfig::job_process_limit`: `ACTIVE_PROCESS`
        // counts the process count of **the entire Job**, and the plugin itself
        // occupies one slot. This used to write the config value straight in as the
        // total, so a default of 1 meant "only 1 process allowed in the Job" = the
        // plugin itself fills it exactly — plugins wanting to call curl / git / an
        // interpreter all got `WinError 1816` (insufficient quota), showing up as
        // "the plugin mysteriously can't get anything done". Now that number in the
        // config truly means "how many more child processes it may open".
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
            // Failing to set UI restrictions is **not fatal** (older systems may not
            // support all the bits), so ignore the return value. But the resource
            // limit must succeed — it was already `?`-ed above.
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

/// Find the main thread of a process and resume it. Returns whether it succeeded.
///
/// A `CREATE_SUSPENDED` process has **only this one thread** (it hasn't had time
/// to create others), so "the first thread in the snapshot belonging to that pid"
/// is the main thread — no smarter criterion is needed.
///
/// SAFETY: read-only snapshot + one thread handle opened and closed locally.
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
///
/// Pulled into its own line because the field name differs between `0.59` and older versions
/// (`th32ThreadID` / `th32ThreadId`); if we upgrade the crate someday, only this spot needs to
/// change.
#[cfg(windows)]
fn th32_thread_id(te: &windows_sys::Win32::System::Diagnostics::ToolHelp::THREADENTRY32) -> u32 {
    te.th32ThreadID
}

#[cfg(windows)]
fn last_error_text() -> String {
    crate::tf!("error code {}", unsafe { windows_sys::Win32::Foundation::GetLastError() })
}

// ─────────────────────────── Reporting ───────────────────────────

/// List the loaded plugins whose fence is **not fitted**, for the CLI and GUI to
/// speak up about at startup.
///
/// Kept here instead of written once per front end, because this decision's wording
/// and conditions must agree: "thinking there's protection when there isn't" is the
/// fence's worst failure mode, and two separate copies will eventually disagree.
///
/// Returns empty when the sandbox **itself is off** — that's an explicit config
/// choice (`enabled = false`), which shouldn't spam the startup banner; `/sandbox`
/// will explain it clearly.
///
/// **Only slots with a live process are considered.** A dormant plugin has no Job because it has no
/// process, which under lazy loading is the *ordinary* state and not a problem worth raising — the
/// check is about a process that exists and is not fenced, which is the thing that would mislead.
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

/// The body of `/sandbox`: the current limits + **what it cannot manage**.
///
/// The second half is written on purpose. The fence's most common misuse is
/// "assuming it blocks file operations" — printing that sentence directly in the
/// command output is far more effective than writing it in the docs.
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
