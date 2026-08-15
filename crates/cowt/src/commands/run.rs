//! `cowt run <ID> -- <CMD...>` — run a process in the merged virtual view.

use anyhow::{bail, Result};

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
    let code = code?;
    eprintln!("cowt: process exited with code {code}; changes preserved in upper layer");
    Ok(code)
}
