//! `cowt drop <ID>` — discard a worktree: unmount, then atomically remove state.

use std::fs;

use anyhow::{bail, Context, Result};

use crate::backend::default_backend;
use crate::state::State;

pub struct DropArgs {
    pub id: String,
    pub force: bool,
}

pub fn drop_cmd(args: DropArgs) -> Result<()> {
    let state = State::open()?;
    let dir = state.resolve(&args.id)?;
    // Round-23: a corrupt/unreadable meta.json (half-created fork, disk
    // damage) must not brick drop. Without --force we refuse with a clear
    // message; with --force we degrade to a synthetic meta (id from the
    // directory name, unknown target) and skip the mount checks — the
    // target cannot be known, and a foreign mount on an unknown target
    // cannot be verified. The `real` dir data-loss guard below still
    // applies (it does not need meta).
    let meta = match State::load_meta(&dir) {
        Ok(m) => m,
        Err(e) => {
            if !args.force {
                bail!(
                    "worktree state at {} has an unreadable meta.json ({e:#}); \
                     use --force to discard the damaged worktree",
                    dir.display()
                );
            }
            eprintln!("cowt: --force: discarding worktree with unreadable meta.json ({e:#})");
            crate::state::WorktreeMeta {
                id: dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                name: None,
                target: std::path::PathBuf::new(),
                created_epoch: 0,
                status: crate::state::Status::Ready,
                backend: String::new(),
            }
        }
    };
    let backend = default_backend();
    let target_known = !meta.target.as_os_str().is_empty();

    // 1. A live process holds the overlay: refuse, or kill with --force.
    if let Some(pid) = State::running_pid(&dir) {
        if !args.force {
            bail!(
                "worktree '{}' has a running process (pid {pid}); \
                 refusing to drop. Stop it or use --force.",
                meta.id
            );
        }
        eprintln!("cowt: --force: terminating pid {pid}");
        terminate(pid)?;
        // Note: run.pid is cleared *after* the unmount loop below — the
        // stale_run() discriminator needs it to prove the mount is ours.
    }

    // 2. A stale mount must come down before deleting state. Note the owning
    // `cowt run` may be unmounting concurrently after its child died, so the
    // outcome is decided by the final state, not by a single unmount call.
    // Only mounts proven to be our own leftover (stale pidfile, or a
    // moved-aside `real` dir — the kill-window crash case where the pidfile
    // was never written) are torn down. A foreign mount on the target is
    // never unmounted, even with --force (D-005 boundary).
    //
    // With a degraded meta (corrupt meta.json, --force) the target is
    // unknown: no mount check is possible, and the `real`-dir guard below
    // is the remaining data-loss protection.
    let mut foreign_mount = false;
    if target_known {
        for _ in 0..30 {
            if !backend.is_mounted(&meta.target) {
                break;
            }
            let own_leftover = State::stale_run(&dir)
                || dir.join("real").exists()
                || crate::backend::mount_upper_proves_ours(&meta.target, &dir);
            // The mount must ALSO be provably ours: a stale pidfile alone
            // does not authorize tearing down a foreign filesystem mounted
            // at the target later (D-005 boundary, round-31).
            let ours = mount_is_ours(&meta.target);
            if !own_leftover || !ours {
                foreign_mount = true;
                break;
            }
            if !args.force {
                bail!(
                    "{} is still mounted; refusing to drop. Unmount it or use --force.",
                    meta.target.display()
                );
            }
            eprintln!(
                "cowt: --force: unmounting {}",
                crate::state::sanitize_display(&meta.target.display().to_string())
            );
            let _ = backend.unmount(&meta.target); // tolerate races; verify below
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if foreign_mount || backend.is_mounted(&meta.target) {
            if dir.join("real").exists() {
                // Our own stale state: the mount could not be torn down because
                // something blocks the target (external dir, permissions).
                bail!(
                    "{} is blocked by leftover state (host dir still moved aside at {}); \
                     clear the blocker at the mount point, then drop again",
                    meta.target.display(),
                    dir.join("real").display()
                );
            }
            bail!(
                "{} is mounted by something else (not a cowt leftover); refusing to unmount it",
                meta.target.display()
            );
        }
    }

    // 3. Atomic-ish removal: rename aside first so the worktree id vanishes
    // immediately, then delete the data directory. On Windows the killed
    // `cowt run` may still be tearing down its WinFsp mount (inside the state
    // dir), so retry the deletion briefly. Old `.trash-*` leftovers from
    // previously failed drops are swept first.
    //
    // NEVER delete a state dir that still holds the moved-aside HOST
    // directory: `real` is the user's actual data (macOS can strand it when
    // the mountpoint symlink was removed externally — a subsequent drop
    // would silently destroy the host directory).
    //
    // The check happens AFTER the trash sweep and immediately before the
    // rename, so the sweep's (potentially slow) remove_dir_all cannot widen
    // a check→rename TOCTOU window during which a concurrent `cowt run`
    // could move the host dir into `real` (round-31).
    let dir_id = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| meta.id.clone());
    if let Ok(rd) = fs::read_dir(state.root()) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with(".trash-") {
                // A trash holding a moved-aside host dir (`real`) must NOT
                // be swept — deleting it would destroy user data (round-31).
                if e.path().join("real").exists() {
                    eprintln!(
                        "cowt: warning: {} still holds a moved-aside host dir; not sweeping it",
                        e.path().display()
                    );
                    continue;
                }
                let _ = fs::remove_dir_all(e.path());
            }
        }
    }
    // Re-check after the sweep, immediately before the rename (round-31).
    if dir.join("real").exists() {
        bail!(
            "the host directory is still moved aside at {}; refusing to delete it. \
             Restore it (or clear the blocker at the mount point) and drop again",
            dir.join("real").display()
        );
    }
    let trash = state.root().join(format!(
        ".trash-{}-{}",
        dir_id,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs::rename(&dir, &trash).with_context(|| format!("rename {} aside", dir.display()))?;
    // The host dir could have moved into `real` between the check and the
    // rename (a concurrent run's mount window); never delete that trash.
    if trash.join("real").exists() {
        let _ = fs::rename(&trash, &dir); // roll back
        bail!(
            "the host directory moved aside into {} during drop; rolled back. \
             Retry after the run finishes",
            trash.join("real").display()
        );
    }
    let mut last_err = None;
    for _ in 0..30 {
        match fs::remove_dir_all(&trash) {
            Ok(()) => {
                last_err = None;
                break;
            }
            // Already gone (a concurrent drop swept it): success.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                last_err = None;
                break;
            }
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
    if let Some(e) = last_err {
        return Err(anyhow::anyhow!(e)).with_context(|| format!("delete {}", trash.display()));
    }

    println!(
        "dropped worktree {} ({}): isolated data deleted, host directory untouched",
        crate::state::sanitize_display(&meta.id),
        crate::state::sanitize_display(&meta.target.display().to_string())
    );
    Ok(())
}

/// Whether the mount at `target` (if any) is provably one cowt created.
/// Linux: the /proc/self/mounts device must be overlay/fuse-overlayfs (a
/// stale pidfile alone does not authorize tearing down a foreign fs the
/// user mounted later — D-005 boundary, round-31). Other platforms: the
/// backend's own guards apply (macOS unmount validates the mountpoint
/// symlink; WinFsp mounts live inside the state dir).
#[cfg(target_os = "linux")]
fn mount_is_ours(target: &std::path::Path) -> bool {
    crate::backend::linux::mount_is_ours_proc(target)
}

#[cfg(not(target_os = "linux"))]
fn mount_is_ours(_target: &std::path::Path) -> bool {
    true
}

#[cfg(unix)]
fn terminate(pid: u32) -> Result<()> {
    use crate::state::pid_alive;
    use std::process::Command;
    let _ = Command::new("kill").arg(pid.to_string()).status();
    // Wait briefly for SIGTERM to land, then escalate. `kill -0` probes
    // liveness on macOS too, where /proc does not exist.
    for _ in 0..20 {
        if !pid_alive(pid) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
    // Verify SIGKILL landed; if the process survives, the drop must NOT
    // proceed (it would leave the mount/real in its hands and misreport
    // the blocker — round-31). A ZOMBIE counts as dead: it holds nothing
    // (the parent will reap it), but kill -0 still reports it alive.
    for _ in 0..20 {
        if !pid_alive(pid) || is_zombie(pid) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    anyhow::bail!(
        "process {pid} survived SIGKILL (uninterruptible or protected); \
         refusing to continue. Investigate the process, then drop again"
    )
}

/// True if `pid` is a zombie (Linux: /proc/<pid>/stat state 'Z'). A zombie
/// no longer runs and cannot hold the mount.
#[cfg(target_os = "linux")]
fn is_zombie(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| s.split_whitespace().nth(2).map(|st| st == "Z"))
        .unwrap_or(false)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn is_zombie(_pid: u32) -> bool {
    false
}

#[cfg(windows)]
fn terminate(pid: u32) -> Result<()> {
    use crate::state::pid_alive;
    use std::process::Command;
    let _ = Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .status();
    // taskkill returns before the process is fully gone; stale_run() needs
    // it dead, so wait for the exit.
    for _ in 0..50 {
        if !pid_alive(pid) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    anyhow::bail!(
        "process {pid} survived taskkill /F; refusing to continue. \
         Investigate the process, then drop again"
    )
}
