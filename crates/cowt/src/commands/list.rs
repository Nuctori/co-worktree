//! `cowt list` / `cowt status` / `cowt doctor`.

use anyhow::Result;
use serde_json::json;

use crate::backend::default_backend;
use crate::state::{State, Status};

pub fn list(json_out: bool) -> Result<()> {
    let state = State::open()?;
    let metas = state.list()?;
    if json_out {
        let rows: Vec<_> = metas.iter().map(|m| row_json(&state, m)).collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if metas.is_empty() {
        println!(
            "no worktrees (state root: {})",
            crate::state::sanitize_display(&state.root().display().to_string())
        );
        return Ok(());
    }
    println!(
        "{:<10} {:<20} {:<9} {:<9} TARGET",
        "ID", "NAME", "STATUS", "BACKEND"
    );
    for m in &metas {
        let status = effective_status(&state, m);
        println!(
            "{:<10} {:<20} {:<9} {:<9} {}",
            crate::state::sanitize_display(&m.id),
            crate::state::sanitize_display(&m.name.clone().unwrap_or_else(|| "-".into())),
            status,
            crate::state::sanitize_display(&m.backend),
            crate::state::sanitize_display(&m.target.display().to_string())
        );
    }
    Ok(())
}

pub fn status(id: &str, json_out: bool) -> Result<()> {
    let state = State::open()?;
    let dir = state.resolve(id)?;
    let meta = State::load_meta(&dir)?;
    let running = State::running_pid(&dir);
    // Round-37: a corrupt/unreadable manifest.json must not be hidden —
    // status previously reported "ready" rc=0 while diff/apply hit a bare
    // parse error. Reading it here makes the damage visible up front.
    let manifest_ok = State::load_manifest(&dir).is_ok();
    if !manifest_ok {
        eprintln!(
            "cowt: warning: manifest.json for worktree {} is corrupted or unreadable; \
             diff/apply will refuse to run. Restore the manifest from a backup, or \
             `cowt drop {} --force` to discard the worktree",
            crate::state::sanitize_display(&meta.id),
            crate::state::sanitize_display(&meta.id)
        );
    }
    // Round-37: a missing target must be visible here (it is silently
    // invisible to `diff` otherwise).
    let target_missing = !meta.target.is_dir();
    if target_missing {
        eprintln!(
            "cowt: warning: target directory {} does not exist; \
             if a crashed run moved it aside, run any command to restore it, \
             or check {}",
            crate::state::sanitize_display(&meta.target.display().to_string()),
            crate::state::sanitize_display(&dir.join("real").display().to_string())
        );
    }
    // Distinguish "upper is genuinely empty" from "upper is broken or
    // unreadable": a 0-byte lie would hide corruption from scripts
    // (round-33).
    let upper_missing = !dir.join("upper").exists();
    let upper_size = match dir_size(&dir.join("upper")) {
        Ok(sz) => Some(sz),
        Err(e) => {
            eprintln!("cowt: warning: cannot measure upper layer: {e:#}",);
            None
        }
    };
    let status = effective_status(&state, &meta);
    let info = json!({
        "id": meta.id,
        "name": meta.name,
        "target": meta.target,
        "created_epoch": meta.created_epoch,
        "status": status,
        "backend": meta.backend,
        "running_pid": running,
        "upper_bytes": upper_size,
        "upper_missing": upper_missing,
        "manifest_ok": manifest_ok,
        "target_missing": target_missing,
        "state_dir": dir,
    });
    if json_out {
        println!("{}", serde_json::to_string_pretty(&info)?);
    } else {
        println!("id:        {}", crate::state::sanitize_display(&meta.id));
        if let Some(n) = &meta.name {
            println!("name:      {}", crate::state::sanitize_display(n));
        }
        println!(
            "target:    {}",
            crate::state::sanitize_display(&meta.target.display().to_string())
        );
        println!(
            "created:   {}",
            crate::state::sanitize_display(&meta.created_epoch.to_string())
        );
        println!("status:    {}", status);
        println!(
            "backend:   {}",
            crate::state::sanitize_display(&meta.backend)
        );
        if let Some(pid) = running {
            println!("running:   pid {pid}");
        }
        match upper_size {
            // Round-37: a MISSING upper (crashed apply, external delete) is
            // not "0 bytes" — the run recreates it, but the user should see
            // that a crash happened.
            Some(_sz) if upper_missing => {
                println!("upper:     MISSING (will be recreated on next run)")
            }
            Some(sz) => println!("upper:     {sz} bytes of isolated data"),
            None => println!("upper:     unknown (unreadable)"),
        }
        println!(
            "state:     {}",
            crate::state::sanitize_display(&dir.display().to_string())
        );
    }
    Ok(())
}

/// `cowt doctor` — report backend availability and installation health;
/// used by CI diagnostics. Round-37: expanded from backend+state-root to a
/// per-worktree health scan (corrupt/missing meta, corrupt manifest,
/// missing target, stranded real, pidfile state) plus residue detection
/// (.trash-*, *.json.tmp-*, .cowt-apply-*, .cowt-copy-tmp.*). Contract
/// (round-33): always exits 0 and prints the three header lines first, so
/// scripts keying on them keep working.
pub fn doctor() -> Result<()> {
    let backend = default_backend();
    println!("backend:   {}", backend.name());
    match backend.available() {
        Ok(()) => println!("available: yes"),
        Err(e) => println!("available: NO ({e:#})"),
    }
    let state = State::open()?;
    println!(
        "state:     {}",
        crate::state::sanitize_display(&state.root().display().to_string())
    );

    // ---- per-worktree health (round-37) ----
    let metas = state.list()?; // warns on corrupt/missing meta itself
    if metas.is_empty() {
        println!("worktrees: 0 (nothing to check)");
    } else {
        println!("worktrees: {} found", metas.len());
        for m in &metas {
            let issues = doctor_worktree_issues(&state, m);
            if issues.is_empty() {
                println!("  {}: ok", crate::state::sanitize_display(&m.id));
            } else {
                println!(
                    "  {}: WARN ({})",
                    crate::state::sanitize_display(&m.id),
                    issues.join("; ")
                );
            }
        }
    }

    // ---- residue scan (round-37) ----
    let residue = doctor_residue(&state, &metas);
    if residue.is_empty() {
        println!("residue:   none");
    } else {
        for r in &residue {
            println!("residue:   {r}");
        }
    }
    Ok(())
}

/// Per-worktree health issues reported by `cowt doctor` (round-37).
fn doctor_worktree_issues(state: &State, m: &crate::state::WorktreeMeta) -> Vec<&'static str> {
    let dir = state.dir(&m.id);
    let mut issues: Vec<&'static str> = Vec::new();
    if State::load_manifest(&dir).is_err() {
        issues.push("manifest corrupted");
    }
    if !m.target.is_dir() {
        issues.push(if dir.join("real").exists() {
            "target missing but real/ present (crash strand; auto-restorable)"
        } else {
            "target missing (externally deleted?)"
        });
    }
    match State::running_pid(&dir) {
        Some(_) => issues.push("running"),
        None => {
            if State::stale_run(&dir) {
                issues.push("stale pidfile (self-heals on next run)");
            }
        }
    }
    if !dir.join("upper").exists() {
        issues.push("upper missing (recreated on next run)");
    }
    issues
}

/// Residue scan for `cowt doctor` (round-37): leftovers that self-heal on
/// the next operation are reported with that fact; nothing is deleted.
fn doctor_residue(state: &State, metas: &[crate::state::WorktreeMeta]) -> Vec<String> {
    let mut residue: Vec<String> = Vec::new();
    if let Ok(rd) = fs::read_dir(state.root()) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with(".trash-") {
                residue.push(format!(
                    "{name} (leftover drop; cleaned by the next drop, or manually if it holds no real/)"
                ))
            } else if name.contains("json.tmp-") {
                residue.push(format!("{name} (crashed write; cleaned by the next write)"));
            }
        }
    }
    // .cowt-apply-* staging dirs live next to the TARGET, not in state.
    for m in metas {
        if let Some(parent) = m.target.parent() {
            if let Ok(rd) = fs::read_dir(parent) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if name.starts_with(".cowt-apply-") {
                        residue.push(format!(
                            "{name} (crashed apply staging; cleaned by the next apply)"
                        ));
                    }
                }
            }
        }
        // .cowt-copy-tmp.* residues live in upper dirs.
        let upper = state.dir(&m.id).join("upper");
        if let Ok(rd) = fs::read_dir(&upper) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with(".cowt-copy-tmp.") {
                    residue.push(format!(
                        "{name} (crashed copy-up; ignored by diff, safe to delete)"
                    ));
                }
            }
        }
    }
    residue
}

fn effective_status(state: &State, meta: &crate::state::WorktreeMeta) -> String {
    let dir = state.dir(&meta.id);
    if State::running_pid(&dir).is_some() {
        "running".into()
    } else {
        match meta.status {
            Status::Ready => "ready".into(),
            Status::Applied => "applied".into(),
        }
    }
}

fn row_json(state: &State, m: &crate::state::WorktreeMeta) -> serde_json::Value {
    json!({
        "id": m.id,
        "name": m.name,
        "target": m.target,
        "status": effective_status(state, m),
        "backend": m.backend,
        "created_epoch": m.created_epoch,
    })
}

fn dir_size(path: &std::path::Path) -> std::io::Result<u64> {
    let mut total = 0;
    if !path.exists() {
        return Ok(0);
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        // symlink_metadata (no follow): a symlink pointing at an ancestor
        // (self-ring) or an external tree must not be traversed — that
        // would stack-overflow or count foreign data (round-32, mirrors
        // the R10 collect_whiteouts fix).
        let meta = fs::symlink_metadata(entry.path())?;
        if meta.is_dir() {
            total += dir_size(&entry.path())?;
        } else {
            total += meta.len();
        }
    }
    Ok(total)
}

use std::fs;
