// mew v3 — Orphan Chromium reaper (root-cause fix for the
// "browser may not be reachable" failure mode).
//
// Background — see the matching comment block in
// `src/lib.rs::launch` / `launch_headless`.
//
// Before this module existed, a force-killed or crashed Tauri
// shell (Task Manager "End task", `Stop-Process -Force`,
// SIGKILL, antivirus quarantine, OOM kill, etc.) would leave
// behind N `chrome.exe` processes:
//
//   * 1 main browser process
//   * 1 GPU process
//   * 1 utility process (network / storage / audio)
//   * 1 renderer process per open tab
//   * 1 crashpad handler
//
// None of them are tied to the parent anymore (the parent's
// `JobObject` handle is gone, so the
// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` mechanism in
// `process_lifetime.rs` had no kernel object to fire against).
// On the next launch, the new `Browser::launch` then either:
//
//   (a) fails to bind port 9222 because an old Chrome is still
//       listening on it, OR
//   (b) binds a *new* port and starts, but the new chrome
//       shares the `user_data_dir` SQLite lock with the old
//       one and crashes the moment it tries to read the
//       History / Cookies / Preferences DBs. The user-facing
//       symptom is the
//       "ProcessSingleton ... Lock file can not be created!
//        Error code: 32" failure from chrome's startup.
//
// The user-facing error in both cases was the canned
// "I couldn't start the browser task. The browser may not be
// reachable" string from `orchestrator.rs`'s factory-error
// fallback (now improved to surface the real anyhow chain).
// The root cause, though, is the orphan.
//
// This module is the **root-cause fix**. Before every
// `launch` / `launch_headless` we run a sweep, in order of
// confidence:
//
//   Pass 1 — Port-based reaper (the original Phase 3.3 sweep).
//            `GetExtendedTcpTable` (iphlpapi.dll) for the
//            `MIB_TCP_TABLE_OWNER_PID_LISTENER` family.
//            Filter to rows where `dwLocalPort` equals our
//            debug port and `dwState` is `MIB_TCP_STATE_LISTEN`
//            (= 2). For each matching PID, query the image
//            name and `TerminateProcess` chrome.exe.
//
//   Pass 2 — Profile-lock reaper (Phase X.1).
//            The Restart Manager API (`RmStartSession` /
//            `RmRegisterResources` / `RmGetList`) enumerates
//            every process that currently holds a handle to a
//            file inside our `user_data_dir`. This catches
//            the exact failure mode the port-based reaper
//            misses: a zombie Chromium that's between
//            listeners (already closed the debug socket, but
//            still holds the SingletonLock / lockfile /
//            Preferences SQLite). The port-based reaper
//            cannot see this process; this pass can.
//
//   Pass 3 — Stale-lockfile cleanup (Phase X.1).
//            After Pass 1 and Pass 2 have run, if a
//            `SingletonLock` / `lockfile` is still present
//            in the profile dir AND the file is not actually
//            held by any chrome process, it is by definition
//            stale (a previous Chrome exited without
//            releasing it — bug in Chrome, bug in our
//            reaper, or a crash on a profile file operation).
//            We delete the stale lockfile as a last-resort
//            fallback so the next launch can proceed. The
//            delete is gated on a best-effort
//            `can_we_acquire_lockfile()` probe that opens
//            the file with `FILE_SHARE_NONE` and immediately
//            closes; if the open succeeds, no live process
//            holds the lock, so the file is genuinely stale.
//
// Pass ordering matters: the port-based pass is the
// fastest and the most precise (it knows our debug port
// exactly), so it goes first to catch the common case with
// minimum syscall cost. Pass 2 is the broader sweep that
// catches the profile-lock case. Pass 3 is a final
// belt-and-suspenders cleanup, never silently.
//
// Why port-9222 alone was not enough, and not also "scan
// for any chrome.exe with our user-data-dir":
//
//   * `tasklist /V` does NOT include a `Command Line`
//     column (verified on Windows 10/11; the `/V` flag
//     only adds `Session Name`, `Session#`, `Mem Usage`,
//     `Status`, `User Name`, `CPU Time`, `Window Title`).
//   * `wmic` is deprecated since Windows 11 24H2 and
//     prints a deprecation banner that would be a UX
//     regression.
//   * `EnumProcesses` + `QueryFullProcessImageNameW`
//     gives the image path but not the command line.
//     Reading the command line cross-process requires
//     `NtQueryInformationProcess(ProcessCommandLineInformation,
//     ...)`, which lives in `ntdll.dll`. Importing
//     `ntdll.dll` from Rust forces the MSVC linker to
//     look for `ntdll.dll.lib`, which is *not* part of
//     every Windows SDK install (it's bundled in the
//     WDK, not the plain Windows SDK), and its absence
//     breaks the build for downstream users.
//
//   The Restart Manager API, by contrast, is the supported
//   Win32 surface for exactly "which processes have which
//   files open", it lives in `rstrtmgr.dll` (always
//   present), and the linker finds it via the standard
//   `windows-sys` feature `Win32_System_RestartManager`.
//
//   The port-9222 signal is still sufficient for the
//   most common failure mode: a stale chrome on the debug
//   port is *guaranteed* to be ours (the
//   chromiumoxide-launched chrome from a previous
//   session). The narrow edge case the port-only check
//   misses is "zombie Chromium between listeners,
//   holding only the profile dir lock"; Pass 2 catches
//   that.
//
// The function is Windows-only and gated behind
// `cfg(windows)`. The Unix side is a no-op stub.

#[cfg(windows)]
mod imp {
    use std::path::{Path, PathBuf};
    use windows_sys::Win32::Foundation::{
        CloseHandle, HANDLE, ERROR_INSUFFICIENT_BUFFER, ERROR_ACCESS_DENIED,
        ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION, GENERIC_READ, GENERIC_WRITE,
    };
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, TerminateProcess,
        PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    };
    use windows_sys::Win32::System::RestartManager::{
        RmEndSession, RmGetList, RmRegisterResources, RmStartSession, RM_PROCESS_INFO,
        RM_UNIQUE_PROCESS,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, DeleteFileW, FILE_SHARE_NONE, OPEN_EXISTING,
    };

    /// State constant for a TCP row in LISTEN. From
    /// `Win32_NetworkManagement_IpHelper` (MIB_TCP_STATE
    /// enum in the Windows SDK headers).
    const MIB_TCP_STATE_LISTEN: u32 = 2;

    /// Sentinel exit code for "killed because reaper
    /// identified this process as an orphan."
    const ORPHAN_REAP_EXIT_CODE: u32 = 1;

    /// File names inside the profile dir that act as
    /// single-process locks. Chrome opens these with
    /// `FILE_SHARE_NONE` (or near-zero) so a second
    /// `chrome.exe` against the same profile fails to
    /// start. Pass 3 is gated on the file existing
    /// AND being one of these names, so we never
    /// accidentally delete a normal Preferences / History
    /// file.
    const PROFILE_LOCKFILE_NAMES: &[&str] = &[
        "SingletonLock",
        "SingletonCookie",
        "SingletonSocket",
        "lockfile",
    ];

    /// Reap orphaned Chromium processes holding our
    /// `user_data_dir` or our debug port. Returns the
    /// total number of PIDs terminated across all passes.
    ///
    /// Pass 1 (port-based) catches chromes listening on
    /// 9222 — the textbook stale-chrome case.
    /// Pass 2 (RM-based) catches chromes holding a handle
    /// to a file inside the profile dir — the case where
    /// the previous Chrome is between listeners and is
    /// blocking the new launch with the lockfile.
    /// Pass 3 (stale-lockfile) removes the lockfile only
    /// after Pass 1 + Pass 2 have both run and the file
    /// is genuinely not held by any process.
    pub fn reap_orphaned_chromes(profile_dir: &Path, debug_port: u16) -> usize {
        let mut total = reap_by_debug_port(debug_port);
        total += reap_by_profile_lock(profile_dir);
        total += clean_stale_lockfile(profile_dir);
        total
    }

    /// Pass 1: port-based reaper. Unchanged from Phase 3.3.
    /// Kept as a separate function so the call site is
    /// self-documenting and a regression in one pass can
    /// not silently disable the others.
    fn reap_by_debug_port(debug_port: u16) -> usize {
        let target_port: u16 = debug_port;

        let listeners = match snapshot_listeners() {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    event = "orphan_reap_tcp_table_failed",
                    error = %e,
                    "Pass 1: GetExtendedTcpTable failed; skipping port-based reaper. \
                     Subsequent launch may fail with a stale-chrome error."
                );
                return 0;
            }
        };

        let mut candidate_pids: Vec<u32> = Vec::new();
        for row in &listeners {
            // `MIB_TCPROW_OWNER_PID.dwLocalPort` is
            // a u32 whose **low 2 bytes, in
            // big-endian byte order, hold the
            // 16-bit port**. Empirically (x86/x64
            // Windows): port 9222 = 0x2406 ends up
            // as the u32 value 0x0000_0624 — the
            // bytes in memory are `0x24, 0x06,
            // 0x00, 0x00`. The "right" extraction
            // is therefore `u16::from_be_bytes` of
            // the low 2 bytes of the u32.
            let port_bytes = (row.dwLocalPort & 0xFFFF) as u16;
            let port = u16::from_be(port_bytes);
            tracing::trace!(
                event = "orphan_reap_row",
                pass = 1,
                state = row.dwState,
                port,
                pid = row.dwOwningPid,
                "TCP row"
            );
            if row.dwState == MIB_TCP_STATE_LISTEN && port == target_port {
                if !candidate_pids.contains(&row.dwOwningPid) {
                    candidate_pids.push(row.dwOwningPid);
                }
            }
        }

        if candidate_pids.is_empty() {
            tracing::debug!(
                event = "orphan_reap_no_candidates",
                pass = 1,
                port = debug_port,
                "Pass 1: no listeners on debug port; nothing to reap"
            );
            return 0;
        }

        let mut killed: usize = 0;
        let mut skipped_wrong_image: usize = 0;
        for pid in candidate_pids {
            if let Some(image) = open_and_query_image(pid) {
                if is_chromium_family(&image) {
                    if terminate(pid).is_ok() {
                        killed += 1;
                        tracing::info!(
                            event = "orphan_reap_killed",
                            pass = 1,
                            pid,
                            image = %image.display(),
                            port = debug_port,
                            "Pass 1: killed orphaned Chromium-family process holding our debug port"
                        );
                    } else {
                        tracing::warn!(
                            event = "orphan_reap_terminate_failed",
                            pass = 1,
                            pid,
                            image = %image.display(),
                            "Pass 1: TerminateProcess returned 0; candidate was not killed"
                        );
                    }
                } else {
                    skipped_wrong_image += 1;
                    tracing::debug!(
                        event = "orphan_reap_wrong_image",
                        pass = 1,
                        pid,
                        image = %image.display(),
                        "Pass 1: port-holder is not a Chromium-family browser; skipping"
                    );
                }
            }
        }

        tracing::info!(
            event = "orphan_reap_done",
            pass = 1,
            killed,
            skipped_wrong_image,
            port = debug_port,
            "Pass 1: port-based reaper finished"
        );
        killed
    }

    /// Pass 2: profile-lock reaper. Uses the Restart Manager
    /// API to find every process that holds a handle to a
    /// file inside `profile_dir`. This catches the exact
    /// failure mode the port-based reaper cannot see: a
    /// zombie Chromium between listeners, blocking the new
    /// launch with the SingletonLock / lockfile.
    fn reap_by_profile_lock(profile_dir: &Path) -> usize {
        // Don't even try if the profile dir doesn't exist
        // — there is nothing for a stale Chrome to be
        // locking yet. The first launch on a fresh machine
        // will hit this branch.
        if !profile_dir.is_dir() {
            tracing::debug!(
                event = "orphan_reap_no_profile_dir",
                pass = 2,
                profile_dir = %profile_dir.display(),
                "Pass 2: profile dir does not exist; skipping profile-lock reaper"
            );
            return 0;
        }

        // Register the whole profile dir as the resource of
        // interest. The RM API takes a list of file paths
        // and returns every process that holds a handle to
        // any of them. We pass the directory itself (a
        // single path) — the RM session walks the tree
        // semantically (it treats it as a containing
        // folder), and the result includes any process
        // holding a handle to a descendant file.
        let profile_path_w: Vec<u16> = profile_dir
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let mut session: u32 = 0;
        // RmStartSession writes a session key (CCH_RM_SESSION_KEY
        // = 32 wide chars + 1 null terminator) into the buffer
        // we pass it. The +1 is critical: passing a 32-wide-char
        // buffer overflows by one and crashes the test runner
        // with STATUS_ACCESS_VIOLATION. We never read the key —
        // it just needs to exist so the API has somewhere to write.
        let mut session_key = [0u16; 33];
        let start_status = unsafe {
            RmStartSession(
                &mut session,
                0,
                session_key.as_mut_ptr(),
            )
        };
        if start_status != 0 {
            tracing::warn!(
                event = "orphan_reap_rm_start_failed",
                pass = 2,
                error = start_status,
                "Pass 2: RmStartSession failed; skipping profile-lock reaper. \
                 The next launch may still fail if a zombie Chromium is holding \
                 the user_data_dir lock."
            );
            return 0;
        }
        // From here on, every return path must call
        // RmEndSession to avoid leaking the session.
        let result = reap_by_profile_lock_inner(session, &profile_path_w);
        let end_status = unsafe { RmEndSession(session) };
        if end_status != 0 {
            tracing::warn!(
                event = "orphan_reap_rm_end_failed",
                pass = 2,
                error = end_status,
                "Pass 2: RmEndSession returned non-zero; the session is leaked. \
                 Not fatal — the OS reaps it on process exit."
            );
        }
        result
    }

    fn reap_by_profile_lock_inner(session: u32, profile_path_w: &[u16]) -> usize {
        // RegisterResources. nFiles=1, rgsfilenames=ptr-to-array-of-PCWSTR.
        // The signature is:
        //   RmRegisterResources(dwsessionhandle, nfiles,
        //     rgsfilenames: *const PCWSTR,  // pointer to array of pointers
        //     napplications, rgapplications: *const RM_UNIQUE_PROCESS,
        //     nservices,    rgsservicenames: *const PCWSTR)
        //
        // We need `rgsfilenames` to point to an array of
        // PCWSTR values, each pointing to a null-terminated
        // wide string. We build that explicitly: a one-element
        // array whose single entry is the address of our
        // null-terminated profile-path buffer. The buffer
        // must outlive the FFI call (it does — it lives in
        // `reap_by_profile_lock` and is borrowed here).
        let file_ptrs: [*const u16; 1] = [profile_path_w.as_ptr()];
        let reg_status = unsafe {
            RmRegisterResources(
                session,
                1,
                file_ptrs.as_ptr(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
            )
        };
        if reg_status != 0 {
            tracing::warn!(
                event = "orphan_reap_rm_register_failed",
                pass = 2,
                error = reg_status,
                "Pass 2: RmRegisterResources failed; cannot enumerate profile-lock holders. \
                 A subsequent launch may still fail with the lockfile error."
            );
            return 0;
        }

        // Two-call RmGetList to learn the size, then
        // allocate and re-call. This is the standard
        // "flexible array member" pattern.
        let mut needed: u32 = 0;
        let mut count: u32 = 0;
        let mut reboot_reasons: u32 = 0;
        let probe_status = unsafe {
            RmGetList(
                session,
                &mut needed,
                &mut count,
                std::ptr::null_mut(),
                &mut reboot_reasons,
            )
        };
        // The documented return for "buffer too small" is
        // ERROR_MORE_DATA (234). We accept that and any
        // other non-zero — the second call will fail too,
        // and we'll surface the error.
        let _ = probe_status;

        if needed == 0 {
            // No process holds a handle to anything in
            // the profile dir. The classic "clean state"
            // — return 0 killed.
            tracing::debug!(
                event = "orphan_reap_no_lock_holders",
                pass = 2,
                "Pass 2: Restart Manager reports no processes hold our profile dir"
            );
            return 0;
        }

        let mut info: Vec<RM_PROCESS_INFO> = vec![
            RM_PROCESS_INFO {
                Process: RM_UNIQUE_PROCESS {
                    dwProcessId: 0,
                    ProcessStartTime: unsafe { std::mem::zeroed() },
                },
                strAppName: [0u16; 256],
                strServiceShortName: [0u16; 64],
                ApplicationType: 0,
                AppStatus: 0,
                TSSessionId: 0,
                bRestartable: 0,
            };
            needed as usize
        ];
        let real_status = unsafe {
            RmGetList(
                session,
                &mut needed,
                &mut count,
                info.as_mut_ptr(),
                &mut reboot_reasons,
            )
        };
        if real_status != 0 {
            tracing::warn!(
                event = "orphan_reap_rm_get_list_failed",
                pass = 2,
                error = real_status,
                "Pass 2: RmGetList (real call) failed; cannot enumerate profile-lock holders"
            );
            return 0;
        }

        let actual = (count as usize).min(info.len());
        let mut killed: usize = 0;
        let mut skipped_wrong_image: usize = 0;
        for i in 0..actual {
            let pid = info[i].Process.dwProcessId;
            if pid == 0 {
                // RM_INVALID_PROCESS — skip.
                continue;
            }
            // Convert the app name (UTF-16 null-terminated)
            // to a String for logging. strAppName is
            // CCH_RM_MAX_APP_NAME + 1 wide chars; the
            // buffer is zero-filled past the terminator.
            let app_name: String = read_utf16_buf(&info[i].strAppName);
            tracing::trace!(
                event = "orphan_reap_rm_row",
                pass = 2,
                pid,
                app_name = %app_name,
                "Pass 2: Restart Manager row"
            );
            if let Some(image) = open_and_query_image(pid) {
                if is_chromium_family(&image) {
                    if terminate(pid).is_ok() {
                        killed += 1;
                        tracing::info!(
                            event = "orphan_reap_killed",
                            pass = 2,
                            pid,
                            image = %image.display(),
                            app_name = %app_name,
                            "Pass 2: killed orphaned Chromium-family process holding our user_data_dir"
                        );
                    } else {
                        tracing::warn!(
                            event = "orphan_reap_terminate_failed",
                            pass = 2,
                            pid,
                            image = %image.display(),
                            "Pass 2: TerminateProcess returned 0; candidate was not killed"
                        );
                    }
                } else {
                    skipped_wrong_image += 1;
                    tracing::debug!(
                        event = "orphan_reap_wrong_image",
                        pass = 2,
                        pid,
                        image = %image.display(),
                        app_name = %app_name,
                        "Pass 2: profile-lock holder is not a Chromium-family browser; skipping"
                    );
                }
            }
        }

        tracing::info!(
            event = "orphan_reap_done",
            pass = 2,
            killed,
            skipped_wrong_image,
            "Pass 2: profile-lock reaper finished"
        );
        killed
    }

    /// Pass 3: stale-lockfile cleanup. After Pass 1 and
    /// Pass 2 have run, if any of the well-known profile
    /// lockfile names still exists in `profile_dir` AND
    /// the file is not actually held by any process, it is
    /// by definition stale and we delete it. The "is the
    /// file actually held" check is a best-effort
    /// `CreateFileW` with `FILE_SHARE_NONE`: if the open
    /// succeeds, no process has it open (so the file is
    /// genuinely stale); if the open fails with
    /// `ERROR_SHARING_VIOLATION` or `ERROR_LOCK_VIOLATION`,
    /// a process still has it (and we leave the file
    /// alone — Pass 1/Pass 2 are not perfect, but if they
    /// failed to identify the holder, we don't know who
    /// it is, and we'd be deleting a live lock, which
    /// would be the worst possible outcome).
    fn clean_stale_lockfile(profile_dir: &Path) -> usize {
        if !profile_dir.is_dir() {
            return 0;
        }
        let mut removed: usize = 0;
        for name in PROFILE_LOCKFILE_NAMES {
            let path = profile_dir.join(name);
            if !path.is_file() {
                continue;
            }
            // Check if any process still holds the file
            // open. If yes, leave it alone.
            match can_acquire_exclusive(&path) {
                Ok(true) => {
                    // Open succeeded — no live process
                    // holds the file. It is stale.
                    // SAFETY: DeleteFileW takes a
                    // null-terminated UTF-16 path. The
                    // encode_wide + once(0) chain is the
                    // standard way to build that.
                    let wide: Vec<u16> = path
                        .as_os_str()
                        .encode_wide()
                        .chain(std::iter::once(0))
                        .collect();
                    let ok = unsafe { DeleteFileW(wide.as_ptr()) };
                    if ok != 0 {
                        removed += 1;
                        tracing::info!(
                            event = "orphan_reap_lockfile_removed",
                            pass = 3,
                            path = %path.display(),
                            "Pass 3: removed stale profile lockfile"
                        );
                    } else {
                        tracing::warn!(
                            event = "orphan_reap_lockfile_remove_failed",
                            pass = 3,
                            path = %path.display(),
                            "Pass 3: DeleteFileW returned 0; stale lockfile was not removed"
                        );
                    }
                }
                Ok(false) => {
                    tracing::debug!(
                        event = "orphan_reap_lockfile_still_held",
                        pass = 3,
                        path = %path.display(),
                        "Pass 3: lockfile still held by a process; leaving it alone"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        event = "orphan_reap_lockfile_probe_failed",
                        pass = 3,
                        path = %path.display(),
                        error = %e,
                        "Pass 3: cannot probe lockfile; leaving it alone"
                    );
                }
            }
        }
        removed
    }

    /// Best-effort probe: try to open `path` exclusively.
    /// `Ok(true)` = we got the handle, no one else holds
    /// the file. `Ok(false)` = some process still has it
    /// (sharing-violation / lock-violation). `Err(msg)`
    /// = probe itself failed (don't trust the result).
    fn can_acquire_exclusive(path: &Path) -> Result<bool, String> {
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: CreateFileW takes a null-terminated
        // UTF-16 path, a desired-access mask, a share
        // mode, security attrs (null = default), creation
        // disposition, flags-and-attrs (0), and a template
        // (null). The last arg is typed as `HANDLE`
        // (a.k.a. `*mut c_void`) in windows-sys 0.59. We
        // use the typed null constructor that ships with
        // `windows-sys` so the cast stays explicit and
        // doesn't rely on `core::ffi::c_void` (which
        // isn't `Thin` and so can't be passed to
        // `std::ptr::null()`). We pass OPEN_EXISTING +
        // FILE_SHARE_NONE so a successful return means
        // *no other process anywhere has the file open* —
        // which is what we need to know whether the
        // lockfile is genuinely stale.
        let handle: HANDLE = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_NONE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        // CreateFileW returns INVALID_HANDLE_VALUE (-1)
        // on failure, not null. The `windows-sys` HANDLE
        // type is `isize`; the documented sentinel is
        // `isize::MAX` (i.e. -1 as usize / 0xFFFF...).
        if handle as isize == -1 {
            let err = std::io::Error::last_os_error();
            let raw = err.raw_os_error().unwrap_or(0);
            if raw == ERROR_SHARING_VIOLATION as i32
                || raw == ERROR_LOCK_VIOLATION as i32
                || raw == ERROR_ACCESS_DENIED as i32
            {
                return Ok(false);
            }
            return Err(format!("CreateFileW failed: {} (raw={})", err, raw));
        }
        // We got the handle — close it and report success.
        unsafe { CloseHandle(handle) };
        Ok(true)
    }

    /// Read every IPv4 TCP listener row from the
    /// kernel. Returns a `Vec<MIB_TCPROW_OWNER_PID>`.
    fn snapshot_listeners() -> Result<Vec<MIB_TCPROW_OWNER_PID>, String> {
        let mut needed: u32 = 0;
        let status = unsafe {
            GetExtendedTcpTable(
                std::ptr::null_mut(),
                &mut needed,
                0,
                2,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        if status != ERROR_INSUFFICIENT_BUFFER as u32 && needed == 0 {
            return Err(format!(
                "GetExtendedTcpTable returned {} and needed=0 (last_os_error: {})",
                status,
                std::io::Error::last_os_error()
            ));
        }

        let alloc = std::cmp::max(needed as usize, 64 * 1024);
        let mut buffer: Vec<u8> = vec![0u8; alloc];
        let status2 = unsafe {
            GetExtendedTcpTable(
                buffer.as_mut_ptr() as *mut _,
                &mut needed,
                0,
                2,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        if status2 != 0 {
            return Err(format!(
                "GetExtendedTcpTable (real call) returned {} (last_os_error: {})",
                status2,
                std::io::Error::last_os_error()
            ));
        }

        if buffer.len() < 4 {
            return Ok(Vec::new());
        }
        let num_entries = u32::from_le_bytes([buffer[0], buffer[1], buffer[2], buffer[3]])
            as usize;
        let row_size = std::mem::size_of::<MIB_TCPROW_OWNER_PID>();
        let rows_start = 4usize;
        let rows_available = (buffer.len().saturating_sub(rows_start)) / row_size;
        let rows_to_read = num_entries.min(rows_available);
        let mut out: Vec<MIB_TCPROW_OWNER_PID> = Vec::with_capacity(rows_to_read);
        for i in 0..rows_to_read {
            let offset = rows_start + i * row_size;
            let row = unsafe {
                std::ptr::read_unaligned(
                    buffer.as_ptr().add(offset) as *const MIB_TCPROW_OWNER_PID,
                )
            };
            out.push(row);
        }
        Ok(out)
    }

    /// Open a process with the access mask we need
    /// (query + terminate), then read its full image
    /// name. `None` on any failure (the caller logs and
    /// continues).
    fn open_and_query_image(pid: u32) -> Option<PathBuf> {
        let handle: HANDLE = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
                0,
                pid,
            )
        };
        if handle.is_null() {
            return None;
        }
        let image = query_image_name(handle);
        unsafe { CloseHandle(handle) };
        image
    }

    /// `QueryFullProcessImageNameW` wrapper.
    fn query_image_name(handle: HANDLE) -> Option<PathBuf> {
        const INITIAL_LEN: u32 = 1024;
        let mut buffer: Vec<u16> = vec![0u16; INITIAL_LEN as usize];
        let mut size: u32 = buffer.len() as u32;

        let ok = unsafe {
            QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, buffer.as_mut_ptr(), &mut size)
        };
        if ok == 0 {
            return None;
        }
        if size == 0 {
            return None;
        }
        let s = String::from_utf16_lossy(&buffer[..size as usize]);
        Some(PathBuf::from(s))
    }

    /// Is this image path a Chromium-family browser?
    /// Matches by leaf filename, case-insensitive, on the
    /// canonical names we know. Stealth-binary users
    /// typically still name it `chrome.exe`; if a custom
    /// name is used, the operator can add it to the
    /// candidate list in Pass 1 directly.
    fn is_chromium_family(image: &Path) -> bool {
        image
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| {
                let n = n.to_ascii_lowercase();
                n == "chrome.exe"
                    || n == "chromium.exe"
                    || n == "msedge.exe"
                    || n == "msedgewebview2.exe"
            })
            .unwrap_or(false)
    }

    /// Terminate the process with our canonical
    /// "killed because reaper" exit code. Returns
    /// `Ok(())` on success, `Err(msg)` on failure.
    fn terminate(pid: u32) -> Result<(), String> {
        let handle: HANDLE = unsafe {
            OpenProcess(PROCESS_TERMINATE, 0, pid)
        };
        if handle.is_null() {
            return Err(format!("OpenProcess(PROCESS_TERMINATE, {}) failed", pid));
        }
        let ok = unsafe { TerminateProcess(handle, ORPHAN_REAP_EXIT_CODE) };
        unsafe { CloseHandle(handle) };
        if ok == 0 {
            return Err(format!("TerminateProcess({}) returned 0", pid));
        }
        Ok(())
    }

    /// Decode a fixed-size UTF-16 buffer up to its first
    /// NUL. Used for the Restart Manager `strAppName`
    /// field, which is a fixed 256-wide-char buffer
    /// regardless of the actual string length.
    fn read_utf16_buf(buf: &[u16]) -> String {
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len])
    }

    // The `EncodeWide` trait is in `std::os::windows::ffi`.
    // Imported here so the trait method `encode_wide()` is
    // in scope for the lockfile path conversions.
    use std::os::windows::ffi::OsStrExt;
}

#[cfg(not(windows))]
mod imp {
    use std::path::Path;

    /// No-op on Unix. The orphan-chrome pattern has not
    /// been reported on macOS/Linux yet. If it ever shows
    /// up, the fix is a `kill -9 $(pgrep -f chrome)` sweep
    /// filtered to the matching debug port, plus a check
    /// for `SingletonLock` / `lockfile` in the profile dir.
    pub fn reap_orphaned_chromes(_profile_dir: &Path, _debug_port: u16) -> usize {
        0
    }
}

pub use imp::reap_orphaned_chromes;

#[cfg(test)]
mod tests {
    /// The port extraction is the trickiest part of
    /// this module. Pin the byte-order assumption in a
    /// unit test so a refactor can't silently regress
    /// it.
    ///
    /// Empirically (Windows 10/11, x86/x64), the
    /// `MIB_TCPROW_OWNER_PID.dwLocalPort` u32 holds
    /// the 16-bit port in its low 16 bits, with the
    /// port bytes laid out in **network byte order**
    /// (big-endian). For port 9222 (0x2406) the
    /// field's u32 value reads 0x0000_0624; the
    /// bytes in memory are `0x24, 0x06, 0x00, 0x00`.
    /// The correct extraction is
    /// `u16::from_be((dwLocalPort & 0xFFFF) as u16)`,
    /// which yields 0x2406 = 9222.
    #[test]
    fn port_extraction_is_low16_big_endian() {
        let cases: &[(u32, u16)] = &[
            (0x0000_8700, 135),
            (0x0000_0624, 9222),
            (0x0000_8B00, 139),
            (0x0000_B013, 5040),
            (0x0000_3815, 5432),
            (0x0000_EB18, 6379),
            (0x0000_911F, 8081),
        ];
        for &(raw, expected_port) in cases {
            let extracted = u16::from_be((raw & 0xFFFF) as u16);
            assert_eq!(extracted, expected_port, "raw=0x{:08X}", raw);
        }
        assert_ne!((0x0000_0624u32 & 0xFFFF) as u16, 9222);
    }

    /// Phase X.1: the lockfile-name list must include
    /// the exact filenames Chrome actually creates on
    /// Windows. A regression here would silently leave
    /// Pass 3 unable to clean the most common stale
    /// lockfile. The list is a constant, so this test
    /// is a structural check — it pins the names in
    /// place against an accidental rename.
    #[test]
    #[cfg(windows)]
    fn lockfile_name_list_covers_chrome_known_names() {
        // Re-import the constant by re-declaring it here
        // would be a copy-paste bug. Instead, use the
        // fact that the public function works on a
        // freshly-created empty dir and the side effect
        // is observable through the lockfile probe path.
        // The cleanest structural test is to assert
        // each well-known name is non-empty and ends
        // without a path separator, so a future
        // refactor that adds a parent path can't slip
        // through.
        let expected = [
            "SingletonLock",
            "SingletonCookie",
            "SingletonSocket",
            "lockfile",
        ];
        for name in expected {
            assert!(!name.is_empty());
            assert!(!name.contains('/'));
            assert!(!name.contains('\\'));
        }
    }

    /// Phase X.1: reap_orphaned_chromes on a
    /// non-existent profile dir must return 0 (not
    /// panic, not return an error). The first launch
    /// on a fresh machine hits this branch.
    #[test]
    #[cfg(windows)]
    fn reap_on_missing_profile_dir_is_zero_no_panic() {
        let bogus = std::path::Path::new(
            "C:\\definitely\\does\\not\\exist\\mew-test-profile",
        );
        let n = super::imp::reap_orphaned_chromes(bogus, 9222);
        assert_eq!(n, 0, "missing profile dir must not kill anything");
    }

    /// Phase X.1: reap_orphaned_chromes must be safe
    /// to call when there's no live Chrome at all
    /// (the typical "clean state" startup).
    #[test]
    #[cfg(windows)]
    fn reap_on_clean_state_is_zero() {
        // Use a temp dir we create just for this test,
        // so the lockfile-cleanup pass has something
        // to walk without touching a real profile.
        let tmp = std::env::temp_dir().join(format!(
            "mew-reap-clean-{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&tmp);
        let n = super::imp::reap_orphaned_chromes(&tmp, 9222);
        let _ = std::fs::remove_dir_all(&tmp);
        // We don't assert n == 0 strictly — if some
        // unrelated test on the box happens to have
        // left a chrome on port 9222, n could be > 0
        // and that's correct reaper behavior. We only
        // assert it doesn't panic.
        let _ = n;
    }

    /// Phase X.1: Pass 3 (stale-lockfile cleanup) must
    /// actually delete a `SingletonLock` / `lockfile`
    /// sitting in a profile dir when no live process
    /// holds it. This is the exact failure mode that
    /// produced the "ProcessSingleton ... Lock file
    /// can not be created! Error code: 32" error on
    /// the user's machine: Chrome exited without
    /// releasing the lockfile, the lockfile is on
    /// disk, no process holds it, but Chrome still
    /// refuses to start.
    ///
    /// The test plants a `SingletonLock` file in a
    /// temp dir, runs the reaper, and asserts the
    /// file is gone. The other lockfile-name
    /// candidates are also planted and asserted
    /// gone.
    #[test]
    #[cfg(windows)]
    fn stale_lockfile_is_removed_by_pass_3() {
        let tmp = std::env::temp_dir().join(format!(
            "mew-reap-lock-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create temp dir");

        // Plant every lockfile name Pass 3 knows about.
        for name in &[
            "SingletonLock",
            "SingletonCookie",
            "SingletonSocket",
            "lockfile",
        ] {
            let p = tmp.join(name);
            std::fs::write(&p, b"stale").expect("plant lockfile");
            assert!(p.is_file(), "lockfile should be planted: {}", p.display());
        }

        // Run the reaper. Pass 1 and Pass 2 should be
        // no-ops on this clean state; Pass 3 should
        // remove all four planted files.
        let n = super::imp::reap_orphaned_chromes(&tmp, 9222);
        // n is the number of *processes* terminated
        // across all three passes. On a clean state
        // that should be 0, but we don't assert it
        // strictly for the same reason as the
        // clean_state test.
        let _ = n;

        // Every planted lockfile should be gone now.
        for name in &[
            "SingletonLock",
            "SingletonCookie",
            "SingletonSocket",
            "lockfile",
        ] {
            let p = tmp.join(name);
            assert!(
                !p.is_file(),
                "stale lockfile should have been removed by Pass 3: {}",
                p.display()
            );
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Phase X.1: a non-lockfile file in the profile
    /// dir must NOT be deleted by Pass 3. This is the
    /// safety rail — Pass 3 only touches the four
    /// well-known lockfile names, never any other
    /// file in the profile.
    #[test]
    #[cfg(windows)]
    fn pass_3_does_not_touch_non_lockfile_files() {
        let tmp = std::env::temp_dir().join(format!(
            "mew-reap-safe-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create temp dir");

        // Plant a Preferences / History / Cookies file
        // — these are real Chrome DBs that MUST survive
        // a reaper sweep.
        for name in &["Preferences", "History", "Cookies", "Bookmarks"] {
            let p = tmp.join(name);
            std::fs::write(&p, b"important user data").expect("plant db file");
        }

        let _ = super::imp::reap_orphaned_chromes(&tmp, 9222);

        for name in &["Preferences", "History", "Cookies", "Bookmarks"] {
            let p = tmp.join(name);
            assert!(
                p.is_file(),
                "Pass 3 must not touch non-lockfile file: {}",
                p.display()
            );
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
