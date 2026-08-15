//! Linux backend with three strategies, auto-detected per host:
//!
//! 1. **Kernel overlayfs, direct mount** (when running as root): native
//!    kernel performance, explicit umount on exit.
//! 2. **Kernel overlayfs inside a rootless user namespace** (unprivileged):
//!    `unshare --mount --map-root-user` provides a private mount namespace;
//!    the mount vanishes with the namespace — zero host residue.
//! 3. **fuse-overlayfs** (fallback): user-space overlay for hosts where
//!    unprivileged user namespaces are restricted (e.g. AppArmor-confined
//!    Ubuntu runners).

use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};

use super::{Backend, MountGuard};

pub struct FuseOverlayfs;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    KernelDirect,
    KernelUserns,
    Fuse,
}

static MODE: OnceLock<Mode> = OnceLock::new();

/// Probe once for the best available strategy on this host.
fn detect_mode() -> Mode {
    *MODE.get_or_init(|| {
        if euid() == 0 && kernel_direct_works() {
            Mode::KernelDirect
        } else if kernel_userns_works() {
            Mode::KernelUserns
        } else {
            Mode::Fuse
        }
    })
}

/// Effective uid parsed from /proc (avoids a libc dependency).
fn euid() -> u32 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(2)?.parse().ok())
        })
        .unwrap_or(u32::MAX)
}

/// Probe: direct kernel overlay mount (root only).
fn kernel_direct_works() -> bool {
    with_probe_dirs(|l, u, w, m| {
        let ok = Command::new("mount")
            .arg("-t")
            .arg("overlay")
            .arg("overlay")
            .arg("-o")
            .arg(format!(
                "lowerdir={},upperdir={},workdir={}",
                l.display(),
                u.display(),
                w.display()
            ))
            .arg(m)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            let _ = Command::new("umount").arg(m).status();
        }
        ok
    })
}

/// Probe: kernel overlay inside a rootless user namespace.
fn kernel_userns_works() -> bool {
    with_probe_dirs(|l, u, w, m| {
        Command::new("unshare")
            .args([
                "--mount",
                "--map-root-user",
                "--fork",
                "--kill-child",
                "--propagation",
                "private",
                "sh",
                "-c",
                "mount -t overlay overlay -o \"lowerdir=$1,upperdir=$2,workdir=$3\" \"$4\"",
                "cowt-probe",
            ])
            .arg(l)
            .arg(u)
            .arg(w)
            .arg(m)
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

fn with_probe_dirs(f: impl Fn(&Path, &Path, &Path, &Path) -> bool) -> bool {
    let probe = std::env::temp_dir().join(format!("cowt-probe-{}", std::process::id()));
    let (l, u, w, m) = (
        probe.join("l"),
        probe.join("u"),
        probe.join("w"),
        probe.join("m"),
    );
    let dirs_ok = [&l, &u, &w, &m]
        .iter()
        .all(|d| std::fs::create_dir_all(d).is_ok());
    let ok = dirs_ok && f(&l, &u, &w, &m);
    let _ = std::fs::remove_dir_all(&probe);
    ok
}

/// Shell wrapper executed inside the user namespace: mount, then exec the
/// user command (`"$@"`).
const WRAPPER: &str = r#"set -e
mount -t overlay overlay -o "lowerdir=$COWT_LOWER,upperdir=$COWT_UPPER,workdir=$COWT_WORK" "$COWT_MNT"
exec "$@""#;

impl FuseOverlayfs {
    /// Run `cmd` with `mountpoint` overlaid via a direct kernel mount (root).
    fn run_kernel_direct(
        &self,
        lower: &Path,
        upper: &Path,
        work: &Path,
        mountpoint: &Path,
        cmd: &[String],
        pidfile: &Path,
    ) -> Result<i32> {
        let status = Command::new("mount")
            .arg("-t")
            .arg("overlay")
            .arg("overlay")
            .arg("-o")
            .arg(format!(
                "lowerdir={},upperdir={},workdir={}",
                lower.display(),
                upper.display(),
                work.display()
            ))
            .arg(mountpoint)
            .status()
            .context("spawn mount")?;
        if !status.success() {
            bail!("kernel overlay mount failed at {}", mountpoint.display());
        }
        let mut guard = MountGuard::new(mountpoint.to_path_buf());
        eprintln!(
            "cowt: kernel overlay mounted at {} (upper: {})",
            mountpoint.display(),
            upper.display()
        );
        let mut child = Command::new(&cmd[0])
            .args(&cmd[1..])
            .spawn()
            .with_context(|| format!("spawn '{}'", cmd[0]))?;
        super::write_pidfile(pidfile, child.id())?;
        let result = child.wait();
        // Lazy copy-up: a renamed lower dir has no materialized children in
        // upper; copy them from the still-mounted view so the offline scan
        // (diff/apply) matches what the program saw (else apply drops them).
        // Userns mode cannot do this from here (the mount lives in a private
        // namespace) — documented limitation.
        if let Err(e) = super::materialize_lazy_upper(mountpoint, upper) {
            eprintln!("cowt: warning: materialize upper failed: {e:#}");
        }
        match self.unmount(mountpoint) {
            Ok(()) => guard.disarm(),
            Err(e) => eprintln!("cowt: warning: unmount failed: {e:#}"),
        }
        let status = result.context("wait for child process")?;
        Ok(status.code().unwrap_or(1))
    }

    /// Run `cmd` with `mountpoint` overlaid via rootless kernel overlayfs.
    fn run_kernel_userns(
        &self,
        lower: &Path,
        upper: &Path,
        work: &Path,
        mountpoint: &Path,
        cmd: &[String],
        pidfile: &Path,
    ) -> Result<i32> {
        eprintln!(
            "cowt: kernel overlay (user namespace) over {} (upper: {})",
            mountpoint.display(),
            upper.display()
        );
        let mut child = Command::new("unshare")
            .args([
                "--mount",
                "--map-root-user",
                "--fork",
                "--kill-child",
                "--propagation",
                "private",
                "sh",
                "-c",
                WRAPPER,
                "cowt-run",
            ])
            .args(cmd)
            .env("COWT_LOWER", lower)
            .env("COWT_UPPER", upper)
            .env("COWT_WORK", work)
            .env("COWT_MNT", mountpoint)
            .spawn()
            .context("spawn unshare wrapper")?;
        super::write_pidfile(pidfile, child.id())?;
        let status = child.wait().context("wait for isolated process")?;
        Ok(status.code().unwrap_or(1))
    }

    fn run_fuse(
        &self,
        lower: &Path,
        upper: &Path,
        work: &Path,
        mountpoint: &Path,
        cmd: &[String],
        pidfile: &Path,
    ) -> Result<i32> {
        self.available()?;
        let mut guard = self
            .mount(lower, upper, work, mountpoint)
            .with_context(|| format!("mount overlay at {}", mountpoint.display()))?;
        eprintln!(
            "cowt: fuse-overlayfs mounted at {} (upper: {})",
            mountpoint.display(),
            upper.display()
        );
        let mut child = Command::new(&cmd[0])
            .args(&cmd[1..])
            .spawn()
            .with_context(|| format!("spawn '{}'", cmd[0]))?;
        super::write_pidfile(pidfile, child.id())?;
        let result = child.wait();
        // Always unmount, whatever the child did (including crashes).
        match self.unmount(mountpoint) {
            Ok(()) => guard.disarm(),
            Err(e) => eprintln!("cowt: warning: unmount failed: {e:#}"),
        }
        let status = result.context("wait for child process")?;
        Ok(status.code().unwrap_or(1))
    }
}

impl Backend for FuseOverlayfs {
    fn name(&self) -> &'static str {
        match detect_mode() {
            Mode::KernelDirect => "kernel-overlay",
            Mode::KernelUserns => "kernel-overlay+userns",
            Mode::Fuse => "fuse-overlayfs",
        }
    }

    fn available(&self) -> Result<()> {
        match detect_mode() {
            Mode::KernelDirect | Mode::KernelUserns => Ok(()),
            Mode::Fuse => {
                if !Path::new("/dev/fuse").exists() {
                    bail!("/dev/fuse is missing: FUSE is not available on this host");
                }
                let out = Command::new("fuse-overlayfs")
                    .arg("--version")
                    .output()
                    .context(
                        "fuse-overlayfs not found in PATH (install the 'fuse-overlayfs' package)",
                    )?;
                if !out.status.success() {
                    bail!("fuse-overlayfs --version failed");
                }
                Ok(())
            }
        }
    }

    fn mount(
        &self,
        lower: &Path,
        upper: &Path,
        work: &Path,
        mountpoint: &Path,
    ) -> Result<MountGuard> {
        self.available()?;
        let opts = format!(
            "lowerdir={},upperdir={},workdir={}",
            lower.display(),
            upper.display(),
            work.display()
        );
        let status = Command::new("fuse-overlayfs")
            .arg("-o")
            .arg(&opts)
            .arg(mountpoint)
            .status()
            .context("spawn fuse-overlayfs")?;
        if !status.success() {
            bail!("fuse-overlayfs mount failed at {}", mountpoint.display());
        }
        Ok(MountGuard::new(mountpoint.to_path_buf()))
    }

    fn unmount(&self, mountpoint: &Path) -> Result<()> {
        // Userns-mode mounts live in a private namespace and are never visible
        // here; direct kernel and fuse mounts are torn down via umount.
        if !self.is_mounted(mountpoint) {
            return Ok(());
        }
        let ok = Command::new("fusermount3")
            .arg("-u")
            .arg(mountpoint)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
            || Command::new("fusermount")
                .arg("-u")
                .arg(mountpoint)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
            || Command::new("umount")
                .arg(mountpoint)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
        if ok {
            Ok(())
        } else {
            bail!("failed to unmount {}", mountpoint.display());
        }
    }

    fn is_mounted(&self, mountpoint: &Path) -> bool {
        is_mounted_proc(mountpoint)
    }

    fn run_isolated(
        &self,
        lower: &Path,
        upper: &Path,
        work: &Path,
        mountpoint: &Path,
        cmd: &[String],
        pidfile: &Path,
    ) -> Result<i32> {
        match detect_mode() {
            Mode::KernelDirect => {
                self.run_kernel_direct(lower, upper, work, mountpoint, cmd, pidfile)
            }
            Mode::KernelUserns => {
                self.run_kernel_userns(lower, upper, work, mountpoint, cmd, pidfile)
            }
            Mode::Fuse => self.run_fuse(lower, upper, work, mountpoint, cmd, pidfile),
        }
    }
}

/// Check /proc/self/mounts for the mountpoint (handles \040-style escaping).
pub fn is_mounted_proc(mountpoint: &Path) -> bool {
    let target = mountpoint.to_string_lossy().replace(' ', "\\040");
    let Ok(mounts) = std::fs::read_to_string("/proc/self/mounts") else {
        return false;
    };
    mounts.lines().any(|line| {
        let mut fields = line.split_whitespace();
        fields.next(); // device
        fields.next() == Some(target.as_str())
    })
}
