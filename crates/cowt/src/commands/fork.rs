//! `cowt fork <PATH>` — create an isolated worktree over a host directory.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use cowt_core::Manifest;

use crate::backend;
use crate::state::{default_name, short_id, State, Status, WorktreeMeta};

pub struct ForkArgs {
    pub path: PathBuf,
    pub name: Option<String>,
    pub force_path: bool,
}
pub fn fork(args: ForkArgs) -> Result<()> {
    let target = args
        .path
        .canonicalize()
        .with_context(|| format!("target directory {} not found", args.path.display()))?;
    // On Windows canonicalize returns a `\\?\` verbatim path; strip it so
    // every later path (mount, restore, junction checks) is consistent with
    // the environment's short/long spellings.
    #[cfg(windows)]
    let target = crate::state::dos_path(&target);
    if !target.is_dir() {
        bail!("{} is not a directory", target.display());
    }

    // Boundary: only user-level directories are isolated by default.
    if !args.force_path {
        if let Some(home) = crate::state::home_dir() {
            // canonicalize() both sides: on Windows it emits a `\\?\` extended
            // prefix that would otherwise break prefix comparison.
            let home = home.canonicalize().unwrap_or(home);
            #[cfg(windows)]
            let home = crate::state::dos_path(&home);
            if !target.starts_with(&home) {
                bail!(
                    "refusing to isolate {}: only directories under $HOME are supported\n\
                     (system directories are out of scope by design; pass --force-path to override)",
                    target.display()
                );
            }
        }
    }

    let started = Instant::now();
    let scan = Manifest::scan(&target).context("scan base manifest")?;
    let scan_elapsed = started.elapsed();

    let id = short_id();
    let name = args.name.or_else(|| Some(default_name(&target)));
    let backend = backend::default_backend();
    let meta = WorktreeMeta {
        id: id.clone(),
        name,
        target: target.clone(),
        created_epoch: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        status: Status::Ready,
        backend: backend.name().to_string(),
    };

    let state = State::open()?;
    let dir = state.create(&meta, &scan.manifest)?;
    let total_elapsed = started.elapsed();

    let files = scan
        .manifest
        .entries
        .values()
        .filter(|e| e.kind == cowt_core::EntryKind::File)
        .count();
    println!("forked worktree {}", meta.id);
    if let Some(n) = &meta.name {
        println!("  name:    {n}");
    }
    println!("  target:  {}", target.display());
    println!("  backend: {}", meta.backend);
    println!("  state:   {}", dir.display());
    println!(
        "  base:    {} entries ({} files) scanned in {:.0}ms",
        scan.manifest.entries.len(),
        files,
        scan_elapsed.as_secs_f64() * 1000.0
    );
    if !scan.warnings.is_empty() {
        println!("  warnings: {} entr(y/ies) skipped:", scan.warnings.len());
        for (p, why) in scan.warnings.iter().take(5) {
            println!("    - {}: {why}", p.display());
        }
    }
    println!(
        "  fork completed in {:.1}ms",
        total_elapsed.as_secs_f64() * 1000.0
    );
    Ok(())
}
