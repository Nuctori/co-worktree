//! Platform VFS backend abstraction.
//!
//! The core engine is platform independent; only mounting the virtual merged
//! view is platform specific. Each supported platform ships one backend.

use std::path::Path;

use anyhow::Result;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[allow(dead_code)]
pub mod unsupported;
#[cfg(target_os = "windows")]
pub mod winfsp;

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
    fn run_isolated(
        &self,
        lower: &Path,
        upper: &Path,
        work: &Path,
        mountpoint: &Path,
        cmd: &[String],
        pidfile: &Path,
    ) -> Result<i32> {
        let mut guard = self.mount(lower, upper, work, mountpoint)?;
        let mut child = match std::process::Command::new(&cmd[0]).args(&cmd[1..]).spawn() {
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
                return Err(anyhow::anyhow!("spawn '{}': {e}", cmd[0]));
            }
        };
        write_pidfile(pidfile, child.id()).inspect_err(|e| {
            reap_orphan_child(&mut child);
            eprintln!("cowt: pidfile race lost: {e}");
        })?;
        let result = child.wait();
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
        Ok(status.code().unwrap_or(1))
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
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = std::fs::read_to_string(path)
                .ok()
                .and_then(|s| s.trim().split(':').next()?.parse::<u32>().ok());
            match existing {
                Some(p) if crate::state::pid_alive(p) => Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!("another `cowt run` is in progress (pid {p})"),
                )),
                _ => {
                    // Stale pidfile from a crashed run: replace it.
                    std::fs::write(path, body)
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

/// Process start time in a comparable form: Linux = starttime field of
/// /proc/<pid>/stat (clock ticks since boot); macOS = proc_pidinfo start
/// time (µs since boot); Windows = creation FILETIME (100ns since 1601).
/// `None` when unavailable. Only used to distinguish a recycled pid, so the
/// exact unit is irrelevant as long as it is stable per process.
pub(crate) fn process_starttime(pid: u32) -> Option<u128> {
    #[cfg(target_os = "macos")]
    {
        // libproc's proc_pidinfo(PROC_PIDTBSDINFO) — no crate needed.
        #[repr(C)]
        struct ProcBsdInfo {
            pbi_flags: u32,
            pbi_status: u32,
            pbi_xstatus: u32,
            pbi_pid: u32,
            pbi_ppid: u32,
            pbi_uid: u32,
            pbi_gid: u32,
            pbi_ruid: u32,
            pbi_rgid: u32,
            pbi_svuid: u32,
            pbi_svgid: u32,
            rfu_1: u32,
            pbi_comm: [u8; 64],
            pbi_name: [u8; 64],
            pbi_nfiles: u32,
            pbi_pgid: u32,
            pbi_pjobc: u32,
            e_tdev: u32,
            e_tpgid: u32,
            pbi_nice: i32,
            pbi_start_tvsec: u64,
            pbi_start_tvusec: u64,
        }
        extern "C" {
            fn proc_pidinfo(
                pid: i32,
                flavor: i32,
                arg: u64,
                buffer: *mut std::ffi::c_void,
                buffersize: i32,
            ) -> i32;
        }
        const PROC_PIDTBSDINFO: i32 = 3;
        let mut info = std::mem::MaybeUninit::<ProcBsdInfo>::zeroed();
        let n = unsafe {
            proc_pidinfo(
                pid as i32,
                PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr() as *mut std::ffi::c_void,
                std::mem::size_of::<ProcBsdInfo>() as i32,
            )
        };
        if n == std::mem::size_of::<ProcBsdInfo>() as i32 {
            let info = unsafe { info.assume_init() };
            Some((info.pbi_start_tvsec as u128) * 1_000_000 + info.pbi_start_tvusec as u128)
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
