//! Cycle 5 — adversarial audit of manifest property A5.
//!
//! A5 (content-hash congruence + content_eq equivalence):
//!   * `content_eq` is an equivalence relation (reflexive, symmetric,
//!     transitive) over every `EntryKind` (File / Symlink / Directory).
//!   * Equal content => equal hash (congruence); the hash is a well-defined
//!     function out of the `content_eq` equivalence classes.
//!   * Unix mode is part of file content: two files identical in bytes but
//!     with different mode MUST NOT be `content_eq`.
//!   * The hash is deterministic: same bytes (+ mode + kind on unix) => same
//!     hash, independent of scan order / worker scheduling.
//!
//! Run: `cargo test --test cycle5_manifest_congruence --workspace`.

use std::fs;
use std::path::{Path, PathBuf};

use cowt_core::manifest::{Entry, EntryKind, Manifest};
use proptest::prelude::*;

// ── randomized Entry generator ─────────────────────────────────────────────

fn entry_strategy() -> impl Strategy<Value = Entry> {
    let file = (any::<u64>(), any::<u32>()).prop_map(|(h, mode)| Entry {
        kind: EntryKind::File,
        size: 4 + (h % 1000),
        mode: mode & 0o7777,
        mtime_ns: 0,
        hash: Some(format!("{h:064x}")),
        link_target: None,
    });
    let dir = any::<u32>().prop_map(|mode| Entry {
        kind: EntryKind::Dir,
        size: 0,
        mode: mode & 0o7777,
        mtime_ns: 0,
        hash: None,
        link_target: None,
    });
    let sym = prop::collection::vec(prop::char::range('a', 'z'), 1..8).prop_map(|c| Entry {
        kind: EntryKind::Symlink,
        size: 0,
        mode: 0,
        mtime_ns: 0,
        hash: None,
        link_target: Some(PathBuf::from(c.into_iter().collect::<String>())),
    });
    prop_oneof![file, dir, sym]
}

// ── equivalence-relation laws (proptest, 512+ cases) ───────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Reflexivity: every entry is content_eq to itself.
    #[test]
    fn a5_reflexive(e in entry_strategy()) {
        prop_assert!(e.content_eq(&e), "content_eq must be reflexive");
    }

    /// Symmetry: a~b  <=>  b~a for all entries.
    #[test]
    fn a5_symmetric(a in entry_strategy(), b in entry_strategy()) {
        prop_assert_eq!(
            a.content_eq(&b),
            b.content_eq(&a),
            "content_eq must be symmetric"
        );
    }

    /// Transitivity: (a~b && b~c) => a~c for all entries.
    #[test]
    fn a5_transitive(a in entry_strategy(), b in entry_strategy(), c in entry_strategy()) {
        if a.content_eq(&b) && b.content_eq(&c) {
            prop_assert!(
                a.content_eq(&c),
                "content_eq must be transitive (a~b, b~c but !a~c)"
            );
        }
    }
}

// ── explicit unix-mode-as-content rule ─────────────────────────────────────

/// Two files byte-identical (same hash + size) but with different permission
/// bits MUST NOT be `content_eq`; unix mode is part of content.
#[cfg(unix)]
#[test]
fn a5_mode_is_content_byte_equal() {
    let mk = |mode: u32| Entry {
        kind: EntryKind::File,
        size: 11,
        mode,
        mtime_ns: 0,
        hash: Some("deadbeef".repeat(8)), // 64 hex chars, placeholder digest
        link_target: None,
    };
    let a = mk(0o644);
    let b = mk(0o600); // identical bytes, chmod-only difference
    assert!(a.content_eq(&a), "reflexive baseline");
    assert!(!a.content_eq(&b), "mode-only change must break content_eq");
    assert!(!b.content_eq(&a), "mode-only change must break content_eq (symmetric)");
}

/// On platforms without unix mode, byte-identical files (equal hash/size) are
/// content_eq regardless of the (always-zero) mode field — sanity check that
/// the relation stays well-defined and reflexive everywhere.
#[cfg(not(unix))]
#[test]
fn a5_no_mode_platform_reflexive_sane() {
    let e = Entry {
        kind: EntryKind::File,
        size: 11,
        mode: 0,
        mtime_ns: 0,
        hash: Some("deadbeef".repeat(8)),
        link_target: None,
    };
    assert!(e.content_eq(&e));
}

/// mtime is noise: two entries differing only in mtime must remain content_eq.
#[test]
fn a5_mtime_is_noise() {
    let a = Entry {
        kind: EntryKind::File,
        size: 3,
        mode: 0o644,
        mtime_ns: 1,
        hash: Some("beef".repeat(16)),
        link_target: None,
    };
    let mut b = a.clone();
    b.mtime_ns = 9_999_999;
    assert!(a.content_eq(&b), "mtime must not affect content_eq");
    assert!(b.content_eq(&a));
}

// ── congruence + hash determinism (disk-backed) ────────────────────────────

/// Scan a single file of the given content and return its stored hash.
fn scan_hash(dir: &Path, name: &str, content: &[u8]) -> String {
    fs::write(dir.join(name), content).unwrap();
    let m = Manifest::scan(dir).unwrap().manifest;
    m.entries
        .get(Path::new(name))
        .expect("entry present")
        .hash
        .clone()
        .expect("file hash computed")
}

#[test]
fn a5_hash_is_64_hex_blake3() {
    let d = tempfile::tempdir().unwrap();
    let h = scan_hash(d.path(), "f", b"hello");
    assert_eq!(h.len(), 64, "BLAKE3 hex digest must be 64 chars");
    assert!(h.bytes().all(|b| b.is_ascii_hexdigit()), "must be hex");
}

/// Determinism: identical bytes in two different directories hash identically,
/// and re-scanning the same file reproduces the hash exactly.
#[test]
fn a5_hash_determinism_same_content() {
    let d1 = tempfile::tempdir().unwrap();
    let d2 = tempfile::tempdir().unwrap();
    let content = b"the quick brown fox jumps over the lazy dog 1234567890";
    let h1 = scan_hash(d1.path(), "a.txt", content);
    let h2 = scan_hash(d2.path(), "b.txt", content);
    assert_eq!(h1, h2, "same bytes must hash identically");
    let h3 = scan_hash(d1.path(), "a.txt", content);
    assert_eq!(h1, h3, "re-scan must be deterministic");
}

/// Congruence: two distinct files with identical bytes (and kind) carry
/// identical hashes, and `content_eq` holds between them.
#[test]
fn a5_congruence_equal_content_equal_hash() {
    let d = tempfile::tempdir().unwrap();
    fs::write(d.path().join("x"), b"identical-bytes").unwrap();
    fs::write(d.path().join("y"), b"identical-bytes").unwrap();
    let m = Manifest::scan(d.path()).unwrap().manifest;
    let ex = m.entries.get(Path::new("x")).unwrap();
    let ey = m.entries.get(Path::new("y")).unwrap();
    let hx = ex.hash.clone().unwrap();
    let hy = ey.hash.clone().unwrap();
    assert_eq!(hx, hy, "equal content => equal hash (congruence)");
    assert!(ex.content_eq(ey), "equal content entries must be content_eq");
}

/// The hash is a faithful function: different bytes must produce different
/// hashes (rules out a constant / degenerate hash).
#[test]
fn a5_distinct_content_distinct_hash() {
    let d1 = tempfile::tempdir().unwrap();
    let d2 = tempfile::tempdir().unwrap();
    let h1 = scan_hash(d1.path(), "a", b"alpha");
    let h2 = scan_hash(d2.path(), "a", b"beta");
    assert_ne!(h1, h2, "different bytes must produce different hashes");
}

/// Full-tree scan determinism: scanning the same tree twice yields identical
/// per-entry hashes (parallel worker scheduling must not leak into results).
#[test]
fn a5_full_scan_deterministic() {
    let d = tempfile::tempdir().unwrap();
    fs::write(d.path().join("f1"), b"one").unwrap();
    fs::create_dir(d.path().join("sub")).unwrap();
    fs::write(d.path().join("sub").join("f2"), b"two").unwrap();
    fs::write(d.path().join("f3"), b"three").unwrap();
    let m1 = Manifest::scan(d.path()).unwrap().manifest;
    let m2 = Manifest::scan(d.path()).unwrap().manifest;
    assert_eq!(m1.entries.len(), m2.entries.len());
    for (k, e1) in &m1.entries {
        let e2 = m2
            .entries
            .get(k)
            .unwrap_or_else(|| panic!("missing entry {k:?} on second scan"));
        assert_eq!(
            e1.hash, e2.hash,
            "hash for {k:?} must be deterministic across scans"
        );
    }
}
