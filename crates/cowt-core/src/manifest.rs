//! Base manifest: a metadata-only snapshot of a host directory.
//!
//! A manifest records, for every entry under the base directory, its kind,
//! size, mode, mtime and (for regular files) a BLAKE3 content hash. It never
//! copies file contents, so creating it is cheap and supports 10k+ files.
//!
//! Symlinks are recorded as first-class entries with their raw target; they
//! are never followed, which guarantees the scan cannot escape the base
//! boundary.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::error::{Error, Result};

/// Kind of a manifest entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
}

/// Metadata for a single path in the snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub kind: EntryKind,
    /// File size in bytes (0 for dirs/symlinks).
    pub size: u64,
    /// Unix permission bits (0 on platforms without them).
    pub mode: u32,
    /// Modification time, nanoseconds since UNIX epoch (best effort).
    pub mtime_ns: i128,
    /// BLAKE3 hex digest of the contents. Only present for files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// Raw symlink target. Only present for symlinks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_target: Option<PathBuf>,
}

impl Entry {
    /// Content-level equality. Used by the three-way merge to decide whether
    /// two sides carry identical content regardless of mtime noise. File
    /// permission bits count as content on unix (chmod-only changes are
    /// real changes; copy-up preserves mode, so this stays noise-free).
    pub fn content_eq(&self, other: &Entry) -> bool {
        if self.kind != other.kind {
            return false;
        }
        match self.kind {
            EntryKind::File => {
                self.size == other.size && self.hash == other.hash && {
                    #[cfg(unix)]
                    {
                        self.mode == other.mode
                    }
                    #[cfg(not(unix))]
                    {
                        true
                    }
                }
            }
            EntryKind::Dir => true,
            EntryKind::Symlink => self.link_target == other.link_target,
        }
    }

    /// Cheap metadata equality (size + mode + mtime). When this holds we skip
    /// re-hashing during rescans.
    fn stat_eq(&self, other: &Entry) -> bool {
        self.kind == other.kind
            && self.size == other.size
            && self.mode == other.mode
            && self.mtime_ns == other.mtime_ns
    }
}

/// A metadata-only snapshot of one directory tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Absolute path of the snapshotted directory.
    pub base: PathBuf,
    /// Creation time, seconds since UNIX epoch.
    pub created_epoch: u64,
    /// Relative path -> entry. Symlinks are leaf entries, never traversed.
    pub entries: BTreeMap<PathBuf, Entry>,
}

/// Outcome of a scan, including non-fatal warnings (e.g. unreadable files).
#[derive(Debug)]
pub struct ScanOutcome {
    pub manifest: Manifest,
    /// Relative paths that could not be read; they are simply absent from the
    /// manifest and reported here.
    pub warnings: Vec<(PathBuf, String)>,
}

impl Manifest {
    /// Full scan of `base`, hashing every regular file in parallel.
    pub fn scan(base: &Path) -> Result<ScanOutcome> {
        Self::scan_inner(base, None)
    }

    /// Rescan `base`, reusing hashes from `previous` when size+mode+mtime are
    /// unchanged. This keeps repeated diffs fast on large trees.
    pub fn rescan(base: &Path, previous: &Manifest) -> Result<ScanOutcome> {
        Self::scan_inner(base, Some(previous))
    }

    fn scan_inner(base: &Path, previous: Option<&Manifest>) -> Result<ScanOutcome> {
        let base = base
            .canonicalize()
            .map_err(|e| Error::io(base.to_path_buf(), e))?;
        if !base.is_dir() {
            return Err(Error::io(
                base.clone(),
                std::io::Error::new(std::io::ErrorKind::NotADirectory, "base is not a directory"),
            ));
        }

        let mut entries: BTreeMap<PathBuf, Entry> = BTreeMap::new();
        let mut warnings: Vec<(PathBuf, String)> = Vec::new();
        // Files whose contents still need hashing: (rel path, entry slot values).
        let mut to_hash: Vec<PathBuf> = Vec::new();

        let walker = WalkDir::new(&base)
            .follow_links(false)
            .sort_by_file_name()
            .into_iter();
        for item in walker {
            let dent = match item {
                Ok(d) => d,
                Err(e) => {
                    warnings.push((
                        e.path().map(|p| p.to_path_buf()).unwrap_or_default(),
                        e.to_string(),
                    ));
                    continue;
                }
            };
            let abs = dent.path();
            if abs == base {
                continue; // the root itself is implicit
            }
            let rel = match abs.strip_prefix(&base) {
                Ok(r) => r.to_path_buf(),
                Err(_) => return Err(Error::BoundaryEscape(abs.to_path_buf())),
            };
            // Defense in depth: reject any relative component that could climb
            // out of the base (walkdir never yields these when links are not
            // followed, but the invariant is load-bearing).
            if rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err(Error::BoundaryEscape(abs.to_path_buf()));
            }

            let meta = match fs::symlink_metadata(abs) {
                Ok(m) => m,
                Err(e) => {
                    warnings.push((rel.clone(), e.to_string()));
                    continue;
                }
            };

            let kind = if meta.file_type().is_symlink() {
                EntryKind::Symlink
            } else if meta.is_dir() {
                EntryKind::Dir
            } else if meta.is_file() {
                EntryKind::File
            } else {
                // Sockets / fifos / devices cannot be snapshotted; report and skip.
                warnings.push((rel.clone(), "unsupported special file".into()));
                continue;
            };

            let mtime_ns = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i128)
                .unwrap_or(0);
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::MetadataExt;
                meta.mode()
            };
            #[cfg(not(unix))]
            let mode = 0u32;

            let mut entry = Entry {
                kind,
                size: if kind == EntryKind::File {
                    meta.len()
                } else {
                    0
                },
                mode,
                mtime_ns,
                hash: None,
                link_target: None,
            };

            match kind {
                EntryKind::File => {
                    // Fast path: reuse the previous hash when stat data matches.
                    let reused = previous
                        .and_then(|p| p.entries.get(&rel))
                        .filter(|old| old.kind == EntryKind::File && old.stat_eq(&entry))
                        .and_then(|old| old.hash.clone());
                    match reused {
                        Some(h) => entry.hash = Some(h),
                        None => {
                            to_hash.push(rel.clone());
                        }
                    }
                }
                EntryKind::Symlink => match fs::read_link(abs) {
                    Ok(t) => entry.link_target = Some(t),
                    Err(e) => warnings.push((rel.clone(), e.to_string())),
                },
                EntryKind::Dir => {}
            }

            entries.insert(rel, entry);
        }

        // Parallel hashing with a bounded worker pool; sync I/O only.
        hash_files(&base, &to_hash, &mut entries, &mut warnings)?;

        let created_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        Ok(ScanOutcome {
            manifest: Manifest {
                base,
                created_epoch,
                entries,
            },
            warnings,
        })
    }

    /// Serialize to JSON.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).map_err(|e| Error::Serde(e.to_string()))
    }

    /// Deserialize from JSON.
    pub fn from_json(s: &str) -> Result<Manifest> {
        let m: Manifest =
            serde_json::from_str(s).map_err(|e| Error::CorruptManifest(e.to_string()))?;
        // A hash that is present must be a full 64-hex BLAKE3 digest. An empty
        // or truncated hash would make content_eq report phantom changes and
        // merge invent conflicts — fail loudly instead (round-21).
        for (rel, e) in &m.entries {
            if e.kind == EntryKind::File {
                if let Some(h) = &e.hash {
                    let ok = h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit());
                    if !ok {
                        return Err(Error::CorruptManifest(format!(
                            "invalid hash for {}",
                            rel.display()
                        )));
                    }
                }
            }
        }
        Ok(m)
    }

    /// Look up an entry by relative path.
    pub fn get(&self, rel: &Path) -> Option<&Entry> {
        self.entries.get(rel)
    }
}

/// Hash every file in `to_hash` using a fixed pool of worker threads.
fn hash_files(
    base: &Path,
    to_hash: &[PathBuf],
    entries: &mut BTreeMap<PathBuf, Entry>,
    warnings: &mut Vec<(PathBuf, String)>,
) -> Result<()> {
    if to_hash.is_empty() {
        return Ok(());
    }
    let workers = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(16);
    let (job_tx, job_rx) = mpsc::channel::<PathBuf>();
    let (res_tx, res_rx) = mpsc::channel::<(PathBuf, std::io::Result<String>)>();
    let job_rx = std::sync::Arc::new(std::sync::Mutex::new(job_rx));

    thread::scope(|s| {
        for _ in 0..workers.min(to_hash.len()) {
            let job_rx = std::sync::Arc::clone(&job_rx);
            let res_tx = res_tx.clone();
            let base = base.to_path_buf();
            s.spawn(move || loop {
                let rel = {
                    let lock = job_rx.lock().unwrap();
                    match lock.recv() {
                        Ok(r) => r,
                        Err(_) => return, // channel closed: no more jobs
                    }
                };
                let hash = hash_one(&base.join(&rel));
                let _ = res_tx.send((rel, hash));
            });
        }
        drop(res_tx);

        for rel in to_hash {
            // Workers only exit early if the result channel died, which cannot
            // happen here since we hold res_rx until the end of the scope.
            let _ = job_tx.send(rel.clone());
        }
        drop(job_tx);

        for (rel, hash) in res_rx.iter() {
            match hash {
                Ok(h) => {
                    if let Some(e) = entries.get_mut(&rel) {
                        e.hash = Some(h);
                    }
                }
                Err(e) => warnings.push((rel, e.to_string())),
            }
        }
    });
    Ok(())
}

/// Streaming BLAKE3 hash of a single file. Returns lowercase hex.
fn hash_one(path: &Path) -> std::io::Result<String> {
    let mut f = fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}
