//! `cowt apply <ID>` — three-way merge worktree changes into the host directory.

use anyhow::{bail, Result};
use cowt_core::{merge, overlay, Manifest};

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
    let current = Manifest::scan(&meta.target)?.manifest;
    let work = overlay::effective_manifest(&base, &upper)?;

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
            "worktree preserved for audit; `cowt drop {}` to discard",
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
            merge::Operation::Mkdir { path } => println!(
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
            c.path.display(),
            c.base_hash.as_deref().unwrap_or("-"),
            c.current_hash.as_deref().unwrap_or("-"),
            c.work_hash.as_deref().unwrap_or("-"),
        );
    }
    for k in &plan.kept {
        println!(
            "  keep   {} (host changed, worktree untouched)",
            k.display()
        );
    }
}
