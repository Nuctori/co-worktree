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
        let mut child = std::process::Command::new(&cmd[0])
            .args(&cmd[1..])
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawn '{}': {e}", cmd[0]))?;
        write_pidfile(pidfile, child.id());
        let result = child.wait();
        // teardown() drops the in-process mount host first (WinFsp/FUSE-T),
        // then restores the host directory; on unix it is a plain unmount.
        if let Err(e) = guard.teardown() {
            eprintln!("cowt: warning: unmount failed: {e:#}");
        }
        let status = result.map_err(|e| anyhow::anyhow!("wait for child: {e}"))?;
        Ok(status.code().unwrap_or(1))
    }
}
/// Record the running child's pid (best effort — drop still verifies /proc).
pub(crate) fn write_pidfile(path: &Path, pid: u32) {
    let _ = std::fs::write(path, pid.to_string());
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
        Box::new(macos::Union)
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
/// pidfile proves a previous `cowt run` died (crashed or killed): that makes
/// the mount ours by construction. Anything else — a live run that has not
/// written its pidfile yet, or a foreign mount (manual mount, another tool)
/// — refuses, preserving the original "already a mountpoint" semantics.
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
    if crate::state::State::stale_run(dir) {
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
