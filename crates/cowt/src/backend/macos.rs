//! macOS backend: kernel union mount (`mount -t union`).
//!
//! macOS ships a BSD-style union mount in the kernel — no third-party driver
//! needed, which is what makes it viable on CI (macFUSE kexts cannot be
//! approved headlessly on GitHub Actions runners). Writes trigger a kernel
//! copy-up into the upper directory; deletions produce `.wh.<name>` whiteout
//! files, which cowt-core already parses.
//!
//! Like the Linux `kernel-overlay` mode this requires root (`mount(2)` is
//! privileged); `cowt doctor` probes an actual mount to report availability.

use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};

use super::{Backend, MountGuard};

pub struct Union;

/// True once `mount -t union` has been observed to work on this host.
static AVAILABLE: OnceLock<bool> = OnceLock::new();

fn available() -> bool {
    *AVAILABLE.get_or_init(probe)
}

/// Probe: mount a union over a scratch dir, then unmount it.
fn probe() -> bool {
    let probe = std::env::temp_dir().join(format!("cowt-union-probe-{}", std::process::id()));
    let (upper, mountpoint) = (probe.join("upper"), probe.join("mount"));
    let dirs_ok = [&upper, &mountpoint]
        .iter()
        .all(|d| std::fs::create_dir_all(d).is_ok());
    if !dirs_ok {
        return false;
    }
    let ok = mount_union(&upper, &mountpoint).is_ok();
    if ok {
        // Unmount the probe immediately; the entry point stays mounted until
        // the process exits, which is fine for a throwaway temp dir, but a
        // clean teardown keeps `mount` output readable.
        let _ = Command::new("umount").arg(&mountpoint).status();
    }
    let _ = std::fs::remove_dir_all(&probe);
    ok
}

/// Human-readable reason when the union probe fails.
fn probe_reason() -> String {
    // macOS 15+ ships no `/Library/Filesystems/union.fs` helper at all —
    // `mount -t union` fails with exec ENOENT (verified on macos-latest).
    let helper = Path::new("/Library/Filesystems/union.fs/Contents/Resources/mount_union");
    if !helper.exists() {
        return format!(
            "this macOS no longer ships the union mount helper ({} is missing); \
             cowt's macOS backend requires macOS 14 (Sonoma) or older",
            helper.display()
        );
    }
    "union mount failed (root required — run as root or via sudo)".into()
}

/// `mount -t union -o nobrowse <upper> <mountpoint>`: the mountpoint's own
/// content becomes the lower layer (classic BSD union semantics).
fn mount_union(upper: &Path, mountpoint: &Path) -> Result<()> {
    let status = Command::new("mount")
        .args(["-t", "union", "-o", "nobrowse"])
        .arg(upper)
        .arg(mountpoint)
        .status()
        .context("spawn mount")?;
    if !status.success() {
        bail!(
            "kernel union mount failed at {} (root required; macOS union mounts \
             are unsupported on this host if this persists)",
            mountpoint.display()
        );
    }
    Ok(())
}

impl Backend for Union {
    fn name(&self) -> &'static str {
        "kernel-union"
    }

    fn available(&self) -> Result<()> {
        if available() {
            Ok(())
        } else {
            bail!("kernel union mount unavailable: {}", probe_reason())
        }
    }

    fn mount(
        &self,
        _lower: &Path,
        upper: &Path,
        _work: &Path,
        mountpoint: &Path,
    ) -> Result<MountGuard> {
        self.available()?;
        mount_union(upper, mountpoint)
            .with_context(|| format!("mount union at {}", mountpoint.display()))?;
        eprintln!(
            "cowt: kernel union mounted at {} (upper: {})",
            mountpoint.display(),
            upper.display()
        );
        Ok(MountGuard::new(mountpoint.to_path_buf()))
    }

    fn unmount(&self, mountpoint: &Path) -> Result<()> {
        if !self.is_mounted(mountpoint) {
            return Ok(());
        }
        let ok = Command::new("umount")
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
        // `mount` lists "<device> on <mountpoint> (<type>, ...)". The device
        // field is the upper dir; match the mountpoint position only.
        let needle = format!(" on {} ", mountpoint.display());
        Command::new("mount")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&needle))
            .unwrap_or(false)
    }
}
