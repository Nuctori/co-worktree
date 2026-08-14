//! Linux backend: fuse-overlayfs (user-space, no root required for the mount
//! itself when the user has /dev/fuse access; whiteout creation may require
//! mknod privileges depending on the host).

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

use super::{Backend, MountGuard};

pub struct FuseOverlayfs;

impl Backend for FuseOverlayfs {
    fn name(&self) -> &'static str {
        "fuse-overlayfs"
    }

    fn available(&self) -> Result<()> {
        if !Path::new("/dev/fuse").exists() {
            bail!("/dev/fuse is missing: FUSE is not available on this host");
        }
        let out = Command::new("fuse-overlayfs")
            .arg("--version")
            .output()
            .context("fuse-overlayfs not found in PATH (install the 'fuse-overlayfs' package)")?;
        if !out.status.success() {
            bail!("fuse-overlayfs --version failed");
        }
        Ok(())
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
        // Try the unprivileged FUSE unmount first, then fall back to umount.
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
