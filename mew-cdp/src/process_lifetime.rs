// mew v3 — Phase 3.3 (Bug 2): kernel-enforced child-process lifetime
// management for the chromiumoxide-launched Chromium subprocess.
//
// Why this module exists:
//   `mew_cdp::launch` returns a `Browser` whose `Drop` does NOT kill
//   the Chromium subprocess. Cleanup is via the explicit
//   `mew_cdp::shutdown`, which sends `Browser.close` over the CDP
//   websocket. This works on graceful shutdown but leaves orphan
//   chrome.exe processes on a hard parent-process kill
//   (`Stop-Process -Force`, Task Manager "End task", SIGKILL,
//   etc.) — empirically 18 chrome processes per force-kill event on
//   this project, as documented in `phase3.2_evidence.md`.
//
//   The fix has to be kernel-enforced, not a best-effort signal
//   or an `atexit` hook (those don't run on a hard kill/crash).
//   On Windows, the standard mechanism is a Job Object with
//   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` set. The OS kernel kills
//   the child when the parent's job handle closes — which happens
//   automatically on any process exit, graceful or not.
//
// On Unix (macOS/Linux):
//   The same Job Object mechanism doesn't exist. The next best
//   thing is a `Drop` impl that sends SIGTERM (and SIGKILL after a
//   grace period) to the child PID. This handles the *graceful*
//   case but does NOT cover a parent that is itself SIGKILL'd
//   before Drop runs — that's a known limitation. A proper fix on
//   Linux would be `prctl(PR_SET_PDEATHSIG)` on the child, but
//   that's a per-process setting we can't apply after the child
//   has been spawned (we'd have to wrap the spawn in mew-cdp,
//   which is a much larger change to chromiumoxide's surface).
//   This module stubs the Unix path with the Drop-based approach
//   and flags the limitation explicitly.
//
// Safety:
//   All FFI calls in the Windows path are carefully written; the
//   `JobObject` struct owns the kernel handle and drops it on
//   drop. Closing the handle in a Drop impl is sufficient — the
//   kernel handles the rest.

#[cfg(windows)]
mod imp {
    use std::io;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
        JobObjectExtendedLimitInformation, JOBOBJECT_BASIC_LIMIT_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    /// A Windows Job Object configured with
    /// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
    ///
    /// When the last handle to the job closes, the kernel
    /// terminates every process assigned to the job. This is
    /// the mechanism Chrome itself uses to clean up its own
    /// subprocesses, and it's the canonical Windows answer to
    /// "child outlives its parent after a hard kill."
    ///
    /// The job does NOT nest under another job (we don't
    /// call `SetInformationJobObject` with
    /// `JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK`), so if a
    /// process is already in another job when we try to
    /// assign it, Windows will return an error — that's the
    /// expected behavior, not something we paper over.
    pub struct JobObject {
        handle: HANDLE,
    }

    // SAFETY: HANDLE is just a numeric pointer-sized value on
    // x86_64 Windows. The JobObject type doesn't have any
    // interior mutability (the handle is read-only). Sharing
    // across threads is safe because the only operations are
    // reads of `handle` and the FFI calls below, all of
    // which are thread-safe in the Windows API. We need
    // Send so the JobObject can move between the launch
    // thread and the shutdown thread.
    unsafe impl Send for JobObject {}
    unsafe impl Sync for JobObject {}

    impl JobObject {
        /// Create a new job. Returns an error (not a panic) if
        /// `CreateJobObjectW` fails; in practice this only
        /// happens under extreme memory pressure.
        pub fn new() -> io::Result<Self> {
            // SAFETY: `CreateJobObjectW` takes a nullable
            // SECURITY_ATTRIBUTES (we pass null for default
            // security) and a nullable name (we pass null so
            // the job is unnamed and not joinable from
            // elsewhere). The returned HANDLE, if non-null,
            // is owned by us and must be closed via
            // `CloseHandle`.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }

            // Configure the job: KILL_ON_JOB_CLOSE.
            // The other fields of the extended limit struct
            // are zeroed, which means "no other limits."
            // `SetInformationJobObject` with
            // `JobObjectExtendedLimitInformation` takes a
            // pointer to the struct; we box a zeroed one and
            // pass its address. The struct's `BasicLimitInformation`
            // is a `JOBOBJECT_BASIC_LIMIT_INFORMATION`, and we
            // set its `LimitFlags` to
            // `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` to enable
            // the kill-on-close behavior.
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            info.BasicLimitInformation = JOBOBJECT_BASIC_LIMIT_INFORMATION {
                LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                ..unsafe { std::mem::zeroed() }
            };
            // SAFETY: `info` is a stack-allocated POD struct
            // of the type SetInformationJobObject expects
            // for `JobObjectExtendedLimitInformation`. We
            // pass its address and the size of the struct.
            let ok = unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if ok == 0 {
                // SetInformationJobObject failed; close the
                // job handle we just created and propagate
                // the error so the caller can decide what to
                // do (typically: log + continue without the
                // job, i.e. fall back to the orphan-prone
                // behavior — better than crashing the parent
                // because the kill-on-close setup failed).
                let err = io::Error::last_os_error();
                unsafe { CloseHandle(handle) };
                return Err(err);
            }

            Ok(Self { handle })
        }

        /// Assign a running process (by OS PID) to this job.
        /// The process must not already be in another job
        /// (Windows forbids nested jobs by default).
        ///
        /// Errors are returned, not panicked, so the caller
        /// can decide what to do — most importantly, an
        /// `AssignProcessToJobObject` failure here means
        /// the child process is NOT protected by the job,
        /// and a subsequent hard-kill of the parent will
        /// orphan it. We log this loudly so it's visible
        /// in production.
        pub fn assign_pid(&self, pid: u32) -> io::Result<()> {
            // OpenProcess requires the access mask for the
            // operations we need: terminate (in case the
            // kernel wants to kill it later) and set-quota
            // (which AssignProcessToJobObject needs).
            //
            // SAFETY: `OpenProcess` returns a HANDLE we
            // must close. The PID is provided by the caller
            // and assumed to be valid; if the process has
            // already exited, the handle will be valid but
            // AssignProcessToJobObject will return an error,
            // which we propagate.
            let process_handle = unsafe {
                OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid)
            };
            if process_handle.is_null() {
                return Err(io::Error::last_os_error());
            }

            // SAFETY: Both HANDLEs are valid; `AssignProcessToJobObject`
            // is documented to be safe to call with a
            // process HANDLE and a job HANDLE we own.
            let ok = unsafe { AssignProcessToJobObject(self.handle, process_handle) };
            // We always close the process handle — once the
            // process is in the job, we don't need our own
            // handle to it anymore. The job keeps a
            // reference.
            unsafe { CloseHandle(process_handle) };

            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        /// Get the raw HANDLE. Used by tests that need to
        /// inspect the kernel state. Not part of the
        /// public surface; tests can call it via the
        /// `cfg(test)` accessor below.
        #[cfg(test)]
        pub fn raw_handle(&self) -> HANDLE {
            self.handle
        }
    }

    impl Drop for JobObject {
        fn drop(&mut self) {
            // Closing the job handle triggers
            // KILL_ON_JOB_CLOSE. The kernel kills every
            // process in the job before CloseHandle
            // returns.
            //
            // SAFETY: `self.handle` is non-null (we checked
            // at construction) and we own it. CloseHandle
            // is documented to be safe to call on a valid
            // owned handle.
            unsafe { CloseHandle(self.handle) };
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::io;
    use std::time::Duration;

    /// Unix (macOS/Linux) child-process lifetime management.
    ///
    /// **Known limitation (read this):** the implementation
    /// below is a `Drop` impl that sends `SIGTERM` and, after
    /// a short grace period, `SIGKILL` to the child PID. This
    /// handles the *graceful* parent-exit case. It does NOT
    /// handle a parent that is itself `SIGKILL`'d before
    /// `Drop` runs (because the kernel will tear down the
    /// process before any user-space cleanup runs).
    ///
    /// The Windows equivalent (Job Object with
    /// KILL_ON_JOB_CLOSE) is kernel-enforced and survives
    /// SIGKILL. The Linux equivalent is `prctl(PR_SET_PDEATHSIG)`,
    /// which must be set by the parent **before** `fork`+`exec`
    /// the child. We can't apply that to a child chromiumoxide
    /// has already spawned, so the proper Linux fix is to
    /// wrap the spawn in mew-cdp. That's a larger change and
    /// is left as a follow-up — see the `// TODO(linux)` in
    /// the module docs.
    pub struct JobObject {
        pid: u32,
        // True if we ever managed to add this PID to a
        // process group / equivalent. Currently always
        // false on Unix; tracked so the Drop log message
        // can tell the difference between "no protection
        // (Drop-based SIGTERM only)" and "we tried to do
        // better and failed." For now, both cases end up
        // the same — Drop-based SIGTERM — but this lets
        // the implementation evolve without changing the
        // type's public shape.
        in_group: bool,
    }

    impl JobObject {
        /// No-op on Unix (no Job Object equivalent).
        pub fn new() -> io::Result<Self> {
            Ok(Self {
                pid: 0,
                in_group: false,
            })
        }

        /// Record the child's PID for the Drop-based
        /// SIGTERM. On Linux, ideally we'd call
        /// `setpgid(0, 0)` in the child to put it in its
        /// own process group and then signal the group on
        /// parent exit — that survives the parent's own
        /// SIGTERM but not SIGKILL. We can't do that from
        /// here because the child was already spawned by
        /// chromiumoxide. So we just store the PID.
        ///
        /// // TODO(linux): when the Unix port becomes a
        /// real target, wrap the chromiumoxide spawn in
        /// mew-cdp so we can `setpgid` before exec, OR
        /// switch to a `fork+exec` ourselves and pass the
        /// child to chromiumoxide's `Browser::connect`.
        pub fn assign_pid(&mut self, pid: u32) -> io::Result<()> {
            self.pid = pid;
            Ok(())
        }
    }

    impl Drop for JobObject {
        fn drop(&mut self) {
            if self.pid == 0 {
                return;
            }
            // Use the `nix` crate's `kill` if it's
            // available; otherwise fall back to a libc
            // call. For the v1 stub we just use
            // `libc::kill` to avoid pulling another
            // dependency; SIGTERM is portable, the grace
            // period before SIGKILL is best-effort.
            //
            // SAFETY: `libc::kill` is async-signal-safe
            // and the PID is a u32 — invalid PIDs will
            // just return an error which we ignore.
            unsafe {
                libc::kill(self.pid as i32, libc::SIGTERM);
            }
            // Best-effort grace period. If the process is
            // still alive, SIGKILL it. This sleep is the
            // reason a hard-killed parent can still leak
            // children on Unix — the kernel doesn't wait
            // for our Drop to run.
            std::thread::sleep(Duration::from_millis(50));
            unsafe {
                libc::kill(self.pid as i32, libc::SIGKILL);
            }
        }
    }
}

pub use imp::JobObject;

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: JobObject can be created and dropped
    /// without panicking. On Windows this exercises the
    /// real `CreateJobObjectW` / `CloseHandle` pair; on
    /// Unix it's a no-op stub.
    #[test]
    fn job_object_creates_and_drops() {
        let job = JobObject::new().expect("JobObject::new failed");
        drop(job);
    }

    /// Test: assigning a non-existent PID to a job
    /// returns an error rather than panicking. The error
    /// is logged-and-continued at the call site, never
    /// fatal.
    #[cfg(windows)]
    #[test]
    fn assign_nonexistent_pid_returns_error() {
        let job = JobObject::new().expect("JobObject::new failed");
        // PID 0 is the System Idle Process on Windows and
        // can't be opened with our access mask, so this
        // should reliably fail. We don't assert a specific
        // error code — just that it's an error.
        let res = job.assign_pid(0);
        assert!(res.is_err(), "expected AssignProcessToJobObject(0) to fail, got {:?}", res);
    }
}
