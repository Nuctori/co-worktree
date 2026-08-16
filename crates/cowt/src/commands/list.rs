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
    // Distinguish "upper is genuinely empty" from "upper is broken or
    // unreadable": a 0-byte lie would hide corruption from scripts
    // (round-33).
    let upper_size = match dir_size(&dir.join("upper")) {
        Ok(sz) => Some(sz),
        Err(e) => {
            eprintln!("cowt: warning: cannot measure upper layer: {e:#}",);
            None
        }
    };
    let info = json!({
        "id": meta.id,
        "name": meta.name,
        "target": meta.target,
        "created_epoch": meta.created_epoch,
        "status": effective_status(&state, &meta),
        "backend": meta.backend,
        "running_pid": running,
        "upper_bytes": upper_size,
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
        println!("status:    {}", effective_status(&state, &meta));
        println!(
            "backend:   {}",
            crate::state::sanitize_display(&meta.backend)
        );
        if let Some(pid) = running {
            println!("running:   pid {pid}");
        }
        match upper_size {
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

/// `cowt doctor` — report backend availability; used by CI diagnostics.
pub fn doctor() -> Result<()> {
    let backend = default_backend();
    println!("backend:   {}", backend.name());
    match backend.available() {
        Ok(()) => println!("available: yes"),
        Err(e) => println!("available: NO ({e:#})"),
    }
    println!(
        "state:     {}",
        crate::state::sanitize_display(&State::open()?.root().display().to_string())
    );
    Ok(())
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
