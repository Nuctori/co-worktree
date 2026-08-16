//! `cowt apply <ID>` — three-way merge worktree changes into the host directory.

use anyhow::{bail, Result};
use cowt_core::{merge, overlay, Manifest};
use std::fs;

use crate::backend::{default_backend, recover_stale_mount};
use crate::state::{State, Status};

pub struct ApplyArgs {
    pub id: String,
    pub dry_run: bool,
    pub json: bool,
}

pub fn apply(args: ApplyArgs) -> Result<i32> {
    let state = State::open()?;
    let dir = state.resolve(&args.id)?;
    let meta = State::load_meta(&dir)?;
    // Round-36: a kill -9 inside a previous apply's staging phase leaves
    // .cowt-apply-<pid>-<nanos>/ behind in the target's parent. Sweep
    // stale ones (dead pid) before staging a fresh apply, so crashes do
    // not accumulate unbounded copies of staged files.
    let parent = meta
        .target
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    crate::state::sweep_stale_staging(parent);
    if State::running_pid(&dir).is_some() {
        bail!(
            "worktree '{}' is running; apply after the process exits",
            meta.id
        );
    }
    // Never merge into a live or stale mount: the write must land in the
    // real host directory. Stale leftovers (crashed run) are restored first.
    recover_stale_mount(default_backend().as_ref(), &dir, &meta.target)?;

    let base = State::load_manifest(&dir)?;
    let upper = dir.join("upper");
    // Full re-hash of the host: `rescan`'s stat_eq fast path reuses the base
    // hash when size/mtime match, but an external tool can rewrite a file
    // preserving both (touch -r, rsync -t, FAT 2s mtime granularity) — a
    // silent overwrite of that change by apply would lose data.
    let current = Manifest::scan(&meta.target)
        .map_err(|e| {
            // Round-37: a bare "io error on <path>" gave no hint what to do
            // when the target is missing (crashed-run strand or external
            // delete).
            anyhow::anyhow!(
                "cannot scan target directory {}: {e}; if a crashed run moved it aside it \
                 is at {} (any command restores it), otherwise check whether it was deleted \
                 externally",
                meta.target.display(),
                dir.join("real").display()
            )
        })?
        .manifest;
    // Round-23 guard: a whiteout whose victim exists on the host but NOT in
    // the base manifest means the base is semantically corrupted (entries
    // wiped, or a foreign manifest copied in). Planning would hit the
    // b_eq_w "keep host" branch, silently dropping the worktree's deletion
    // intent — then apply would clear upper and advance the baseline,
    // destroying the only record of the intent. Refuse loudly instead.
    // (A legitimate create-then-delete whiteout never matches: its victim
    // is absent from the host too.)
    for victim in overlay::whiteout_victims(&upper) {
        if base.get(&victim).is_none() && current.get(&victim).is_some() {
            bail!(
                "worktree '{}' has a deletion marker for '{}' but the base manifest \
                 has no such entry (base manifest is corrupted or from another tree); \
                 refusing to apply so the deletion intent is not silently lost. \
                 Restore the original manifest.json, or drop the worktree",
                meta.id,
                victim.display()
            );
        }
    }
    let work = overlay::effective_manifest_fold(&base, &upper, crate::state::case_fold_host())?;

    // Round-38-02: on a case-insensitive host, a worktree-added path that
    // differs from an existing base/host path by case alone is physically
    // the same file — the byte-exact plan would emit a WriteFile that
    // Windows verify_unchanged then misreports as a TOCTOU "appeared on
    // the host", a permanent deadlock. Refuse with an explicit conflict
    // instead.
    if crate::state::case_fold_host() {
        let coll = merge::case_fold_conflicts(&base, &work, &current);
        if !coll.is_empty() {
            bail!(
                "cannot apply on this case-insensitive filesystem: the worktree \
                 contains path(s) that collide with existing files by case alone: {}; \
                 rename one side (in the worktree view or on the host) and re-apply",
                coll.iter()
                    .map(|p| crate::state::sanitize_display(&p.display().to_string()))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

    let plan = merge::plan(&base, &current, &work, &upper);

    if args.dry_run {
        if args.json {
            println!("{}", serde_json::to_string_pretty(&plan)?);
        } else {
            print_plan(&plan);
        }
        return Ok(if plan.is_clean() { 0 } else { 3 });
    }

    if !plan.is_clean() {
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "status": "conflict",
                    "conflicts": plan.conflicts,
                }))?
            );
        } else {
            eprintln!(
                "merge aborted: {} conflict(s); host directory was NOT modified",
                plan.conflicts.len()
            );
            for c in &plan.conflicts {
                eprintln!(
                    "  conflict [{}] {} (base={}, current={}, worktree={})",
                    serde_json::to_string(&c.kind)?.trim_matches('"'),
                    crate::state::sanitize_display(&c.path.display().to_string()),
                    c.base_hash.as_deref().unwrap_or("-"),
                    c.current_hash.as_deref().unwrap_or("-"),
                    c.work_hash.as_deref().unwrap_or("-"),
                );
            }
            eprintln!(
                "resolve manually, or `cowt drop {}` to discard the worktree",
                meta.id
            );
            // Round-37: a .cowt-old-* backup file (Windows two-step rename
            // crash window, round-36-06) is the PRE-apply data of a file
            // whose apply was interrupted; a permanent Delete-vs-modify
            // conflict on it is recoverable by restoring the backup.
            #[cfg(not(unix))]
            if let Ok(rd) = std::fs::read_dir(&meta.target) {
                let mut old: Vec<String> = rd
                    .flatten()
                    .filter_map(|e| {
                        let n = e.file_name().to_string_lossy().into_owned();
                        n.contains(".cowt-old-").then_some(n)
                    })
                    .collect();
                old.sort();
                if !old.is_empty() {
                    eprintln!(
                        "note: interrupted-apply backup file(s) found: {}; \
                         each is the pre-apply content of the matching file — \
                         restore with `mv <name>.cowt-old-<pid> <name>`",
                        old.join(", ")
                    );
                }
            }
        }
        return Ok(3);
    }

    // TOCTOU guard: re-verify the gates right before the write phase — a run
    // may have started while we were planning (the check above is not
    // atomic). A live mount or fresh pidfile means upper is being written.
    if State::running_pid(&dir).is_some() {
        bail!(
            "worktree '{}' started running during planning; apply aborted (nothing written)",
            meta.id
        );
    }
    if default_backend().is_mounted(&meta.target) {
        bail!(
            "{} is mounted again; apply aborted (nothing written)",
            meta.target.display()
        );
    }

    let report = merge::execute(&plan, &meta.target)?;
    // Round-24: re-check the gates AFTER the commit phase too — a `cowt run`
    // started mid-execute (during the unbounded staging phase) would be
    // writing into upper; clearing upper and advancing the baseline now
    // would destroy its writes. Refuse to finalize; the changes are on the
    // host but the worktree stays "not applied" so the run's data survives.
    if State::running_pid(&dir).is_some() || default_backend().is_mounted(&meta.target) {
        bail!(
            "worktree '{}' started running while changes were being applied; \
             NOT advancing the baseline (the running process keeps its upper layer). \
             Run `cowt apply {}` again after it exits",
            meta.id,
            meta.id
        );
    }
    // Advance the baseline: the host now matches the merged result, so the
    // next run/diff/apply iterates against THIS state rather than the stale
    // fork snapshot (fixes apply→run→apply false conflicts, silently
    // dropped deletions of previously-applied files, and revert-to-base).
    // rescan(current) reuses hashes for untouched files via stat_eq — the
    // full scan above already hashed everything; a second full scan would
    // double the I/O on large trees (round-32).
    let new_base = Manifest::rescan(&meta.target, &current)?.manifest;
    // Round-39-02: the rescan is unbounded — a `cowt run` may have started
    // after the post-execute gate. Clearing upper now would destroy its
    // early writes. Re-check immediately before the destructive reset.
    if State::running_pid(&dir).is_some() || default_backend().is_mounted(&meta.target) {
        bail!(
            "worktree '{}' started running while changes were being applied; \
             NOT advancing the baseline (the running process keeps its upper layer). \
             Run `cowt apply {}` again after it exits",
            meta.id,
            meta.id
        );
    }
    // Round-39-05: a concurrent `drop` may have renamed the state dir away
    // while we were executing. `create_dir_all(upper)` below would
    // silently RECREATE it as an empty ghost worktree (round-28-06
    // resurrection). Verify the dir is still the live worktree before the
    // destructive finalize.
    if !dir.join("meta.json").exists() {
        bail!(
            "worktree '{}' state was removed during apply (concurrent drop?); \
             NOT advancing the baseline. The changes are on the host; fork again \
             if you want to keep them",
            meta.id
        );
    }
    State::write_manifest(&dir, &new_base)?;
    // Reset the layer: applied changes now live in the host. Keeping them
    // in upper would re-display them as pending and make upper-only
    // deletions after apply unrepresentable (effective_manifest keeps base
    // entries that upper no longer has).
    let _ = fs::remove_dir_all(&upper);
    fs::create_dir_all(&upper)?;
    let mut meta = meta;
    meta.status = Status::Applied;
    State::write_meta(&dir, &meta)?;

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "status": "applied",
                "report": report,
            }))?
        );
    } else {
        println!(
            "applied: {} written, {} deleted, {} kept (host), {} converged",
            report.written, report.deleted, report.kept, report.converged
        );
        println!(
            "changes applied and baseline advanced; `cowt drop {}` to discard",
            meta.id
        );
    }
    Ok(0)
}

fn print_plan(plan: &merge::MergePlan) {
    println!(
        "dry-run: {} operation(s), {} conflict(s)",
        plan.operations.len(),
        plan.conflicts.len()
    );
    for op in &plan.operations {
        match op {
            merge::Operation::WriteFile { path, .. } => println!(
                "  write  {}",
                crate::state::sanitize_display(&path.display().to_string())
            ),
            merge::Operation::WriteSymlink { path, target } => {
                println!(
                    "  symlink {} -> {}",
                    crate::state::sanitize_display(&path.display().to_string()),
                    crate::state::sanitize_display(&target.display().to_string())
                )
            }
            merge::Operation::Mkdir { path, .. } => println!(
                "  mkdir  {}",
                crate::state::sanitize_display(&path.display().to_string())
            ),
            merge::Operation::Delete { path, .. } => println!(
                "  delete {}",
                crate::state::sanitize_display(&path.display().to_string())
            ),
        }
    }
    for c in &plan.conflicts {
        println!(
            "  CONFLICT [{}] {} (base={}, current={}, worktree={})",
            serde_json::to_string(&c.kind).unwrap().trim_matches('"'),
            crate::state::sanitize_display(&c.path.display().to_string()),
            c.base_hash.as_deref().unwrap_or("-"),
            c.current_hash.as_deref().unwrap_or("-"),
            c.work_hash.as_deref().unwrap_or("-"),
        );
    }
    for k in &plan.kept {
        println!(
            "  keep   {} (host changed, worktree untouched)",
            crate::state::sanitize_display(&k.display().to_string())
        );
    }
}
