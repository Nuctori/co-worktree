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
    let meta = State::load_meta(&dir)?;
    let backend = default_backend();

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
        terminate(pid);
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
    let mut foreign_mount = false;
    for _ in 0..30 {
        if !backend.is_mounted(&meta.target) {
            break;
        }
        let own_leftover = State::stale_run(&dir) || dir.join("real").exists();
        if !own_leftover {
            foreign_mount = true;
            break;
        }
        if !args.force {
            bail!(
                "{} is still mounted; refusing to drop. Unmount it or use --force.",
                meta.target.display()
            );
        }
        eprintln!("cowt: --force: unmounting {}", meta.target.display());
        let _ = backend.unmount(&meta.target); // tolerate races; verify below
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if foreign_mount || backend.is_mounted(&meta.target) {
        bail!(
            "{} is mounted by something else (not a cowt leftover); refusing to unmount it",
            meta.target.display()
        );
    }

    // 3. Atomic-ish removal: rename aside first so the worktree id vanishes
    // immediately, then delete the data directory. On Windows the killed
    // `cowt run` may still be tearing down its WinFsp mount (inside the state
    // dir), so retry the deletion briefly. Old `.trash-*` leftovers from
    // previously failed drops are swept first.
    let trash = state.root().join(format!(
        ".trash-{}-{}",
        meta.id,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    if let Ok(rd) = fs::read_dir(state.root()) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with(".trash-") {
                let _ = fs::remove_dir_all(e.path());
            }
        }
    }
    fs::rename(&dir, &trash).with_context(|| format!("rename {} aside", dir.display()))?;
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

#[cfg(unix)]
fn terminate(pid: u32) {
    use crate::state::pid_alive;
    use std::process::Command;
    let _ = Command::new("kill").arg(pid.to_string()).status();
    // Wait briefly for SIGTERM to land, then escalate. `kill -0` probes
    // liveness on macOS too, where /proc does not exist.
    for _ in 0..20 {
        if !pid_alive(pid) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
}

#[cfg(windows)]
fn terminate(pid: u32) {
    use crate::state::pid_alive;
    use std::process::Command;
    let _ = Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .status();
    // taskkill returns before the process is fully gone; stale_run() needs
    // it dead, so wait for the exit.
    for _ in 0..50 {
        if !pid_alive(pid) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
