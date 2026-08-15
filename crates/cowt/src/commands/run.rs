//! `cowt run <ID> -- <CMD...>` — run a process in the merged virtual view.

use std::fs;

use anyhow::{bail, Context, Result};

use crate::backend::{default_backend, recover_stale_mount};
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
    recover_stale_mount(backend.as_ref(), &dir, &meta.target)?;

    let upper = dir.join("upper");
    let work = dir.join("work");
    // Self-heal a crashed apply (round-24): a kill -9 between apply's
    // remove_dir_all(upper) and create_dir_all(upper) leaves the layer
    // missing — the mount needs it to exist.
    fs::create_dir_all(&upper)
        .with_context(|| format!("create upper layer {}", upper.display()))?;
    fs::create_dir_all(&work).with_context(|| format!("create work layer {}", work.display()))?;
    let pidfile = dir.join("run.pid");
    let code = backend.run_isolated(
        &meta.target,
        &upper,
        &work,
        &meta.target,
        &args.cmd,
        &pidfile,
    );
    State::clear_running(&dir);
    let (code, desc) = code?;
    // The moved-aside host dir must be back. If it is not (e.g. the macOS
    // mountpoint symlink was removed externally), say so loudly and force a
    // non-zero exit — a script must be able to tell that the worktree is in
    // a damaged state even when the child happened to exit 0. A later
    // `cowt drop` refuses to delete state that still holds the host dir.
    if dir.join("real").exists() {
        eprintln!(
            "cowt: ERROR: the host directory was NOT restored — it is still at {}. \
             Fix the mount point state before dropping this worktree",
            dir.join("real").display()
        );
        return Ok(1);
    }
    eprintln!("cowt: process {desc}; changes preserved in upper layer");
    Ok(code)
}
