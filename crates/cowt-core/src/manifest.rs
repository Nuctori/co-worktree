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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    #[default]
    File,
    Dir,
    Symlink,
}

/// Metadata for a single path in the snapshot.
///
/// ALL fields carry `#[serde(default)]`: adding a field to this struct must
/// not make every pre-existing manifest unreadable — a new field without a
/// default is a forward-compat break (round-34). New fields MUST follow
/// this pattern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    #[serde(default)]
    pub kind: EntryKind,
    /// File size in bytes (0 for dirs/symlinks).
    #[serde(default)]
    pub size: u64,
    /// Unix permission bits (0 on platforms without them).
    #[serde(default)]
    pub mode: u32,
    /// Modification time, nanoseconds since UNIX epoch (best effort).
    #[serde(default)]
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
            EntryKind::Dir => {
                // Directory permission bits count as content on unix too:
                // a chmod-only change on a directory must be visible to
                // diff and restored by apply (round-30, mirrors File).
                #[cfg(unix)]
                {
                    self.mode == other.mode
                }
                #[cfg(not(unix))]
                {
                    true
                }
            }
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
    /// Format version. Omitted when 1 (the only version this binary
    /// understands) so existing manifests stay byte-identical; a future
    /// incompatible format bumps it and old binaries refuse loudly instead
    /// of misreading (round-34).
    #[serde(default = "default_format_version", skip_serializing_if = "is_v1")]
    pub version: u32,
    /// Absolute path of the snapshotted directory.
    pub base: PathBuf,
    /// When this scan ran, seconds since UNIX epoch (NOT the fork time:
    /// every scan/rescan re-stamps it — round-34).
    pub created_epoch: u64,
    /// Relative path -> entry. Symlinks are leaf entries, never traversed.
    #[serde(deserialize_with = "deserialize_entries")]
    pub entries: BTreeMap<PathBuf, Entry>,
}

fn default_format_version() -> u32 {
    1
}

fn is_v1(v: &u32) -> bool {
    *v == 1
}

/// Custom `entries` deserializer: rejects duplicate path keys, which
/// serde_json's BTreeMap silently collapses (last-wins). A corrupt second
/// entry would otherwise override a good one and produce misleading diff
/// reports (round-23).
fn deserialize_entries<'de, D>(d: D) -> std::result::Result<BTreeMap<PathBuf, Entry>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct EntriesVisitor;
    impl<'de> serde::de::Visitor<'de> for EntriesVisitor {
        type Value = BTreeMap<PathBuf, Entry>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a map of path -> entry")
        }
        fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            let mut out = BTreeMap::new();
            while let Some((k, v)) = map.next_entry::<PathBuf, Entry>()? {
                if out.insert(k.clone(), v).is_some() {
                    return Err(serde::de::Error::custom(format!(
                        "duplicate path key '{}'",
                        k.display()
                    )));
                }
            }
            Ok(out)
        }
    }
    d.deserialize_map(EntriesVisitor)
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
            // Non-UTF-8 paths cannot be serialized into the JSON manifest
            // (serde rejects them). Skipping with a warning (like special
            // files) keeps fork/apply working instead of hard-failing or
            // wedging forever on an unserializable entry (round-29).
            if rel.to_str().is_none() {
                warnings.push((rel.clone(), "non-UTF-8 filename skipped".into()));
                continue;
            }
            // macOS: APFS is normalization-insensitive — readdir may return
            // NFD while a program later spells the same file NFC, and the
            // byte-exact whiteout match would miss the deletion. Canonicalize
            // keys to NFC so every spelling of a name compares equal
            // (round-29).
            #[cfg(target_os = "macos")]
            let rel: PathBuf = {
                let s = rel.to_string_lossy();
                let nfc = unicode_normalization::UnicodeNormalization::nfc(&*s).collect::<String>();
                PathBuf::from(nfc)
            };

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
                    Ok(t) => {
                        // A non-UTF-8 link target cannot be serialized into
                        // the manifest either (round-29).
                        if t.to_str().is_none() {
                            warnings.push((rel.clone(), "non-UTF-8 symlink target skipped".into()));
                            continue;
                        }
                        entry.link_target = Some(t);
                    }
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
                version: 1,
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
        // A future format version must fail LOUDLY and distinctly — the
        // file is not corrupt, it is newer than this binary. Misreporting
        // it as corruption would push users toward `drop --force`, which
        // would destroy a healthy worktree (round-34).
        if m.version > 1 {
            return Err(Error::UnsupportedFormat(format!(
                "manifest format version {} (this binary supports up to 1); \
                 written by a newer cowt — upgrade or restore the old manifest",
                m.version
            )));
        }
        // Path keys must respect the same invariants the scanner enforces
        // (relative, no `.`/`..`/empty components): a corrupt key would
        // otherwise turn a real worktree change into a misleading
        // both_added conflict or a silent no-op (round-23). On macOS, also
        // NFC-normalize like the scanner does, so manifests written before
        // round-29 (or hand-edited with NFD keys) match the rescan key set
        // — otherwise the same file appears as Deleted+Added (round-34).
        #[cfg(target_os = "macos")]
        let entries: BTreeMap<PathBuf, Entry> = m
            .entries
            .into_iter()
            .map(|(k, v)| {
                let s = k.to_string_lossy();
                let nfc = unicode_normalization::UnicodeNormalization::nfc(&*s).collect::<String>();
                (PathBuf::from(nfc), v)
            })
            .collect();
        #[cfg(target_os = "macos")]
        let m = Manifest { entries, ..m };
        for rel in m.entries.keys() {
            if rel.as_os_str().is_empty()
                || rel.is_absolute()
                || rel
                    .components()
                    .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                return Err(Error::CorruptManifest(format!(
                    "invalid path key '{}'",
                    rel.display()
                )));
            }
        }
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

/// Keys of `entries` that collide after case folding — two paths differing
/// by case alone are physically one file on NTFS/default APFS (round-38-04:
/// a Linux-created manifest read by a Windows cowt). Comparison is
/// component-wise (round-40-01: string-level folding is separator-sensitive,
/// while a "/" key and a "\" key denote the same file on Windows). Returns
/// the colliding keys; the caller refuses with a cross-platform explanation.
pub fn case_fold_collision_keys(entries: &BTreeMap<PathBuf, Entry>) -> Vec<PathBuf> {
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut collisions: Vec<PathBuf> = Vec::new();
    for p in entries.keys() {
        if seen.iter().any(|s| crate::merge::case_fold_path_eq(s, p)) {
            collisions.push(p.clone());
        }
        seen.push(p.clone());
    }
    collisions.sort();
    collisions.dedup();
    collisions
}

/// Windows reserved names (CON, PRN, AUX, NUL, COM1-9, LPT1-9 — any
/// extension variant) and trailing-dot/space components. NTFS cannot
/// express them through normal APIs, so a cross-platform manifest
/// containing them is inapplicable on Windows; Win32 path normalization
/// additionally strips trailing dots/spaces, silently collapsing distinct
/// keys (round-38-05).
pub fn windows_inexpressible_keys(entries: &BTreeMap<PathBuf, Entry>) -> Vec<PathBuf> {
    fn reserved(component: &str) -> bool {
        // The reserved name is the part before the first dot.
        let base = component.split('.').next().unwrap_or("");
        let b = base.to_ascii_uppercase();
        b == "CON"
            || b == "PRN"
            || b == "AUX"
            || b == "NUL"
            || ((b.starts_with("COM") || b.starts_with("LPT"))
                && b.len() == 4
                && b.as_bytes()[3].is_ascii_digit()
                && b.as_bytes()[3] != b'0')
    }
    entries
        .keys()
        .filter(|p| {
            p.components().any(|c| {
                let s = c.as_os_str().to_string_lossy();
                reserved(&s) || s.ends_with('.') || s.ends_with(' ') || s.is_empty()
            })
        })
        .cloned()
        .collect()
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
