//! Three-way merge (base / current / worktree) with atomic application.
//!
//! For every path we compare three signatures:
//!   * `base`    — the fork-time snapshot
//!   * `current` — the host directory as it is right now
//!   * `work`    — the effective worktree state
//!
//! Rules (per path):
//!   * work == base                    -> keep current (host may have moved on)
//!   * current == base, work != base   -> apply the worktree change
//!   * current != base, work == base   -> keep current
//!   * current == work != base         -> already converged, no-op
//!   * all three differ                -> conflict
//!
//! Application is atomic: every file body is first staged in a hidden sibling
//! directory of the target, then `rename(2)`-ed into place. If any conflict
//! exists, nothing is written at all. Note: per-file renames are atomic, but
//! the multi-file commit is not transactional — a failure mid-way leaves the
//! already-renamed files in place (each file individually consistent); the
//! staging phase still guarantees zero *partial* files on the host.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::manifest::{Entry, EntryKind, Manifest};

/// A single filesystem mutation, relative to the target root.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Operation {
    /// Copy file content from `source` (inside the worktree) to `path`.
    WriteFile { path: PathBuf, source: PathBuf },
    /// Create a symlink at `path` pointing at `target`.
    WriteSymlink { path: PathBuf, target: PathBuf },
    /// Create a directory.
    Mkdir { path: PathBuf },
    /// Delete a file, symlink or empty directory. `migration` marks a
    /// kind-migration delete (file↔dir, symlink↔file): it must run BEFORE
    /// the matching Mkdir/WriteFile of the new kind, so it sorts first.
    Delete { path: PathBuf, migration: bool },
}

/// Why a path could not be merged automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    /// Both sides modified the same path differently.
    BothModified,
    /// Host modified it, worktree deleted it.
    ModifyVsDelete,
    /// Worktree modified it, host deleted it.
    DeleteVsModify,
    /// Both sides created the same path with different content.
    BothAdded,
}

/// A structured conflict record (path, type, three signatures).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conflict {
    pub path: PathBuf,
    pub kind: ConflictKind,
    pub base_hash: Option<String>,
    pub current_hash: Option<String>,
    pub work_hash: Option<String>,
}

/// The full merge plan.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct MergePlan {
    pub operations: Vec<Operation>,
    pub conflicts: Vec<Conflict>,
    /// Paths where the host moved on its own and was kept as-is.
    pub kept: Vec<PathBuf>,
    /// Paths where both sides converged to the same content.
    pub converged: Vec<PathBuf>,
}

impl MergePlan {
    pub fn is_clean(&self) -> bool {
        self.conflicts.is_empty()
    }
}

/// Compute the three-way merge plan. `work_root` is the on-disk location of
/// the worktree tree (sources for `WriteFile` operations).
pub fn plan(base: &Manifest, current: &Manifest, work: &Manifest, work_root: &Path) -> MergePlan {
    let mut out = MergePlan::default();

    let mut paths: BTreeSet<PathBuf> = BTreeSet::new();
    paths.extend(base.entries.keys().cloned());
    paths.extend(current.entries.keys().cloned());
    paths.extend(work.entries.keys().cloned());

    for path in paths {
        let b = base.entries.get(&path);
        let c = current.entries.get(&path);
        let w = work.entries.get(&path);

        let b_eq_c = opt_eq(b, c);
        let b_eq_w = opt_eq(b, w);
        let c_eq_w = opt_eq(c, w);

        if b_eq_w {
            // Worktree left it untouched (or base never had it and worktree
            // still doesn't): whatever the host did wins.
            if !b_eq_c && b.is_some() && c.is_some() {
                out.kept.push(path);
            }
            continue;
        }
        if b_eq_c {
            // Host untouched -> the worktree change applies cleanly.
            match w {
                Some(entry) => {
                    // Kind migration (file<->dir, symlink<->file): the old
                    // entry must be deleted first or create_dir_all / rename
                    // hits the wrong kind and the change becomes un-applyable.
                    if let Some(b_entry) = b {
                        if b_entry.kind != entry.kind {
                            out.operations.push(Operation::Delete {
                                path: path.clone(),
                                migration: true,
                            });
                        }
                    }
                    out.operations.push(write_op(&path, entry, work_root));
                }
                None => out.operations.push(Operation::Delete {
                    path,
                    migration: false,
                }),
            }
            continue;
        }
        if c_eq_w {
            // Both sides ended up identical.
            if b.is_some() || c.is_some() {
                out.converged.push(path);
            }
            continue;
        }
        // Genuine divergence.
        let kind = match (b, c, w) {
            (None, Some(_), Some(_)) => ConflictKind::BothAdded,
            (Some(_), Some(_), None) => ConflictKind::ModifyVsDelete,
            (Some(_), None, Some(_)) => ConflictKind::DeleteVsModify,
            _ => ConflictKind::BothModified,
        };
        out.conflicts.push(Conflict {
            path,
            kind,
            base_hash: b.and_then(hash_of),
            current_hash: c.and_then(hash_of),
            work_hash: w.and_then(hash_of),
        });
    }

    // Deterministic application order: dirs before their contents, deletes of
    // children before deletes of their parents.
    out.operations.sort_by_key(op_sort_key);
    out.conflicts.sort_by(|x, y| x.path.cmp(&y.path));
    out
}

fn opt_eq(a: Option<&Entry>, b: Option<&Entry>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => x.content_eq(y),
        _ => false,
    }
}

fn hash_of(e: &Entry) -> Option<String> {
    e.hash.clone().or_else(|| {
        e.link_target
            .as_ref()
            .map(|t| format!("symlink:{}", t.display()))
    })
}

fn write_op(path: &Path, entry: &Entry, work_root: &Path) -> Operation {
    match entry.kind {
        EntryKind::File => Operation::WriteFile {
            path: path.to_path_buf(),
            source: work_root.join(path),
        },
        EntryKind::Symlink => Operation::WriteSymlink {
            path: path.to_path_buf(),
            target: entry.link_target.clone().unwrap_or_default(),
        },
        EntryKind::Dir => Operation::Mkdir {
            path: path.to_path_buf(),
        },
    }
}

/// Sort key: (depth, kind-order, path). Mkdir=0, writes=1, deletes=2 so that
/// directories exist before their files and files vanish before their dirs.
/// Kind-migration deletes (order 0) run first so the old entry is gone
/// before the new kind is created at the same path.
fn op_sort_key(op: &Operation) -> (usize, u8, PathBuf) {
    let (path, order) = match op {
        Operation::Mkdir { path } => (path, 0u8),
        Operation::WriteFile { path, .. } => (path, 1),
        Operation::WriteSymlink { path, .. } => (path, 1),
        Operation::Delete { path, migration } => (path, if *migration { 0 } else { 2 }),
    };
    let depth = path.components().count();
    if order == 2 {
        // Deletes: deepest first.
        (usize::MAX - depth, order, path.clone())
    } else {
        (depth, order, path.clone())
    }
}

/// Summary of a successful application.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ApplyReport {
    pub written: usize,
    pub deleted: usize,
    pub kept: usize,
    pub converged: usize,
}

/// Execute a clean plan against `target_root`. Refuses to touch anything when
/// conflicts exist.
pub fn execute(plan_result: &MergePlan, target_root: &Path) -> Result<ApplyReport> {
    if !plan_result.conflicts.is_empty() {
        return Err(Error::Conflicts(plan_result.conflicts.len()));
    }

    // Staging area on the same filesystem as the target so rename is atomic.
    let parent = target_root.parent().unwrap_or_else(|| Path::new("/"));
    let staging = parent.join(format!(
        ".cowt-apply-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&staging).map_err(|e| Error::io(staging.clone(), e))?;

    let result = execute_inner(plan_result, target_root, &staging);
    // Best-effort cleanup of the staging area; it is empty on success.
    let _ = fs::remove_dir_all(&staging);
    result
}

fn execute_inner(plan: &MergePlan, target_root: &Path, staging: &Path) -> Result<ApplyReport> {
    let mut report = ApplyReport {
        kept: plan.kept.len(),
        converged: plan.converged.len(),
        ..ApplyReport::default()
    };
    let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new(); // (staged file, final dest)

    // Phase 1: stage every file body. Any failure here leaves the target
    // completely untouched.
    for op in &plan.operations {
        if let Operation::WriteFile { path, source } = op {
            let dest = target_root.join(path);
            let staged_file = staging.join(path);
            if let Some(p) = staged_file.parent() {
                fs::create_dir_all(p).map_err(|e| Error::io(p.to_path_buf(), e))?;
            }
            fs::copy(source, &staged_file).map_err(|e| Error::io(source.clone(), e))?;
            // fsync the staged body so a crash mid-rename never yields zeros.
            // Write access is required: FlushFileBuffers on Windows rejects a
            // read-only handle with ERROR_ACCESS_DENIED.
            let f = fs::OpenOptions::new()
                .write(true)
                .open(&staged_file)
                .map_err(|e| Error::io(staged_file.clone(), e))?;
            f.sync_all()
                .map_err(|e| Error::io(staged_file.clone(), e))?;
            staged.push((staged_file, dest));
        }
    }

    // Phase 2: commit. Deletions run BEFORE renames so that a delete+write
    // pair that resolves to the same file on a case-insensitive volume
    // (delete `cache.bin`, recreate `CACHE.BIN`) does not remove the freshly
    // written file. Staged bodies are independent of the host, so deleting
    // first cannot lose data; rename's create_dir_all covers missing parents.
    for op in &plan.operations {
        if let Operation::Mkdir { path } = op {
            let dest = target_root.join(path);
            // Kind migration file->dir: the old non-directory entry must go
            // before the dir can be created (the planner's migration Delete
            // runs in the later Delete phase — handle it here).
            if let Ok(m) = fs::symlink_metadata(&dest) {
                if !m.is_dir() {
                    fs::remove_file(&dest).map_err(|e| Error::io(dest.clone(), e))?;
                }
            }
            fs::create_dir_all(&dest).map_err(|e| Error::io(dest.clone(), e))?;
            report.written += 1;
        }
    }
    for op in &plan.operations {
        if let Operation::Delete { path, .. } = op {
            let dest = target_root.join(path);
            let meta = fs::symlink_metadata(&dest);
            match meta {
                Ok(m) if m.is_dir() => {
                    // Only remove if empty; a host-created file inside means
                    // the dir is in use -> leave it (conservative).
                    if fs::remove_dir(&dest).is_ok() {
                        report.deleted += 1;
                    }
                }
                Ok(_) => {
                    fs::remove_file(&dest).map_err(|e| Error::io(dest.clone(), e))?;
                    report.deleted += 1;
                }
                Err(_) => {} // already gone
            }
        }
    }
    for (staged_file, dest) in &staged {
        if let Some(p) = dest.parent() {
            fs::create_dir_all(p).map_err(|e| Error::io(p.to_path_buf(), e))?;
        }
        commit_rename(staged_file, dest)?;
        report.written += 1;
    }
    for op in &plan.operations {
        if let Operation::WriteSymlink { path, target } = op {
            let dest = target_root.join(path);
            write_symlink(target, &dest)?;
            report.written += 1;
        }
    }
    // Prune directories left empty by deletions (deepest first), but never the
    // target root itself.
    let mut dirs: Vec<PathBuf> = plan
        .operations
        .iter()
        .filter_map(|op| match op {
            Operation::Delete { path, .. } => path.parent().map(|p| p.to_path_buf()),
            _ => None,
        })
        .collect();
    dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    dirs.dedup();
    for d in dirs {
        if d.as_os_str().is_empty() {
            continue;
        }
        let _ = fs::remove_dir(target_root.join(d)); // fails silently if non-empty
    }
    Ok(report)
}

/// Replace `dest` with a symlink pointing at `target`.
#[cfg(unix)]
fn write_symlink(target: &Path, dest: &Path) -> Result<()> {
    if let Some(p) = dest.parent() {
        fs::create_dir_all(p).map_err(|e| Error::io(p.to_path_buf(), e))?;
    }
    let _ = fs::remove_file(dest); // replace existing link/file
    std::os::unix::fs::symlink(target, dest).map_err(|e| Error::io(dest.to_path_buf(), e))
}

#[cfg(not(unix))]
fn write_symlink(_target: &Path, dest: &Path) -> Result<()> {
    Err(Error::io(
        dest.to_path_buf(),
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "symlink application is only supported on unix",
        ),
    ))
}

/// Move the staged body into place, handling a directory left at the
/// destination (kind migration dir→file whose Delete was skipped by the
/// conservative non-empty rule): a now-empty dir is removed so the rename
/// can proceed; anything else fails loudly. The platform rename itself
/// lives in `do_rename`.
fn commit_rename(staged: &Path, dest: &Path) -> Result<()> {
    if let Ok(m) = fs::symlink_metadata(dest) {
        if m.is_dir() {
            fs::remove_dir(dest).map_err(|e| Error::io(dest.to_path_buf(), e))?;
        }
    }
    do_rename(staged, dest)
}

/// Move the staged body into place. On unix `rename(2)` atomically replaces
/// the destination; Windows `MoveFile` refuses to overwrite, so the old file
/// is removed first — the commit is no longer atomic there, but the staging
/// phase still guarantees zero pollution on any pre-commit failure.
#[cfg(unix)]
fn do_rename(staged: &Path, dest: &Path) -> Result<()> {
    fs::rename(staged, dest).map_err(|e| Error::io(dest.to_path_buf(), e))
}

#[cfg(not(unix))]
fn do_rename(staged: &Path, dest: &Path) -> Result<()> {
    let _ = fs::remove_file(dest); // replace: Windows rename fails if dest exists
    fs::rename(staged, dest).map_err(|e| Error::io(dest.to_path_buf(), e))
}
