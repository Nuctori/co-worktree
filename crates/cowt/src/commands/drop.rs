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
        State::clear_running(&dir);
    }

    // 2. A stale mount must come down before deleting state. Note the owning
    // `cowt run` may be unmounting concurrently after its child died, so the
    // outcome is decided by the final state, not by a single unmount call.
    for _ in 0..30 {
        if !backend.is_mounted(&meta.target) {
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
    if backend.is_mounted(&meta.target) {
        bail!("could not unmount {}; drop aborted", meta.target.display());
    }

    // 3. Atomic-ish removal: rename aside first so the worktree id vanishes
    // immediately, then delete the data directory.
    let trash = state.root().join(format!(
        ".trash-{}-{}",
        meta.id,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs::rename(&dir, &trash).with_context(|| format!("rename {} aside", dir.display()))?;
    fs::remove_dir_all(&trash).with_context(|| format!("delete {}", trash.display()))?;

    println!(
        "dropped worktree {} ({}): isolated data deleted, host directory untouched",
        meta.id,
        meta.target.display()
    );
    Ok(())
}

#[cfg(unix)]
fn terminate(pid: u32) {
    use std::process::Command;
    let _ = Command::new("kill").arg(pid.to_string()).status();
    // Wait briefly for SIGTERM to land, then escalate.
    for _ in 0..20 {
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
}

#[cfg(not(unix))]
fn terminate(_pid: u32) {}
