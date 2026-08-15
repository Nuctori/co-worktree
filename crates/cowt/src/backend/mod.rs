//! Platform VFS backend abstraction.
//!
//! The core engine is platform independent; only mounting the virtual merged
//! view is platform specific. Each supported platform ships one backend.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};

use anyhow::Result;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[allow(dead_code)]
pub mod unsupported;
#[cfg(target_os = "windows")]
pub mod winfsp;

/// Pid of the isolated child, registered while `run_isolated` is executing.
/// Used by the signal handler to forward SIGTERM/SIGINT (round-26): without
/// forwarding, killing `cowt run` orphans the child, which keeps holding
/// the mount and the pidfile.
static CHILD_PID: AtomicI32 = AtomicI32::new(0);

/// Register the SIGTERM/SIGINT forwarding handler (unix). The handler is
/// async-signal-safe: it only kills the child (escalating to SIGKILL, since
/// the child may trap TERM/INT) and lets the normal wait/teardown path run.
#[cfg(unix)]
pub(crate) fn install_signal_forwarding() {
    extern "C" fn forward(sig: libc::c_int) {
        let pid = CHILD_PID.load(Ordering::Relaxed);
        if pid > 0 {
            // First deliver the same signal (graceful), then escalate to
            // SIGKILL if the child traps it.
            unsafe {
                libc::kill(pid, sig);
                libc::alarm(3);
            }
        }
    }
    extern "C" fn escalate(_: libc::c_int) {
        let pid = CHILD_PID.load(Ordering::Relaxed);
        if pid > 0 {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
    unsafe {
        // Casting a fn item to libc::sighandler_t is inherent to signal
        // registration; rustc's fn_to_numeric_cast has no cleaner form.
        #[allow(clippy::fn_to_numeric_cast)]
        libc::signal(libc::SIGTERM, forward as *const () as libc::sighandler_t);
        #[allow(clippy::fn_to_numeric_cast)]
        libc::signal(libc::SIGINT, forward as *const () as libc::sighandler_t);
        #[allow(clippy::fn_to_numeric_cast)]
        libc::signal(libc::SIGALRM, escalate as *const () as libc::sighandler_t);
    }
}
/// Register Ctrl-C/Ctrl-Break forwarding (Windows): the handler terminates
/// the child so the normal wait/teardown path can restore the host.
#[cfg(windows)]
pub(crate) fn install_signal_forwarding() {
    extern "system" fn handler(_ctrl: u32) -> windows::core::BOOL {
        let pid = CHILD_PID.load(Ordering::Relaxed);
        if pid > 0 {
            use windows::Win32::Foundation::CloseHandle;
            use windows::Win32::System::Threading::{
                OpenProcess, TerminateProcess, PROCESS_TERMINATE,
            };
            if let Ok(h) = unsafe { OpenProcess(PROCESS_TERMINATE, false, pid as u32) } {
                let _ = unsafe { TerminateProcess(h, 1) };
                // Round-28: don't leak the process handle on every Ctrl-C.
                let _ = unsafe { CloseHandle(h) };
            }
        }
        windows::core::BOOL(1) // handled: let the main thread reap and tear down
    }
    unsafe {
        use windows::Win32::System::Console::SetConsoleCtrlHandler;
        let _ = SetConsoleCtrlHandler(Some(handler), true);
    }
}

/// Register the child pid with the forwarding handler; returns a guard that
/// clears it on drop (so a stale pid is never signalled).
pub(crate) struct ChildSignalGuard;

impl ChildSignalGuard {
    pub fn track(pid: u32) -> Self {
        CHILD_PID.store(pid as i32, Ordering::Relaxed);
        ChildSignalGuard
    }
}

impl Drop for ChildSignalGuard {
    fn drop(&mut self) {
        CHILD_PID.store(0, Ordering::Relaxed);
        #[cfg(unix)]
        unsafe {
            libc::alarm(0); // cancel any pending escalation
        }
    }
}

/// Kill every process still in the child's process group (unix). Used after
/// the direct child exits to reap grandchildren that inherited the view
/// (e.g. backgrounded processes holding the mount cwd), which would
/// otherwise make the unmount fail with EBUSY and deadlock drop --force
/// (round-26).
#[cfg(unix)]
pub(crate) fn kill_child_process_group(pid: u32) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}
/// A virtual filesystem backend: mounts a merged view whose writes are
/// redirected to an isolated upper layer.
pub trait Backend: Send + Sync {
    /// Stable backend name, recorded in worktree metadata.
    fn name(&self) -> &'static str;

    /// Whether the backend's external dependencies are present on this host.
    fn available(&self) -> Result<()>;

    /// Mount the merged view at `mountpoint`:
    /// reads pass through to `lower` (the host directory), writes land in
    /// `upper`, and `work` is the overlayfs scratch dir.
    fn mount(
        &self,
        lower: &Path,
        upper: &Path,
        work: &Path,
        mountpoint: &Path,
    ) -> Result<MountGuard>;

    /// Unmount a mountpoint previously mounted by this backend.
    fn unmount(&self, mountpoint: &Path) -> Result<()>;

    /// Whether `mountpoint` currently has a filesystem mounted on it.
    fn is_mounted(&self, mountpoint: &Path) -> bool;

    /// Run a command with `mountpoint` overlaid. Default: mount, spawn, wait,
    /// unmount. Backends whose mount lives in a private namespace (kernel
    /// overlay via user namespace) override this with a single-shot wrapper.
    ///
    /// The implementation writes the child pid into `pidfile` once spawned.
    /// Returns `(exit_code, human description)`; on unix a signal-killed
    /// child is reported as such instead of a misleading "code 1".
    fn run_isolated(
        &self,
        lower: &Path,
        upper: &Path,
        work: &Path,
        mountpoint: &Path,
        cmd: &[String],
        pidfile: &Path,
    ) -> Result<(i32, String)> {
        let mut guard = self.mount(lower, upper, work, mountpoint)?;
        let mut cmd_ = std::process::Command::new(&cmd[0]);
        cmd_.args(&cmd[1..]);
        // Own process group (unix): lets us reap stray grandchildren that
        // inherited the view before unmounting (round-26).
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd_.process_group(0);
        }
        let mut child = match cmd_.spawn() {
            Ok(c) => c,
            Err(e) => {
                // The mount is up and the host dir sits in `real`: tear the
                // mount down explicitly (teardown drops the in-process host
                // FIRST, so the unmount can succeed — relying on Drop would
                // unmount while the WinFsp/FUSE-T host is still alive and
                // strand the host directory in `real`).
                if let Err(te) = guard.teardown() {
                    eprintln!("cowt: warning: unmount failed: {te:#}");
                }
                // Shell convention: 127 = command not found, 126 = not
                // executable (round-26) — scripts distinguish "missing"
                // from "failed".
                if let Some(code) = spawn_error_code(&e) {
                    eprintln!("cowt: cannot run '{}': {e}", cmd[0]);
                    return Ok((code, format!("failed to start '{}'", cmd[0])));
                }
                return Err(anyhow::anyhow!("spawn '{}': {e}", cmd[0]));
            }
        };
        match write_pidfile(pidfile, child.id()) {
            Ok(()) => {}
            Err(e) => {
                reap_orphan_child(&mut child);
                eprintln!("cowt: pidfile race lost: {e}");
                return Err(anyhow::anyhow!("{e}"));
            }
        }
        // Forward SIGTERM/SIGINT to the child so killing `cowt run` never
        // orphans it (round-26). Cleared when the guard drops below.
        let _sig = ChildSignalGuard::track(child.id());
        let result = child.wait();
        // Reap stray grandchildren (backgrounded processes that kept the
        // view's cwd / open files) before unmounting — otherwise the unmount
        // fails with EBUSY and drop --force deadlocks (round-26).
        #[cfg(unix)]
        kill_child_process_group(child.id());
        // Kernel overlayfs renames a lower directory lazily: upper/ holds the
        // renamed dir but its children stay un-materialized (resolved through
        // lower while mounted). Materialize them while the view is still
        // mounted so the offline upper scan (diff/apply) sees the children —
        // otherwise apply would drop them. Only dirs that exist in upper are
        // walked, so untouched lower files are never copied.
        #[cfg(target_os = "linux")]
        if let Err(e) = materialize_lazy_upper(mountpoint, upper) {
            eprintln!("cowt: warning: materialize upper failed: {e:#}");
        }
        // teardown() drops the in-process mount host first (WinFsp/FUSE-T),
        // then restores the host directory; on unix it is a plain unmount.
        if let Err(e) = guard.teardown() {
            eprintln!("cowt: warning: unmount failed: {e:#}");
        }
        let status = result.map_err(|e| anyhow::anyhow!("wait for child: {e}"))?;
        Ok(exit_code_and_desc(&status))
    }
}

/// Map a child's `ExitStatus` to `(exit_code, human description)`. On unix a
/// signal-killed child has no exit code; report the signal instead of a
/// misleading "code 1" (the run command still exits non-zero).
pub(crate) fn exit_code_and_desc(status: &std::process::ExitStatus) -> (i32, String) {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return (128 + sig, format!("killed by signal {sig}"));
        }
    }
    (
        status.code().unwrap_or(1),
        format!("exited with code {}", status.code().unwrap_or(1)),
    )
}

/// Shell-convention exit codes for spawn failures: 127 = command not found,
/// 126 = found but not executable (round-26). Other errors are reported as
/// the process-level failure (1) by the caller.
pub(crate) fn spawn_error_code(e: &std::io::Error) -> Option<i32> {
    match e.kind() {
        std::io::ErrorKind::NotFound => Some(127),
        std::io::ErrorKind::PermissionDenied => Some(126),
        _ => None,
    }
}

/// After a run, kernel overlayfs may have lazily copied up a renamed lower
/// directory: the dir exists in upper but its children are not materialized
/// (they resolve through lower while mounted). Copy any missing children
/// from the still-mounted merged view into upper, recursively, so the
/// offline upper scan matches what the view showed.
#[cfg(target_os = "linux")]
pub(crate) fn materialize_lazy_upper(view: &Path, upper: &Path) -> std::io::Result<()> {
    for e in std::fs::read_dir(upper)? {
        let e = e?;
        let name = e.file_name();
        let s = name.to_string_lossy();
        if s.starts_with(".wh.") {
            continue;
        }
        if !e.file_type()?.is_dir() {
            continue;
        }
        let view_dir = view.join(&name);
        let upper_dir = upper.join(&name);
        if std::fs::symlink_metadata(&view_dir).is_ok() {
            for ve in std::fs::read_dir(&view_dir)? {
                let ve = ve?;
                let vname = ve.file_name();
                let vs = vname.to_string_lossy();
                if vs.starts_with(".wh.") {
                    continue;
                }
                let dst = upper_dir.join(&vname);
                if std::fs::symlink_metadata(&dst).is_ok() {
                    continue;
                }
                let vft = ve.file_type()?;
                if vft.is_dir() {
                    std::fs::create_dir_all(&dst)?;
                } else if vft.is_file() {
                    std::fs::copy(ve.path(), &dst)?;
                }
            }
        }
        materialize_lazy_upper(&view_dir, &upper_dir)?;
    }
    Ok(())
}
/// Record the running child's pid (best effort — drop still verifies /proc).
/// Format: `<pid>` or `<pid>:<starttime>` — the starttime lets `drop
/// --force` distinguish a recycled pid (old crash residue whose pid was
/// reused by an unrelated process) from the actual child, preventing
/// killing innocent processes.
///
/// The pidfile is created with O_EXCL: a second `cowt run` racing through
/// the check-then-write window is refused instead of silently overwriting
/// the first run's pidfile (which would break every stale-run gate). A
/// pidfile whose pid is dead is stale and gets replaced.
///
/// On failure the caller must kill/wait the already-spawned child (the
/// spawn happens before this call; an untracked leftover would keep
/// running against the shared upper).
pub(crate) fn write_pidfile(path: &Path, pid: u32) -> std::io::Result<()> {
    let start = process_starttime(pid);
    let body = match start {
        Some(s) => format!("{pid}:{s}"),
        None => pid.to_string(),
    };
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut f) => {
            use std::io::Write;
            f.write_all(body.as_bytes())?;
            f.sync_all()?;
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // The pidfile exists. Is it a LIVE run (ours or another) or a
            // stale leftover from a crashed run? Same semantics as
            // State::running_pid: pid alive AND (no starttime, or starttime
            // matches) => live. Anything else (dead pid, recycled pid,
            // unparseable/empty content) is stale and replaceable.
            let s = fs::read_to_string(path).ok();
            let live = s.as_deref().and_then(|s| {
                let t = s.trim();
                let (pid, expected) = match t.split_once(':') {
                    Some((p, st)) => (p.parse::<u32>().ok()?, Some(st.parse::<u128>().ok()?)),
                    None => (t.parse::<u32>().ok()?, None),
                };
                if !crate::state::pid_alive(pid) {
                    return None;
                }
                if let Some(exp) = expected {
                    if process_starttime(pid) != Some(exp) {
                        return None; // recycled pid
                    }
                }
                Some(pid)
            });
            match live {
                Some(p) => Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!("another `cowt run` is in progress (pid {p})"),
                )),
                None => {
                    // Stale pidfile: replace atomically. remove-then-create
                    // closes the read-judge-write race — two concurrent
                    // runners both find it stale, but only one wins the
                    // O_EXCL create after removal (round-28).
                    match fs::remove_file(path) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e),
                    }
                    write_pidfile(path, pid) // retry: creates fresh, O_EXCL
                }
            }
        }
        Err(e) => Err(e),
    }
}

/// Reap an already-spawned child when the pidfile race was lost: without
/// this the loser's process keeps running untracked against the shared
/// upper after the caller returns the error.
pub(crate) fn reap_orphan_child(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// proc_shortbsdinfo from macOS libproc.h — mirrored exactly so
/// `pbsi_start_tvsec` lands on the real offset (88). Layout is pinned by a
/// size assertion in tests (a wrong layout silently returns None from
/// proc_pidinfo and disables the recycled-pid guard).
#[cfg(target_os = "macos")]
#[repr(C)]
pub(crate) struct ProcShortBsdInfo {
    pub(crate) pbsi_flags: u32,
    pub(crate) pbsi_status: u32,
    pub(crate) pbsi_xstatus: u32,
    pub(crate) pbsi_pid: u32,
    pub(crate) pbsi_ppid: u32,
    pub(crate) pbsi_uid: u32,
    pub(crate) pbsi_gid: u32,
    pub(crate) pbsi_ruid: u32,
    pub(crate) pbsi_rgid: u32,
    pub(crate) pbsi_svuid: u32,
    pub(crate) pbsi_svgid: u32,
    pub(crate) rfu_1: u32,
    pub(crate) pbsi_comm: [u8; 16], // MAXCOMLEN
    pub(crate) pbsi_nfiles: u32,
    pub(crate) pbsi_pgid: u32,
    pub(crate) pbsi_pjobc: u32,
    pub(crate) e_tdev: u32,
    pub(crate) e_tpgid: u32,
    pub(crate) pbsi_nice: i32,
    pub(crate) pbsi_start_tvsec: u64,
    pub(crate) pbsi_start_tvusec: u64,
}

/// Process start time in a comparable form: Linux = starttime field of
/// /proc/<pid>/stat (clock ticks since boot); macOS = proc_pidinfo start
/// time (µs since boot); Windows = creation FILETIME (100ns since 1601).
/// `None` when unavailable. Only used to distinguish a recycled pid, so the
/// exact unit is irrelevant as long as it is stable per process.
pub(crate) fn process_starttime(pid: u32) -> Option<u128> {
    #[cfg(target_os = "macos")]
    {
        // libproc's proc_pidinfo(PROC_PIDT_SHORTBSDINFO) — no crate needed.
        // proc_shortbsdinfo (libproc.h): 12×u32 header + MAXCOMLEN(16) comm
        // + 5×u32 + nice + start times; start_tvsec sits at offset 88.
        extern "C" {
            fn proc_pidinfo(
                pid: i32,
                flavor: i32,
                arg: u64,
                buffer: *mut std::ffi::c_void,
                buffersize: i32,
            ) -> i32;
        }
        const PROC_PIDT_SHORTBSDINFO: i32 = 4;
        let mut info = std::mem::MaybeUninit::<ProcShortBsdInfo>::zeroed();
        let n = unsafe {
            proc_pidinfo(
                pid as i32,
                PROC_PIDT_SHORTBSDINFO,
                0,
                info.as_mut_ptr() as *mut std::ffi::c_void,
                std::mem::size_of::<ProcShortBsdInfo>() as i32,
            )
        };
        if n == std::mem::size_of::<ProcShortBsdInfo>() as i32 {
            let info = unsafe { info.assume_init() };
            Some((info.pbsi_start_tvsec as u128) * 1_000_000 + info.pbsi_start_tvusec as u128)
        } else {
            None
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after_comm = stat.rfind(')')?;
        let fields: Vec<&str> = stat[after_comm + 1..].split_whitespace().collect();
        // Fields 3.. are state, ppid, ..., starttime is field 22 -> index 19.
        fields.get(19)?.parse().ok()
    }
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::{CloseHandle, FILETIME};
        use windows::Win32::System::Threading::{
            GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
        let mut created = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kern = FILETIME::default();
        let mut user = FILETIME::default();
        let ok = unsafe { GetProcessTimes(handle, &mut created, &mut exit, &mut kern, &mut user) };
        let _ = unsafe { CloseHandle(handle) };
        ok.is_ok()
            .then_some(((created.dwHighDateTime as u128) << 32) | created.dwLowDateTime as u128)
    }
}

/// RAII guard: unmounts on drop unless explicitly disarmed (used after a
/// deliberate unmount to make error paths idempotent). On Windows the guard
/// additionally owns the WinFsp mount host, so the filesystem is torn down
/// exactly when the guard drops.
pub struct MountGuard {
    mountpoint: std::path::PathBuf,
    armed: bool,
    #[cfg(windows)]
    #[allow(dead_code)] // written once, read by Drop semantics
    host: Option<winfsp::CowtHost>,
    #[cfg(target_os = "macos")]
    session: Option<fuser::BackgroundSession>,
}

impl MountGuard {
    // Constructed/consumed by platform backends; unused where no real backend exists.
    #[allow(dead_code)]
    pub fn new(mountpoint: std::path::PathBuf) -> Self {
        Self {
            mountpoint,
            armed: true,
            #[cfg(windows)]
            host: None,
            #[cfg(target_os = "macos")]
            session: None,
        }
    }

    /// macOS-only: wrap an already-mounted FUSE-T session.
    #[cfg(target_os = "macos")]
    pub fn with_session(mountpoint: std::path::PathBuf, session: fuser::BackgroundSession) -> Self {
        Self {
            mountpoint,
            armed: true,
            session: Some(session),
        }
    }

    /// Windows-only: wrap an already-mounted WinFsp host.
    #[cfg(windows)]
    pub fn with_host(mountpoint: std::path::PathBuf, host: winfsp::CowtHost) -> Self {
        Self {
            mountpoint,
            armed: true,
            host: Some(host),
        }
    }

    #[allow(dead_code)]
    pub fn disarm(&mut self) {
        self.armed = false;
    }

    /// Full teardown: drop the in-process mount host (WinFsp / FUSE-T), then
    /// restore the host directory. Used by the default `run_isolated` after
    /// the child exits — at that point the mount still lives inside this
    /// process, so a plain `unmount` cannot move the host dir back yet.
    /// On unix this is just a best-effort unmount.
    pub fn teardown(&mut self) -> Result<()> {
        #[cfg(windows)]
        {
            self.host.take(); // WinFsp volume goes away with the host
        }
        #[cfg(target_os = "macos")]
        {
            self.session.take(); // FUSE-T session unmounts on drop
        }
        let result = crate::backend::unmount_best_effort(&self.mountpoint);
        self.armed = false;
        result
    }
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = crate::backend::unmount_best_effort(&self.mountpoint);
        }
    }
}

/// Platform-default backend selection.
pub fn default_backend() -> Box<dyn Backend> {
    #[cfg(target_os = "linux")]
    {
        Box::new(linux::FuseOverlayfs)
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(winfsp::WinFspBackend)
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::FuseT)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        Box::new(unsupported::Unsupported::new(
            "none",
            "no VFS backend exists for this platform",
        ))
    }
}

/// Best-effort unmount used by guards and cleanup paths.
pub fn unmount_best_effort(mountpoint: &Path) -> Result<()> {
    default_backend().unmount(mountpoint)
}

/// Shared stale-mount gate for `run` / `diff` / `apply`.
///
/// A mount at `target` is only ever torn down when the worktree's own
/// pidfile proves a previous `cowt run` died (crashed or killed), or when
/// the moved-aside `real` dir exists (the kill-window crash case where the
/// pidfile was never written): either makes the mount ours by construction.
/// Anything else — a live run that has not written its pidfile yet, or a
/// foreign mount (manual mount, another tool) — refuses, preserving the
/// original "already a mountpoint" semantics.
///
/// Returns `true` when a stale mount was cleaned up.
pub fn recover_stale_mount(
    backend: &dyn Backend,
    dir: &std::path::Path,
    target: &Path,
) -> Result<bool> {
    if !backend.is_mounted(target) {
        return Ok(false);
    }
    let own_leftover = crate::state::State::stale_run(dir) || dir.join("real").exists();
    if own_leftover {
        eprintln!("cowt: removing stale mount at {}", target.display());
        backend.unmount(target)?;
        Ok(true)
    } else {
        anyhow::bail!(
            "{} is already a mountpoint; refusing to stack a second overlay",
            target.display()
        )
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_ffi_tests {
    use super::ProcShortBsdInfo;
    use std::mem::{offset_of, size_of};

    #[test]
    fn proc_shortbsdinfo_layout_matches_libproc() {
        // proc_shortbsdinfo from macOS libproc.h:
        // 12×u32 header (48) + char pbsi_comm[MAXCOMLEN=16] (64)
        // + 5×u32 (84) + int32 pbsi_nice (88) + u64 start (96, aligned)
        // + u64 usec (104). A wrong layout makes proc_pidinfo return 0 and
        // silently disables the recycled-pid guard.
        assert_eq!(size_of::<ProcShortBsdInfo>(), 104);
        assert_eq!(offset_of!(ProcShortBsdInfo, pbsi_start_tvsec), 88);
        assert_eq!(offset_of!(ProcShortBsdInfo, pbsi_start_tvusec), 96);
    }
}

#[cfg(test)]
mod pidfile_tests {
    use super::write_pidfile;
    use std::path::PathBuf;

    /// Round-28: a pidfile whose pid is alive but whose starttime does not
    /// match (recycled pid) is stale — write_pidfile must replace it, not
    /// refuse with a fake "another run in progress" (R28-04).
    #[test]
    fn write_pidfile_replaces_recycled_starttime() {
        let tmp = tempfile::tempdir().unwrap();
        let pf: PathBuf = tmp.path().join("run.pid");
        // Our own live pid with a bogus starttime = recycled-pid residual.
        std::fs::write(&pf, format!("{}:1", std::process::id())).unwrap();
        write_pidfile(&pf, std::process::id()).unwrap();
        let body = std::fs::read_to_string(&pf).unwrap();
        assert!(
            body.starts_with(&format!("{}:", std::process::id())),
            "recycled pidfile must be replaced with our pid:starttime, got {body}"
        );
    }

    /// Round-28: a pidfile with a LIVE pid (any starttime, e.g. plain
    /// format) must be refused — never overwrite an active run's marker
    /// (R28-01 keeps O_EXCL atomicity).
    #[test]
    fn write_pidfile_refuses_live_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let pf: PathBuf = tmp.path().join("run.pid");
        std::fs::write(&pf, std::process::id().to_string()).unwrap();
        let err = write_pidfile(&pf, std::process::id()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        // The live marker is untouched.
        assert_eq!(
            std::fs::read_to_string(&pf).unwrap(),
            std::process::id().to_string()
        );
    }
}
