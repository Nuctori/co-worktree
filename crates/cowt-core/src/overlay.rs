//! Interpretation of an overlayfs-style *upper* directory.
//!
//! When a process runs under co-worktree, all writes land in an upper layer
//! maintained by fuse-overlayfs. Deletions are recorded as whiteout entries
//! (character devices `0:0` named `.wh.<name>`) and replaced directories are
//! marked opaque (`.wh..wh..opq`).
//!
//! The primary way to obtain the worktree state is to scan the *merged view*
//! of a live or freshly re-mounted overlay (see the CLI). This module is the
//! offline fallback: it folds an upper directory into the base manifest with
//! no FUSE mount required, which is also what unit tests exercise.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::manifest::{Entry, EntryKind, Manifest};

const WHITEOUT_PREFIX: &str = ".wh.";
const OPAQUE_MARKER: &str = ".wh..wh..opq";
/// Reserved namespace for winfsp/macos copy_up temp files: a crash between
/// the copy and the rename leaves `.cowt-copy-tmp.<name>` in upper; it must
/// never surface as a worktree entry (round-36).
const COPY_TMP_PREFIX: &str = ".cowt-copy-tmp.";

/// Fold `upper` into `base`, producing the effective worktree manifest.
///
/// `upper` must be the overlayfs upper directory (not the merged view).
pub fn effective_manifest(base: &Manifest, upper: &Path) -> Result<Manifest> {
    let mut entries: BTreeMap<PathBuf, Entry> = base.entries.clone();

    // A missing upper layer (kill -9 between apply's remove_dir_all and
    // create_dir_all, round-24) is semantically an empty upper — self-heal
    // instead of erroring: diff/apply can proceed, and apply recreates it.
    if !upper.is_dir() {
        eprintln!(
            "cowt: warning: upper layer {} is missing (crashed apply?); treating it as empty",
            upper.display()
        );
        return Ok(base.clone());
    }

    // Scan the upper layer itself. Special overlayfs artifacts (whiteouts)
    // are character devices; Manifest::scan skips special files with a
    // warning, so we handle them separately by walking raw metadata.
    let scan = Manifest::scan(upper)?;
    // Unreadable upper entries (e.g. another user's worktree) silently
    // collapse to "no changes" if dropped here — surface them loudly.
    for (p, why) in scan.warnings.iter().take(10) {
        eprintln!("cowt: warning: unreadable in upper: {}: {why}", p.display());
    }
    let scan = scan.manifest;

    // 1. Whiteouts & opaque markers (character devices, invisible to scan).
    let mut deleted: Vec<PathBuf> = Vec::new();
    let mut opaque_dirs: Vec<PathBuf> = Vec::new();
    collect_whiteouts(upper, upper, &mut deleted, &mut opaque_dirs);

    // Opaque dirs: every base entry strictly below the dir is shadowed unless
    // re-created in upper. Apply before whiteouts so explicit re-adds win.
    // Batch: collect all shadowed paths first, then ONE retain (round-32 —
    // per-dir retains were O(n·m) on large trees).
    let mut shadowed: Vec<PathBuf> = Vec::new();
    for dir in &opaque_dirs {
        let prefix = dir.clone();
        let upper_paths: std::collections::BTreeSet<&PathBuf> = scan
            .entries
            .keys()
            .filter(|p| p.starts_with(&prefix))
            .collect();
        for p in base.entries.keys() {
            if p.starts_with(&prefix) && !upper_paths.contains(p) {
                shadowed.push(p.clone());
            }
        }
    }
    if !shadowed.is_empty() {
        entries.retain(|p, _| !shadowed.contains(p));
    }

    // 2. Whiteouts delete the named sibling AND its whole subtree. Kernel
    //    overlayfs semantics: a directory whiteout shadows the entire lower
    //    tree. The winfsp/macos backends also collapse nested whiteouts when
    //    a parent dir is removed, so the top-level whiteout must cover all
    //    descendants (Path::starts_with is component-wise, so a file whiteout
    //    still only matches itself).
    //
    //    Batch into a single retain with a prefix set: per-whiteout retains
    //    were O(n·m) (n entries × m whiteouts) on large trees (round-32).
    if !deleted.is_empty() {
        let prefixes: std::collections::BTreeSet<PathBuf> = deleted.into_iter().collect();
        entries.retain(|p, _| {
            // A whiteout shadows the path itself or any ancestor (dir
            // whiteout covers the whole subtree). Walk the ancestor chain:
            // O(depth · log m) per entry instead of O(m) via .any()
            // (round-32).
            let mut cur = Some(p.as_path());
            while let Some(c) = cur {
                if prefixes.contains(c) {
                    return false;
                }
                cur = c.parent().filter(|_| !c.as_os_str().is_empty());
            }
            true
        });
    }

    // 3. Everything present in upper overrides base. A `.wh.`-prefixed name
    //    is skipped only when it is an *actual* whiteout (0-byte marker or
    //    char device — already folded into `deleted` above). A non-empty
    //    `.wh.x` is a plain user file and must stay visible, otherwise real
    //    changes are silently dropped (round-21). `.cowt-copy-tmp.*` is a
    //    reserved copy_up namespace (round-36): a crash between copy and
    //    rename leaves it in upper and it must never surface as an entry.
    for (rel, entry) in scan.entries {
        let name = rel.file_name().and_then(|n| n.to_str());
        let wh_name = name
            .map(|n| n.starts_with(WHITEOUT_PREFIX))
            .unwrap_or(false);
        if wh_name && is_wh_prefixed_whiteout(&upper.join(&rel)) {
            continue;
        }
        if name
            .map(|n| n.starts_with(COPY_TMP_PREFIX))
            .unwrap_or(false)
        {
            continue;
        }
        // A non-directory entry replacing a base directory shadows the whole
        // subtree: `rm -rf x && ln -s t x` leaves only the symlink in upper,
        // and x/f.txt becomes unreachable in the merged view. Keep that
        // overlayfs semantics — otherwise diff misses the deletion and apply
        // deadlocks on the non-empty dir (round-27). Only run the O(n) prune
        // when a base entry below `rel` actually exists (a plain file update
        // with no shadowed subtree stays O(1)).
        if entry.kind != EntryKind::Dir && base_has_descendant(base, &rel) {
            let prefix = rel.clone();
            entries.retain(|p, _| !p.starts_with(&prefix));
        }
        entries.insert(rel, entry);
    }

    let mut manifest = base.clone();
    manifest.entries = entries;
    Ok(manifest)
}

/// The relative paths whiteouted by `upper` (victims, not marker names).
/// Used by apply to cross-check whiteout targets against the base manifest
/// (a whiteout whose victim exists on the host but not in base means the
/// base is semantically corrupt — see apply.rs round-23 guard).
pub fn whiteout_victims(upper: &Path) -> Vec<PathBuf> {
    let mut deleted = Vec::new();
    let mut opaque_dirs = Vec::new();
    collect_whiteouts(upper, upper, &mut deleted, &mut opaque_dirs);
    deleted
}

/// True if the base has a directory at `rel` — the only case where a
/// non-dir replacement must shadow a subtree. A plain file update (or a
/// new file inside a replaced dir, which cannot exist without the dir
/// itself being replaced) matches nothing, keeping the hot path O(1).
fn base_has_descendant(base: &Manifest, rel: &Path) -> bool {
    base.entries
        .get(rel)
        .map(|e| e.kind == EntryKind::Dir)
        .unwrap_or(false)
}

/// Walk `dir` looking for overlayfs whiteouts.
///
/// Two encodings exist in the wild, and we accept both:
///   * kernel-style: a character device `0:0` that takes the *original* file
///     name (fuse-overlayfs with working mknod, kernel overlayfs);
///   * `.wh.`-prefixed: a zero-size regular file or char device named
///     `.wh.<name>` (fuse-overlayfs fallback when mknod is unavailable).
///
/// Iterative (explicit stack): recursion was unbounded in depth and held a
/// read_dir fd per level, silently missing whiteouts past the fd limit on
/// very deep trees (round-32).
fn collect_whiteouts(
    root: &Path,
    start: &Path,
    deleted: &mut Vec<PathBuf>,
    opaque_dirs: &mut Vec<PathBuf>,
) {
    let mut stack = vec![start.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            // Unreadable subdir (fd exhaustion, permissions): skip this
            // subtree silently, as before — the parent's whiteouts still
            // apply.
            Err(_) => continue,
        };
        for item in rd.flatten() {
            let path = item.path();
            let name = item.file_name();
            let Some(name) = name.to_str() else { continue };

            if name == OPAQUE_MARKER && is_whiteout(&path) {
                if let Ok(rel) = path.strip_prefix(root) {
                    if let Some(parent) = rel.parent() {
                        opaque_dirs.push(parent.to_path_buf());
                    }
                }
                continue;
            }

            if let Some(victim_name) = name.strip_prefix(WHITEOUT_PREFIX) {
                // `.wh.<name>` encoding (char device or zero-size regular file).
                if is_wh_prefixed_whiteout(&path) {
                    if let Ok(rel) = path.strip_prefix(root) {
                        deleted.push(rel.with_file_name(victim_name));
                    }
                }
                continue;
            }

            // Kernel-style encoding: char device 0:0 with the victim's own name.
            if is_whiteout(&path) {
                if let Ok(rel) = path.strip_prefix(root) {
                    deleted.push(rel.to_path_buf());
                }
                continue;
            }

            // Descend only into real directories: `is_dir()` follows
            // symlinks, and an upper-layer symlink/junction pointing at an
            // external tree (created by any process during `cowt run`) would
            // make every diff/apply walk that whole tree — a junction ring
            // crashes with a stack overflow (reproduced). Never follow
            // links here.
            if std::fs::symlink_metadata(&path)
                .map(|m| m.is_dir())
                .unwrap_or(false)
            {
                stack.push(path);
            }
        }
    }
}

/// Kernel-style whiteout: character device with rdev 0:0.
#[cfg(unix)]
fn is_whiteout(path: &Path) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_char_device() && m.rdev() == 0)
        .unwrap_or(false)
}

/// `.wh.`-prefixed whiteout: char device 0:0, or a zero-size regular file
/// (fuse-overlayfs fallback when mknod is unavailable).
#[cfg(unix)]
fn is_wh_prefixed_whiteout(path: &Path) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let Ok(m) = std::fs::symlink_metadata(path) else {
        return false;
    };
    (m.file_type().is_char_device() && m.rdev() == 0) || (m.file_type().is_file() && m.size() == 0)
}

#[cfg(not(unix))]
fn is_whiteout(_path: &Path) -> bool {
    false
}

/// Windows: no character devices; the backend encodes deletions as `.wh.<name>`
/// zero-size regular files (same convention as the fuse-overlayfs fallback).
#[cfg(not(unix))]
fn is_wh_prefixed_whiteout(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.is_file() && m.len() == 0)
        .unwrap_or(false)
}
