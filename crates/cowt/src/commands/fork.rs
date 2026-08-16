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

    // State root FIRST: every later failure (and the scan itself) needs it,
    // and the boundary checks need the state root to detect containment.
    let state = State::open()?;
    // Canonicalize for prefix comparison: the env (TMP, COWT_HOME) may
    // spell the same directory with an 8.3 short name vs the long form,
    // and Path::starts_with is byte-exact (round-35).
    let state_root = state
        .root()
        .canonicalize()
        .unwrap_or_else(|_| state.root().to_path_buf());
    #[cfg(windows)]
    let state_root = crate::state::dos_path(&state_root);

    // Round-35: the target and the state root must not contain each other.
    // If the target contains the state root, the baseline would snapshot
    // cowt's own state (other worktrees' uppers, run.pids), and on
    // winfsp/macOS the run would try to move the host dir into its own
    // subdirectory, failing forever. If the target is inside the state
    // root, it is cowt-internal state.
    if target.starts_with(state_root) || state_root.starts_with(&target) {
        bail!(
            "refusing to isolate {}: it contains (or is inside) the cowt state root {}",
            target.display(),
            state_root.display()
        );
    }

    // Boundary: only user-level directories are isolated by default. A
    // missing HOME (services/containers) must not silently DISABLE the
    // boundary — refuse instead of forking anything without --force-path
    // (round-35).
    if !args.force_path {
        match crate::state::home_dir() {
            Some(home) => {
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
            None => bail!(
                "refusing to isolate {}: $HOME is not set and --force-path was not given",
                target.display()
            ),
        }
    }

    // Round-35: a target that is currently a MOUNTED VIEW (another
    // worktree running, or a foreign mount) must not be forked — the
    // baseline would snapshot the merged view, including uncommitted upper
    // writes (possibly torn mid-write), and the new worktree could not run
    // anyway (a live pidfile refuses re-mounting).
    if backend::default_backend().is_mounted(&target) {
        bail!(
            "refusing to fork {}: it is currently mounted (a `cowt run` in progress \
             or a foreign mount); fork after the run finishes",
            target.display()
        );
    }

    let started = Instant::now();
    let scan = Manifest::scan(&target).context("scan base manifest")?;
    let scan_elapsed = started.elapsed();

    // Round-35: a fully-unreadable directory (0 entries + scan errors) must
    // not silently produce an "empty" worktree whose diff later reports
    // every host file as Added.
    if scan.manifest.entries.is_empty() && !scan.warnings.is_empty() {
        bail!(
            "cannot fork {}: the scan found 0 entries with {} error(s) (directory \
             unreadable?); fix permissions or drop the directory",
            target.display(),
            scan.warnings.len()
        );
    }

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

    let dir = state.create(&meta, &scan.manifest)?;
    let total_elapsed = started.elapsed();

    let files = scan
        .manifest
        .entries
        .values()
        .filter(|e| e.kind == cowt_core::EntryKind::File)
        .count();
    let symlinks = scan
        .manifest
        .entries
        .values()
        .filter(|e| e.kind == cowt_core::EntryKind::Symlink)
        .count();
    println!("forked worktree {}", meta.id);
    if let Some(n) = &meta.name {
        println!("  name:    {}", crate::state::sanitize_display(n));
    }
    println!(
        "  target:  {}",
        crate::state::sanitize_display(&target.display().to_string())
    );
    println!("  backend: {}", meta.backend);
    println!(
        "  state:   {}",
        crate::state::sanitize_display(&dir.display().to_string())
    );
    println!(
        "  base:    {} entries ({} files) scanned in {:.0}ms",
        scan.manifest.entries.len(),
        files,
        scan_elapsed.as_secs_f64() * 1000.0
    );
    if symlinks > 0 {
        eprintln!(
            "cowt: warning: {symlinks} symlink(s) detected — writes through them during `cowt run`\n\
             reach the host target directly (not isolated, invisible to `cowt diff`)"
        );
    }
    if !scan.warnings.is_empty() {
        println!("  warnings: {} entr(y/ies) skipped:", scan.warnings.len());
        for (p, why) in scan.warnings.iter().take(5) {
            println!(
                "    - {}: {why}",
                crate::state::sanitize_display(&p.display().to_string())
            );
        }
    }
    println!(
        "  fork completed in {:.1}ms",
        total_elapsed.as_secs_f64() * 1000.0
    );
    Ok(())
}
