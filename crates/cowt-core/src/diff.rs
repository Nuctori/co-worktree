//! Diff engine: structural change detection plus content-level details.
//!
//! Structural diff compares two manifests (base snapshot vs. worktree state)
//! and classifies every path as added / modified / deleted. For modified
//! files the diff can be enriched with content details:
//!   * text files  -> unified line-level diff (Myers algorithm)
//!   * .json/.yaml -> key-level diff over the parsed document tree
//!   * anything else -> reported as binary

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use similar::{Algorithm, TextDiff};

use crate::error::Result;
use crate::manifest::{EntryKind, Manifest};

/// How a path changed relative to the base snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

/// Content-level detail for a modified file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ContentDiff {
    /// Unified diff text produced with the Myers algorithm.
    Text { unified: String },
    /// Key-level diff for structured documents (JSON / YAML).
    Keys { changes: Vec<KeyChange> },
    /// Binary or otherwise non-comparable content.
    Binary,
}

/// A single key-level difference inside a JSON/YAML document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyChange {
    /// Dotted key path, e.g. `server.tls.enabled` or `items[2].name`.
    pub key: String,
    pub kind: ChangeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new: Option<String>,
}

/// One changed path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Change {
    pub path: PathBuf,
    pub kind: ChangeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<ContentDiff>,
}

/// Structural diff between the base snapshot and the worktree state.
///
/// Both manifests must describe the same logical tree (typically `work` was
/// produced by scanning the merged overlay view). Directory entries that only
/// exist because their children changed are ignored.
pub fn diff(base: &Manifest, work: &Manifest) -> Vec<Change> {
    let mut out = Vec::new();

    // Added or modified: present in work.
    for (path, w) in &work.entries {
        match base.entries.get(path) {
            None => {
                if w.kind == EntryKind::Dir {
                    // Report added directories too: an explicitly created empty
                    // dir is a real change; parents of added files are filtered
                    // out below by the "has added descendant" rule... we keep
                    // them all, they are cheap and accurate.
                }
                out.push(Change {
                    path: path.clone(),
                    kind: ChangeKind::Added,
                    base_hash: None,
                    work_hash: w.hash.clone(),
                    detail: None,
                });
            }
            Some(b) => {
                if !b.content_eq(w) {
                    out.push(Change {
                        path: path.clone(),
                        kind: ChangeKind::Modified,
                        base_hash: b.hash.clone(),
                        work_hash: w.hash.clone(),
                        detail: None,
                    });
                }
            }
        }
    }

    // Deleted: present in base, absent in work.
    for (path, b) in &base.entries {
        if !work.entries.contains_key(path) {
            out.push(Change {
                path: path.clone(),
                kind: ChangeKind::Deleted,
                base_hash: b.hash.clone(),
                work_hash: None,
                detail: None,
            });
        }
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Enrich `Modified` file changes with content details (line/key level).
///
/// `base_root` and `work_root` are the on-disk locations of the two trees.
/// Failures to read or parse a file degrade gracefully to `Binary`.
pub fn enrich(base_root: &Path, work_root: &Path, changes: &mut [Change]) {
    for ch in changes.iter_mut() {
        if ch.kind != ChangeKind::Modified {
            continue;
        }
        let old = base_root.join(&ch.path);
        let new = work_root.join(&ch.path);
        ch.detail = Some(content_detail(&old, &new));
    }
}

fn content_detail(old: &Path, new: &Path) -> ContentDiff {
    // Content enrichment loads both files fully; cap it so multi-GB files
    // cannot OOM the CLI. Structural diff still reports them as modified.
    const CONTENT_LIMIT: u64 = 64 * 1024 * 1024;
    // Cross-file budget: 100 × 60MB modified files would otherwise pile up
    // ~10GB of unified-diff strings in one `cowt diff --content` run.
    const CONTENT_BUDGET: u64 = 512 * 1024 * 1024;
    static USED: AtomicU64 = AtomicU64::new(0);
    let ok_size = |p: &Path| {
        fs::metadata(p)
            .map(|m| m.len() <= CONTENT_LIMIT)
            .unwrap_or(true)
    };
    if !ok_size(old) || !ok_size(new) {
        return ContentDiff::Binary;
    }
    let new_len = fs::metadata(new).map(|m| m.len()).unwrap_or(0);
    if USED.fetch_add(new_len, Ordering::Relaxed) > CONTENT_BUDGET {
        return ContentDiff::Binary; // budget exhausted: structural diff only
    }
    let old_bytes = match fs::read(old) {
        Ok(b) => b,
        Err(_) => return ContentDiff::Binary,
    };
    let new_bytes = match fs::read(new) {
        Ok(b) => b,
        Err(_) => return ContentDiff::Binary,
    };

    // Structured documents first (by extension), falling back to plain text.
    if let Some(keys) = key_diff_by_ext(old, &old_bytes, new, &new_bytes) {
        return ContentDiff::Keys { changes: keys };
    }

    match (String::from_utf8(old_bytes), String::from_utf8(new_bytes)) {
        (Ok(o), Ok(n)) if is_text(&o) && is_text(&n) => ContentDiff::Text {
            unified: unified_diff(&o, &n),
        },
        _ => ContentDiff::Binary,
    }
}

/// Heuristic text detection: no NUL byte and no C0 control characters
/// (other than tab/CR/LF) in the first 8 KiB. Control bytes are not text;
/// treating them as such would leak raw ANSI escape sequences into the
/// terminal via the human diff output.
fn is_text(s: &str) -> bool {
    !s.as_bytes()
        .iter()
        .take(8192)
        .any(|b| *b == 0 || (*b < 0x20 && !matches!(*b, b'\t' | b'\n' | b'\r')))
}

/// Unified line diff using the Myers algorithm.
///
/// A deadline bounds the worst case: Myers is O(N×M) and two large,
/// completely distinct files (a rewritten log, a regenerated dump) would
/// otherwise spin for minutes-hours. similar degrades to a delete+add
/// approximation past the deadline.
pub fn unified_diff(old: &str, new: &str) -> String {
    TextDiff::configure()
        .algorithm(Algorithm::Myers)
        .deadline(std::time::Instant::now() + std::time::Duration::from_secs(5))
        .diff_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header("base", "worktree")
        .to_string()
}

/// If both paths carry a structured-document extension (.json/.yaml/.yml) and
/// both parse successfully, produce a key-level diff; otherwise `None`.
fn key_diff_by_ext(
    old: &Path,
    old_bytes: &[u8],
    new: &Path,
    new_bytes: &[u8],
) -> Option<Vec<KeyChange>> {
    fn doc_kind(p: &Path) -> Option<&'static str> {
        match p
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
        {
            Some(ref e) if e == "json" => Some("json"),
            Some(ref e) if e == "yaml" || e == "yml" => Some("yaml"),
            _ => None,
        }
    }
    let kind = doc_kind(old)?;
    if doc_kind(new)? != kind {
        return None;
    }
    let old_v: serde_json::Value = match kind {
        "json" => serde_json::from_slice(old_bytes).ok()?,
        _ => serde_yml::from_slice(old_bytes).ok()?,
    };
    let new_v: serde_json::Value = match kind {
        "json" => serde_json::from_slice(new_bytes).ok()?,
        _ => serde_yml::from_slice(new_bytes).ok()?,
    };
    let mut flat_old = BTreeMap::new();
    let mut flat_new = BTreeMap::new();
    flatten(&old_v, String::new(), &mut flat_old);
    flatten(&new_v, String::new(), &mut flat_new);

    let mut changes = Vec::new();
    for (k, v) in &flat_new {
        match flat_old.get(k) {
            None => changes.push(KeyChange {
                key: k.clone(),
                kind: ChangeKind::Added,
                old: None,
                new: Some(v.clone()),
            }),
            Some(o) if o != v => changes.push(KeyChange {
                key: k.clone(),
                kind: ChangeKind::Modified,
                old: Some(o.clone()),
                new: Some(v.clone()),
            }),
            _ => {}
        }
    }
    for (k, v) in &flat_old {
        if !flat_new.contains_key(k) {
            changes.push(KeyChange {
                key: k.clone(),
                kind: ChangeKind::Deleted,
                old: Some(v.clone()),
                new: None,
            });
        }
    }
    if changes.is_empty() {
        // Root-level changes that flatten cannot express as keyed entries:
        // an empty-container swap ({} vs []), or a root scalar change whose
        // rendered form differs (V3/V4). Report them under the "root" key.
        let root_old = render_scalar(&old_v);
        let root_new = render_scalar(&new_v);
        if root_old != root_new {
            changes.push(KeyChange {
                key: "root".into(),
                kind: ChangeKind::Modified,
                old: Some(root_old),
                new: Some(root_new),
            });
        }
    }
    changes.sort_by(|a, b| a.key.cmp(&b.key));
    Some(changes)
}

/// Escape a key segment so dotted paths stay unambiguous: a literal `.` or
/// `[` in a key name is backslash-escaped (rendered as `a\.b`, `a\[0]`),
/// which prevents collisions like `{"a.b":1}` vs `{"a":{"b":1}}` from
/// silently collapsing to an empty diff.
fn escape_key(k: &str) -> String {
    k.replace('.', "\\.").replace('[', "\\[")
}

/// Flatten a JSON value tree into dotted key paths.
fn flatten(v: &serde_json::Value, prefix: String, out: &mut BTreeMap<String, String>) {
    match v {
        serde_json::Value::Object(map) => {
            if map.is_empty() && !prefix.is_empty() {
                out.insert(prefix, "{}".into());
                return;
            }
            for (k, val) in map {
                let key = if prefix.is_empty() {
                    escape_key(k)
                } else {
                    format!("{prefix}.{}", escape_key(k))
                };
                flatten(val, key, out);
            }
        }
        serde_json::Value::Array(arr) => {
            if arr.is_empty() && !prefix.is_empty() {
                out.insert(prefix, "[]".into());
                return;
            }
            for (i, val) in arr.iter().enumerate() {
                flatten(val, format!("{prefix}[{i}]"), out);
            }
        }
        scalar => {
            let key = if prefix.is_empty() {
                "root".into()
            } else {
                prefix
            };
            out.insert(key, render_scalar(scalar));
        }
    }
}

/// Type-aware scalar rendering: strings are quoted so a type change
/// (`123` -> `"123"`) is visible instead of rendering identically.
fn render_scalar(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => format!("\"{s}\""),
        other => other.to_string(),
    }
}

/// Convenience: full diff between two on-disk trees (used by tests and by the
/// fallback path when no FUSE mount is available).
pub fn diff_trees(base_root: &Path, work_root: &Path) -> Result<(Manifest, Manifest, Vec<Change>)> {
    let base = Manifest::scan(base_root)?.manifest;
    let work = Manifest::scan(work_root)?.manifest;
    let mut changes = diff(&base, &work);
    enrich(base_root, work_root, &mut changes);
    Ok((base, work, changes))
}
