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
    /// Create a directory. `mode` is the unix permission bits to apply
    /// after creation (0 on platforms without them; round-30).
    Mkdir { path: PathBuf, mode: u32 },
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
    /// Host (current) entries at plan time, keyed by path. `execute` uses
    /// them to detect host edits landing between planning and execution
    /// (TOCTOU) so a stale plan never overwrites fresh host data
    /// (round-24).
    #[serde(skip)]
    pub expected_current: std::collections::BTreeMap<PathBuf, Entry>,
    /// Directories that ended up empty because every entry under them was
    /// deleted AND the directory itself is absent from the realized `work`
    /// tree. `execute` removes exactly these, deepest-first, so deleting a
    /// *file* never drops a parent directory that overlayfs still keeps as
    /// an empty dir (adversarial A1: base={a/a,b}, delete a/a must not leave
    /// a phantom `Deleted:a` after re-diff).
    #[serde(skip)]
    pub empty_parents: std::collections::BTreeSet<PathBuf>,
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
    // Base-key membership set for host_only_entries: precomputed once so
    // the per-deleted-dir scans stay O(n + d·depth) instead of O(d·n)
    // (round-32).
    let base_keys: std::collections::HashSet<&PathBuf> = base.entries.keys().collect();

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
                            // A dir->file / dir->symlink migration with host
                            // content unknown to base cannot be applied: the
                            // old dir would have to be removed while holding
                            // the host's own files — surface a conflict
                            // instead of a destructive failed apply
                            // (round-25, mirrors the w=None branch below).
                            let dir_to_non_dir =
                                b_entry.kind == EntryKind::Dir && entry.kind != EntryKind::Dir;
                            if dir_to_non_dir {
                                let host_only = host_only_entries(&base_keys, current, &path);
                                if !host_only.is_empty() {
                                    push_host_only_conflicts(&mut out, current, host_only);
                                    continue;
                                }
                            }
                            out.operations.push(Operation::Delete {
                                path: path.clone(),
                                migration: true,
                            });
                        }
                    }
                    out.operations.push(write_op(&path, entry, work_root));
                }
                None => {
                    // Deleting a directory whose host content includes files
                    // unknown to base (added by the host after fork) cannot
                    // be applied: execute's conservative non-empty rule
                    // would silently skip the delete, apply would report
                    // success and advance the baseline — the deletion
                    // intent lost forever. Surface a conflict instead
                    // (round-24).
                    let dir_delete = matches!(b, Some(e) if e.kind == EntryKind::Dir);
                    if dir_delete {
                        let host_only = host_only_entries(&base_keys, current, &path);
                        if !host_only.is_empty() {
                            push_host_only_conflicts(&mut out, current, host_only);
                            continue;
                        }
                    }
                    out.operations.push(Operation::Delete {
                        path,
                        migration: false,
                    });
                }
            }
            continue;
        }
        if c_eq_w {
            // Both sides ended up identical AND still present (a non-None entry
            // on both). A path deleted on BOTH sides is a no-op (nothing
            // converged — it simply does not exist), not convergence; counting
            // it would inflate the "N converged" summary and mislead the user
            // into thinking a change was applied (round-2 fuzz: base={a},
            // current={}, work={} must NOT report a as converged).
            if c.is_some() && w.is_some() {
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
    // Snapshot the host entries for every path the plan will touch, so
    // execute can detect host edits made after planning (round-24 TOCTOU).
    for op in &out.operations {
        let path = match op {
            Operation::WriteFile { path, .. }
            | Operation::WriteSymlink { path, .. }
            | Operation::Mkdir { path, .. }
            | Operation::Delete { path, .. } => path,
        };
        if let Some(e) = current.entries.get(path) {
            out.expected_current.insert(path.clone(), e.clone());
        }
    }
    // Empty-parent pruning (execute's final phase): a directory may be
    // removed only when it is absent from the realized WORK tree. Deleting a
    // *file* must NOT delete its parent directory if that directory survives
    // in `work` as an (empty) directory — overlayfs keeps it and re-diffing
    // the realized view must not report a phantom Deleted for it (A1 of the
    // adversarial diff-algebra audit: base={a/a,b}, delete a/a left a
    // phantom `Deleted:a` because execute unconditionally pruned every
    // parent of a Delete op, dropping the legitimate empty dir `a`).
    let mut empty_parents: BTreeSet<PathBuf> = BTreeSet::new();
    for op in &out.operations {
        if let Operation::Delete { path, .. } = op {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty()
                    && base.entries.contains_key(parent)
                    && !work.entries.contains_key(parent)
                {
                    empty_parents.insert(parent.to_path_buf());
                }
            }
        }
    }
    out.empty_parents = empty_parents;
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

/// Current (host) entries strictly below `dir` that are absent from base —
/// the host created them after the fork. Shared by the pure-delete branch
/// and the dir->file/symlink migration branch (round-24/25).
fn host_only_entries<'a>(
    base_keys: &std::collections::HashSet<&'a PathBuf>,
    current: &'a Manifest,
    dir: &Path,
) -> Vec<&'a PathBuf> {
    current
        .entries
        .keys()
        .filter(|p| p.starts_with(dir) && **p != *dir && !base_keys.contains(*p))
        .collect()
}

fn push_host_only_conflicts(out: &mut MergePlan, current: &Manifest, host_only: Vec<&PathBuf>) {
    for p in host_only {
        out.conflicts.push(Conflict {
            path: p.clone(),
            kind: ConflictKind::ModifyVsDelete,
            base_hash: None,
            current_hash: current.entries.get(p).and_then(hash_of),
            work_hash: None,
        });
    }
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
            mode: entry.mode,
        },
    }
}

/// Sort key: (depth, kind-order, path). Mkdir=0, writes=1, deletes=2 so that
/// directories exist before their files and files vanish before their dirs.
/// Kind-migration deletes (order 0) run first so the old entry is gone
/// before the new kind is created at the same path.
fn op_sort_key(op: &Operation) -> (usize, u8, PathBuf) {
    let (path, order) = match op {
        Operation::Mkdir { path, .. } => (path, 0u8),
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
    // Round-40 review: a marker file makes the staging dir recognizable —
    // the sweep (state.rs) must never delete a USER directory that merely
    // matches the `.cowt-apply-<deadpid>-*` name pattern in user-owned
    // space. Best-effort: a missing marker only loses crash-residue
    // sweeping, never correctness.
    let _ = fs::write(staging.join(".cowt-staging"), b"");

    let result = execute_inner(plan_result, target_root, &staging);
    // Best-effort cleanup of the staging area; it is empty on success.
    let _ = fs::remove_dir_all(&staging);
    result
}

/// Case-fold equality of two relative paths, COMPONENT-wise (round-40-01):
/// string-level to_lowercase() is separator-sensitive on Windows, where a
/// Linux-created manifest key ("dir/Foo.txt") and a Windows scan key
/// ("dir\Foo.txt") denote the same file but fold to different strings.
/// Comparing folded component sequences matches Path's own
/// separator-insensitive equality. The joined form doubles as a cheap map
/// key (O(n log n) collision detection; a Vec+any scan was O(n²) and
/// turned 10k-file diffs into 44s on Windows — round-40 CI).
pub fn case_fold_key(p: &Path) -> String {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect::<Vec<_>>()
        .join("\u{0}")
}

/// Case-fold equality of two relative paths, component-wise (see
/// [`case_fold_key`]).
pub fn case_fold_path_eq(a: &Path, b: &Path) -> bool {
    case_fold_key(a) == case_fold_key(b)
}

/// Case-fold collisions in `work` against `base`/`current`, for
/// case-insensitive hosts (NTFS, default APFS): a worktree-added `foo.txt`
/// next to a base `Foo.txt` is physically ONE file there. The byte-exact
/// plan would emit a WriteFile whose Windows verify_unchanged then
/// misreports a TOCTOU "path appeared on the host" — a permanent, wrongly
/// diagnosed apply deadlock (round-38-02). Returns the colliding work
/// paths; the caller refuses with an explicit message instead of planning.
pub fn case_fold_conflicts(base: &Manifest, work: &Manifest, current: &Manifest) -> Vec<PathBuf> {
    let mut seen: std::collections::BTreeMap<String, PathBuf> = Default::default();
    let base_current: std::collections::BTreeMap<String, PathBuf> = base
        .entries
        .keys()
        .chain(current.entries.keys())
        .map(|p| (case_fold_key(p), p.clone()))
        .collect();
    let mut conflicts: Vec<PathBuf> = Vec::new();
    for p in work.entries.keys() {
        let k = case_fold_key(p);
        // Collision within work itself (two keys differing by case only —
        // a cross-platform manifest, round-38-04).
        if let Some(other) = seen.insert(k.clone(), p.clone()) {
            conflicts.push(other);
            conflicts.push(p.clone());
        }
        if let Some(bp) = base_current.get(&k) {
            if bp != p
                // Only a real collision when the other spelling SURVIVES
                // in the worktree: a case-different recreate (delete
                // cache.bin + add CACHE.BIN) is a rename-in-place on NTFS
                // and must apply cleanly (e2e_case_recreate).
                && work.entries.contains_key(bp)
            {
                conflicts.push(p.clone());
            }
        }
    }
    conflicts.sort();
    conflicts.dedup();
    conflicts
}

fn execute_inner(plan: &MergePlan, target_root: &Path, staging: &Path) -> Result<ApplyReport> {
    let mut report = ApplyReport {
        kept: plan.kept.len(),
        converged: plan.converged.len(),
        ..ApplyReport::default()
    };
    let mut staged: Vec<(PathBuf, PathBuf, PathBuf)> = Vec::new(); // (staged file, final dest, rel path)

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
            // read-only handle with ERROR_ACCESS_DENIED. The staged copy
            // inherits a read-only source's mode/attribute via fs::copy, so
            // grant write access for the fsync and restore the permissions
            // afterwards (round-24: a read-only worktree file must not make
            // the whole apply fail).
            #[cfg(unix)]
            let staged_perms = {
                use std::os::unix::fs::PermissionsExt;
                let p = fs::metadata(&staged_file)
                    .map(|m| m.permissions())
                    .map_err(|e| Error::io(staged_file.clone(), e))?;
                let _ =
                    fs::set_permissions(&staged_file, fs::Permissions::from_mode(p.mode() | 0o200));
                p
            };
            #[cfg(not(unix))]
            let staged_perms = {
                let p = fs::metadata(&staged_file)
                    .map(|m| m.permissions())
                    .map_err(|e| Error::io(staged_file.clone(), e))?;
                if p.readonly() {
                    let mut w = p.clone();
                    // Deliberate: temporary write access for the fsync only;
                    // the original permissions are restored right below and
                    // the final file keeps them (round-24).
                    #[allow(clippy::permissions_set_readonly_false)]
                    w.set_readonly(false);
                    fs::set_permissions(&staged_file, w)
                        .map_err(|e| Error::io(staged_file.clone(), e))?;
                }
                p
            };
            let f = fs::OpenOptions::new()
                .write(true)
                .open(&staged_file)
                .map_err(|e| Error::io(source.clone(), e))?;
            f.sync_all().map_err(|e| Error::io(source.clone(), e))?;
            drop(f);
            fs::set_permissions(&staged_file, staged_perms)
                .map_err(|e| Error::io(staged_file.clone(), e))?;
            staged.push((staged_file, dest, path.clone()));
        }
    }

    // Phase 2: commit. Deletions run BEFORE renames so that a delete+write
    // pair that resolves to the same file on a case-insensitive volume
    // (delete `cache.bin`, recreate `CACHE.BIN`) does not remove the freshly
    // written file. Staged bodies are independent of the host, so deleting
    // first cannot lose data; rename's create_dir_all covers missing parents.
    for op in &plan.operations {
        if let Operation::Mkdir { path, mode } = op {
            #[cfg(not(unix))]
            let _ = mode;
            verify_unchanged(plan, target_root, path)?;
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
            // Restore the worktree's permission bits: create_dir_all applies
            // the umask, which would widen a private (0700) dir to 0755
            // (round-30). Mode 0 means "no info" on this platform.
            #[cfg(unix)]
            let mode = *mode;
            #[cfg(unix)]
            if mode != 0 {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&dest, fs::Permissions::from_mode(mode))
                    .map_err(|e| Error::io(dest.clone(), e))?;
            }
            report.written += 1;
        }
    }
    for op in &plan.operations {
        if let Operation::Delete { path, migration } = op {
            let dest = target_root.join(path);
            let meta = fs::symlink_metadata(&dest);
            match meta {
                Ok(m) if m.is_dir() => {
                    // Kind-migration deletes (file->dir): the Mkdir phase
                    // already replaced the old file with the new dir — do
                    // NOT remove it again (round-24, was deleting the
                    // freshly created empty dir and losing the intent).
                    if *migration {
                        continue;
                    }
                    verify_unchanged(plan, target_root, path)?;
                    // A host-created file inside means the dir changed after
                    // planning — the deletion intent must NOT be silently
                    // dropped (round-39-03: verify_unchanged only checks the
                    // dir itself, so a new child is invisible). Fail loudly
                    // instead of proceeding and losing the intent.
                    if fs::remove_dir(&dest).is_err() {
                        return Err(Error::io(
                            dest.clone(),
                            std::io::Error::other(format!(
                                "directory {} is not empty after planning; \
                                 a new file appeared inside it (TOCTOU). \
                                 Aborting so the deletion intent is not lost",
                                dest.display()
                            )),
                        ));
                    }
                    report.deleted += 1;
                }
                Ok(_) => {
                    verify_unchanged(plan, target_root, path)?;
                    fs::remove_file(&dest).map_err(|e| Error::io(dest.clone(), e))?;
                    report.deleted += 1;
                }
                Err(_) => {} // already gone
            }
        }
    }
    for (staged_file, dest, rel) in &staged {
        verify_unchanged(plan, target_root, rel)?;
        if let Some(p) = dest.parent() {
            fs::create_dir_all(p).map_err(|e| Error::io(p.to_path_buf(), e))?;
        }
        commit_rename(staged_file, dest)?;
        report.written += 1;
    }
    for op in &plan.operations {
        if let Operation::WriteSymlink { path, target } = op {
            verify_unchanged(plan, target_root, path)?;
            let dest = target_root.join(path);
            write_symlink(target, &dest)?;
            report.written += 1;
        }
    }
    // Prune directories left empty by deletions (deepest first), but never the
    // target root itself. Only directories that the planner marked empty
    // (absent from the realized `work` tree) are removed — a deleted *file*
    // never drags a still-kept empty directory down with it (adversarial A1).
    let mut dirs: Vec<PathBuf> = plan.empty_parents.iter().cloned().collect();
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
    // Kind migration dir->symlink: the old entry is a directory whose
    // children were already deleted by the migration Delete — remove the
    // (now empty) dir, mirroring commit_rename. Anything else (file or
    // symlink) is simply replaced (round-25).
    match fs::symlink_metadata(dest) {
        Ok(m) if m.is_dir() => {
            fs::remove_dir(dest).map_err(|e| Error::io(dest.to_path_buf(), e))?;
        }
        Ok(_) => {
            let _ = fs::remove_file(dest); // replace existing link/file
        }
        Err(_) => {}
    }
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

/// BLAKE3 content hash of one regular file (round-39-04 verify re-check).
/// `None` on any I/O error — a readable file is expected here, so a read
/// failure is treated as "cannot verify" (abort, not trust).
fn hash_file(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut f = fs::File::open(path).ok()?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(hasher.finalize().to_hex().to_string())
}

/// Round-24 TOCTOU guard: verify the on-disk path still matches what the
/// planner observed in `current`. A host edit landing between planning and
/// execution must abort the apply (never silently overwrite the fresh host
/// data); afterwards apply would re-scan and re-plan, converging cleanly.
fn verify_unchanged(plan: &MergePlan, target_root: &Path, rel: &Path) -> Result<()> {
    use std::time::UNIX_EPOCH;
    let dest = target_root.join(rel);
    let expected = match plan.expected_current.get(rel) {
        Some(e) => e,
        None => {
            // Not present in the plan-time snapshot: it must not exist now.
            if fs::symlink_metadata(&dest).is_ok() {
                return Err(Error::io(
                    dest,
                    std::io::Error::other(
                        "path appeared on the host after planning; aborting to avoid overwriting it",
                    ),
                ));
            }
            return Ok(());
        }
    };
    let meta = match fs::symlink_metadata(&dest) {
        Ok(m) => m,
        Err(_) => {
            return Err(Error::io(
                dest,
                std::io::Error::other("path disappeared from the host after planning; aborting"),
            ))
        }
    };
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0);
    let unchanged = match expected.kind {
        EntryKind::File => {
            // mode counts as content (content_eq, round-15); a host chmod in
            // the plan->execute window must abort like any other change
            // (round-30).
            let mode_ok = {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    meta.mode() == expected.mode
                }
                #[cfg(not(unix))]
                {
                    true
                }
            };
            meta.is_file()
                && mode_ok
                && meta.len() == expected.size
                && mtime_ns == expected.mtime_ns
                // Round-39-04: size+mtime can be forged (cp -p, touch -r)
                // and on case-insensitive hosts the path may even resolve
                // to a DIFFERENT file than planned (case-variant sibling,
                // NTFS/APFS). When the plan snapshot carries a content
                // hash, verify it — a mismatched file must abort the
                // apply instead of being silently deleted/overwritten.
                && expected.hash.as_deref().is_none_or(|h| {
                    hash_file(&dest).map(|actual| actual == *h).unwrap_or(false)
                })
        }
        EntryKind::Dir => meta.is_dir(),
        EntryKind::Symlink => {
            // Check the target too: a retargeted link would be silently
            // overwritten otherwise (round-27, mirrors the File branch).
            meta.file_type().is_symlink()
                && fs::read_link(&dest).ok().as_deref() == expected.link_target.as_deref()
        }
    };
    if !unchanged {
        return Err(Error::io(
            dest,
            std::io::Error::other(
                "host path changed after planning; aborting to avoid overwriting it",
            ),
        ));
    }
    Ok(())
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
    // Windows MoveFile refuses to overwrite an existing destination. Blindly
    // removing it first would lose the old file if the subsequent rename
    // fails (and retry would then hit a DeleteVsModify conflict). Move the
    // old file aside instead and restore it on failure (round-24).
    let backup = dest.with_extension(format!("cowt-old-{}", std::process::id()));
    let had_old = fs::symlink_metadata(dest).is_ok();
    if had_old {
        fs::rename(dest, &backup).map_err(|e| Error::io(dest.to_path_buf(), e))?;
    }
    match fs::rename(staged, dest) {
        Ok(()) => {
            if had_old {
                let _ = fs::remove_file(&backup);
            }
            Ok(())
        }
        Err(e) => {
            if had_old {
                let _ = fs::rename(&backup, dest); // restore the old file
            }
            Err(Error::io(dest.to_path_buf(), e))
        }
    }
}
