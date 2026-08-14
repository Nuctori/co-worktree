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
use crate::manifest::{Entry, Manifest};

const WHITEOUT_PREFIX: &str = ".wh.";
const OPAQUE_MARKER: &str = ".wh..wh..opq";

/// Fold `upper` into `base`, producing the effective worktree manifest.
///
/// `upper` must be the overlayfs upper directory (not the merged view).
pub fn effective_manifest(base: &Manifest, upper: &Path) -> Result<Manifest> {
    let mut entries: BTreeMap<PathBuf, Entry> = base.entries.clone();

    // Scan the upper layer itself. Special overlayfs artifacts (whiteouts)
    // are character devices; Manifest::scan skips special files with a
    // warning, so we handle them separately by walking raw metadata.
    let scan = Manifest::scan(upper)?.manifest;

    // 1. Whiteouts & opaque markers (character devices, invisible to scan).
    let mut deleted: Vec<PathBuf> = Vec::new();
    let mut opaque_dirs: Vec<PathBuf> = Vec::new();
    collect_whiteouts(upper, upper, &mut deleted, &mut opaque_dirs);

    // Opaque dirs: every base entry strictly below the dir is shadowed unless
    // re-created in upper. Apply before whiteouts so explicit re-adds win.
    for dir in &opaque_dirs {
        let prefix = dir.clone();
        let upper_paths: Vec<PathBuf> = scan
            .entries
            .keys()
            .filter(|p| p.starts_with(&prefix))
            .cloned()
            .collect();
        entries.retain(|p, _| {
            if p.starts_with(&prefix) && !upper_paths.iter().any(|u| u == p) {
                // Shadowed by the opaque marker unless explicitly re-added.
                false
            } else {
                true
            }
        });
    }

    // 2. Explicit whiteouts delete the named sibling.
    for d in deleted {
        entries.remove(&d);
    }

    // 3. Everything present in upper overrides base.
    for (rel, entry) in scan.entries {
        // Skip overlay internal names if any slipped through.
        if rel
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with(WHITEOUT_PREFIX))
            .unwrap_or(false)
        {
            continue;
        }
        entries.insert(rel, entry);
    }

    let mut manifest = base.clone();
    manifest.entries = entries;
    Ok(manifest)
}

/// Walk `dir` looking for overlayfs whiteout character devices.
fn collect_whiteouts(
    root: &Path,
    dir: &Path,
    deleted: &mut Vec<PathBuf>,
    opaque_dirs: &mut Vec<PathBuf>,
) {
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    for item in rd.flatten() {
        let path = item.path();
        let name = item.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(WHITEOUT_PREFIX) {
            if path.is_dir() {
                collect_whiteouts(root, &path, deleted, opaque_dirs);
            }
            continue;
        }
        if !is_whiteout(&path) {
            continue;
        }
        let rel = match path.strip_prefix(root) {
            Ok(r) => r.to_path_buf(),
            Err(_) => continue,
        };
        if name == OPAQUE_MARKER {
            if let Some(parent) = rel.parent() {
                opaque_dirs.push(parent.to_path_buf());
            }
        } else {
            let victim = rel.with_file_name(&name[WHITEOUT_PREFIX.len()..]);
            deleted.push(victim);
        }
    }
}

/// Whiteout detection. Kernel overlayfs and privileged fuse-overlayfs use
/// character devices with rdev 0:0; unprivileged fuse-overlayfs falls back to
/// zero-size regular files. The `.wh.*` namespace is reserved in upper
/// layers either way, so both encodings are accepted.
#[cfg(unix)]
fn is_whiteout(path: &Path) -> bool {
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
