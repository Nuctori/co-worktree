//! `cowt run <ID> -- <CMD...>` — run a process in the merged virtual view.

use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::backend::default_backend;
use crate::state::State;

pub struct RunArgs {
    pub id: String,
    pub cmd: Vec<String>,
}

pub fn run(args: RunArgs) -> Result<i32> {
    if args.cmd.is_empty() {
        bail!("no command given; usage: cowt run <ID> -- <CMD> [ARGS...]");
    }
    let state = State::open()?;
    let dir = state.resolve(&args.id)?;
    let meta = State::load_meta(&dir)?;
    let backend = default_backend();
    backend.available()?;

    if let Some(pid) = State::running_pid(&dir) {
        bail!(
            "worktree '{}' already has a running process (pid {pid}); \
             wait for it or `cowt drop --force`",
            meta.id
        );
    }
    if backend.is_mounted(&meta.target) {
        bail!(
            "{} is already a mountpoint; refusing to stack a second overlay",
            meta.target.display()
        );
    }

    let upper = dir.join("upper");
    let work = dir.join("work");
    let mut guard = backend
        .mount(&meta.target, &upper, &work, &meta.target)
        .with_context(|| format!("mount overlay at {}", meta.target.display()))?;
    eprintln!(
        "cowt: overlay mounted at {} (upper: {})",
        meta.target.display(),
        upper.display()
    );

    let result = run_child(&args.cmd, &dir);

    // Always unmount, whatever the child did (including crashes).
    let unmount_result = backend.unmount(&meta.target);
    State::clear_running(&dir);
    match unmount_result {
        Ok(()) => guard.disarm(),
        Err(e) => {
            // Guard will retry on drop as a last resort.
            eprintln!("cowt: warning: unmount failed: {e:#}");
        }
    }

    let code = result?;
    eprintln!("cowt: process exited with code {code}; changes preserved in upper layer");
    Ok(code)
}

fn run_child(cmd: &[String], dir: &std::path::Path) -> Result<i32> {
    let mut child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .spawn()
        .with_context(|| format!("spawn '{}'", cmd[0]))?;
    State::set_running(dir, child.id())?;
    let status = child.wait().context("wait for child process")?;
    Ok(status.code().unwrap_or(1))
}
