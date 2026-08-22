//! CYCLE 3 adversarial audit — diff exact-partition property A4.
//!
//! A4 (exact partition of CHANGED paths): the diff change set is an exact
//! partition of the set of paths that differ between base and work — no path
//! double-counted, every changed path appears exactly once, no unchanged path
//! appears. Also: Modified requires content/kind differ; Added requires
//! absent-in-base; Deleted requires absent-in-work.
//!
//! The oracle is derived INDEPENDENTLY from the scanned manifests (presence +
//! `Entry::content_eq`), so any logic error in `diff` that drops, duplicates,
//! or misclassifies a path is caught. Trees are materialized on disk and
//! scanned, then the change set is compared to the oracle.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use cowt_core::diff::{self, Change, ChangeKind};
use cowt_core::manifest::Manifest;
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use tempfile::TempDir;

// ── helpers ───────────────────────────────────────────────────────────────

/// Write a file; skip (rather than crash) if the parent can't be made because
/// a sibling already occupies that name as a file. The oracle is derived from
/// the *scanned* manifest, so dropped paths stay consistent with reality.
fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    if p.parent()
        .map(|pp| fs::create_dir_all(pp).is_err())
        .unwrap_or(true)
    {
        return;
    }
    let _ = fs::write(p, content);
}

/// Create a raw empty directory (exercises "added directory" entries).
fn mkdir(root: &Path, rel: &str) {
    let _ = fs::create_dir_all(root.join(rel));
}

fn scan(p: &Path) -> Manifest {
    Manifest::scan(p).unwrap().manifest
}

fn rel_name() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::char::range('a', 'z'), 1..5)
        .prop_map(|c| c.into_iter().collect::<String>())
}

fn rel_path() -> impl Strategy<Value = PathBuf> {
    prop::collection::vec(rel_name(), 1..4).prop_map(|c| c.iter().collect::<PathBuf>())
}

fn content_str() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::char::range('a', 'z'), 0..24).prop_map(|c| c.into_iter().collect())
}

// ── A4 oracle + check ──────────────────────────────────────────────────────

/// Independent reference partition: for every path in base∪work, classify it
/// Added / Deleted / Modified / unchanged using presence + content_eq. This is
/// the mathematical contract of A4; `diff` must reproduce it exactly.
fn expected_partition(base: &Manifest, work: &Manifest) -> BTreeMap<PathBuf, ChangeKind> {
    let mut expected: BTreeMap<PathBuf, ChangeKind> = BTreeMap::new();
    let mut all: BTreeSet<&PathBuf> = BTreeSet::new();
    for p in base.entries.keys() {
        all.insert(p);
    }
    for p in work.entries.keys() {
        all.insert(p);
    }
    for p in all {
        match (base.entries.get(p), work.entries.get(p)) {
            (None, Some(_)) => {
                expected.insert(p.clone(), ChangeKind::Added);
            }
            (Some(_), None) => {
                expected.insert(p.clone(), ChangeKind::Deleted);
            }
            (Some(b), Some(w)) => {
                if !b.content_eq(w) {
                    expected.insert(p.clone(), ChangeKind::Modified);
                }
            }
            (None, None) => {}
        }
    }
    expected
}

/// Assert the four clauses of A4 against an actual diff result.
fn check_a4(base: &Manifest, work: &Manifest, changes: &[Change]) -> Result<(), TestCaseError> {
    let expected = expected_partition(base, work);

    // (a) change-path set == set of differing paths.
    let observed_paths: BTreeSet<&PathBuf> = changes.iter().map(|c| &c.path).collect();
    let expected_paths: BTreeSet<&PathBuf> = expected.keys().collect();
    prop_assert_eq!(
        &observed_paths,
        &expected_paths,
        "A4(a): change-path set must equal the set of differing paths"
    );

    // (b) no duplicate paths.
    let mut seen: BTreeSet<&PathBuf> = BTreeSet::new();
    for c in changes {
        prop_assert!(
            seen.insert(&c.path),
            "A4(b): duplicate path {}",
            c.path.display()
        );
    }

    // (c) every change has the correct kind per the absent/present/content
    //     rules.
    for c in changes {
        let exp = expected
            .get(&c.path)
            .unwrap_or_else(|| panic!("A4(c): unexpected path {}", c.path.display()));
        prop_assert_eq!(&c.kind, exp, "A4(c): wrong kind at {}", c.path.display());
        match c.kind {
            ChangeKind::Added => prop_assert!(
                !base.entries.contains_key(&c.path),
                "A4(c): Added {} must be absent in base",
                c.path.display()
            ),
            ChangeKind::Deleted => prop_assert!(
                !work.entries.contains_key(&c.path),
                "A4(c): Deleted {} must be absent in work",
                c.path.display()
            ),
            ChangeKind::Modified => {
                let b = base.entries.get(&c.path).expect("Modified base present");
                let w = work.entries.get(&c.path).expect("Modified work present");
                prop_assert!(
                    !b.content_eq(w),
                    "A4(c): Modified {} must differ in content/kind",
                    c.path.display()
                );
            }
        }
    }

    // (d) unchanged paths never appear.
    for (p, b) in &base.entries {
        if let Some(w) = work.entries.get(p) {
            if b.content_eq(w) {
                prop_assert!(
                    !observed_paths.contains(p),
                    "A4(d): unchanged path {} must not appear",
                    p.display()
                );
            }
        }
    }
    Ok(())
}

// ── fuzz harness ───────────────────────────────────────────────────────────

/// Materialize a base tree and a randomized work tree (modifies / deletes /
/// adds from base), scan both, diff, and check A4.
fn fuzz_a4(
    paths: &[PathBuf],
    del_mask: &[bool],
    mod_mask: &[bool],
    add_paths: &[PathBuf],
    add_content: &[String],
    add_dirs: &[String],
) -> Result<(), TestCaseError> {
    let tmp = TempDir::new().unwrap();
    let base_dir = tmp.path().join("base");
    let work_dir = tmp.path().join("work");
    fs::create_dir_all(&base_dir).unwrap();
    fs::create_dir_all(&work_dir).unwrap();

    // Base: every path gets a deterministic base content.
    for (i, p) in paths.iter().enumerate() {
        let c = format!("base-{}-{}", p.display(), i);
        write(&base_dir, &p.to_string_lossy(), &c);
    }

    // Work: apply del/mod to the shared paths, then add new ones.
    for (i, p) in paths.iter().enumerate() {
        if *del_mask.get(i).unwrap_or(&false) {
            continue; // deleted: absent from work
        }
        if *mod_mask.get(i).unwrap_or(&false) {
            let c = format!("work-MOD-{}-{}", p.display(), i);
            write(&work_dir, &p.to_string_lossy(), &c);
        } else {
            let c = format!("base-{}-{}", p.display(), i);
            write(&work_dir, &p.to_string_lossy(), &c);
        }
    }
    for (i, p) in add_paths.iter().enumerate() {
        let c = add_content
            .get(i)
            .cloned()
            .unwrap_or_else(|| format!("work-ADD-{}", p.display()));
        write(&work_dir, &p.to_string_lossy(), &c);
    }
    for d in add_dirs {
        mkdir(&work_dir, d);
    }

    let base_m = scan(&base_dir);
    let work_m = scan(&work_dir);
    let changes = diff::diff(&base_m, &work_m);
    check_a4(&base_m, &work_m, &changes)?;
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Fuzzed A4 exact-partition invariant across randomized base/work trees.
    #[test]
    fn a4_fuzz_exact_partition(
        paths in prop::collection::vec(rel_path(), 0..10),
        del_mask in prop::collection::vec(proptest::bool::ANY, 10),
        mod_mask in prop::collection::vec(proptest::bool::ANY, 10),
        add_paths in prop::collection::vec(rel_path(), 0..10),
        add_content in prop::collection::vec(content_str(), 0..10),
        add_dirs in prop::collection::vec(rel_name(), 0..4),
    ) {
        fuzz_a4(
            &paths, &del_mask, &mod_mask,
            &add_paths, &add_content,
            &add_dirs.iter().map(|s| format!("{}/{}", s, "d")).collect::<Vec<_>>(),
        )?;
    }
}

// ── deterministic adversarial corner cases ─────────────────────────────────

fn tree_of(dir: &Path, files: &[(&str, &str)], dirs: &[&str]) {
    for d in dirs {
        mkdir(dir, d);
    }
    for (p, c) in files {
        write(dir, p, c);
    }
}

#[test]
fn a4_empty_both() {
    let tmp = TempDir::new().unwrap();
    let b = tmp.path().join("b");
    let w = tmp.path().join("w");
    fs::create_dir_all(&b).unwrap();
    fs::create_dir_all(&w).unwrap();
    let bm = scan(&b);
    let wm = scan(&w);
    let ch = diff::diff(&bm, &wm);
    check_a4(&bm, &wm, &ch).unwrap();
    assert!(ch.is_empty(), "empty/empty diff must be empty");
}

#[test]
fn a4_identical_trees() {
    let tmp = TempDir::new().unwrap();
    let b = tmp.path().join("b");
    let w = tmp.path().join("w");
    fs::create_dir_all(&b).unwrap();
    fs::create_dir_all(&w).unwrap();
    tree_of(&b, &[("a.txt", "x"), ("d/e.txt", "y")], &[]);
    tree_of(&w, &[("a.txt", "x"), ("d/e.txt", "y")], &[]);
    let bm = scan(&b);
    let wm = scan(&w);
    let ch = diff::diff(&bm, &wm);
    check_a4(&bm, &wm, &ch).unwrap();
    assert!(ch.is_empty(), "identical trees must produce no changes");
}

#[test]
fn a4_single_add() {
    let tmp = TempDir::new().unwrap();
    let b = tmp.path().join("b");
    let w = tmp.path().join("w");
    fs::create_dir_all(&b).unwrap();
    fs::create_dir_all(&w).unwrap();
    tree_of(&b, &[], &[]);
    tree_of(&w, &[("new.txt", "hi")], &[]);
    let bm = scan(&b);
    let wm = scan(&w);
    let ch = diff::diff(&bm, &wm);
    check_a4(&bm, &wm, &ch).unwrap();
}

#[test]
fn a4_added_nested_dir_and_file() {
    // Adding a file in a brand-new directory must report both the dir and the
    // file exactly once (both are genuinely absent-in-base).
    let tmp = TempDir::new().unwrap();
    let b = tmp.path().join("b");
    let w = tmp.path().join("w");
    fs::create_dir_all(&b).unwrap();
    fs::create_dir_all(&w).unwrap();
    tree_of(&b, &[], &[]);
    tree_of(&w, &[("nested/deep/f.txt", "z")], &[]);
    let bm = scan(&b);
    let wm = scan(&w);
    let ch = diff::diff(&bm, &wm);
    check_a4(&bm, &wm, &ch).unwrap();
}

#[test]
fn a4_added_empty_dir() {
    let tmp = TempDir::new().unwrap();
    let b = tmp.path().join("b");
    let w = tmp.path().join("w");
    fs::create_dir_all(&b).unwrap();
    fs::create_dir_all(&w).unwrap();
    tree_of(&b, &[], &[]);
    tree_of(&w, &[], &["empty"]);
    let bm = scan(&b);
    let wm = scan(&w);
    let ch = diff::diff(&bm, &wm);
    check_a4(&bm, &wm, &ch).unwrap();
}

#[test]
fn a4_single_delete() {
    let tmp = TempDir::new().unwrap();
    let b = tmp.path().join("b");
    let w = tmp.path().join("w");
    fs::create_dir_all(&b).unwrap();
    fs::create_dir_all(&w).unwrap();
    tree_of(&b, &[("gone.txt", "x")], &[]);
    tree_of(&w, &[], &[]);
    let bm = scan(&b);
    let wm = scan(&w);
    let ch = diff::diff(&bm, &wm);
    check_a4(&bm, &wm, &ch).unwrap();
}

#[test]
fn a4_delete_with_parent_dir_stays() {
    // Deleting a file inside a dir keeps the (unchanged) parent dir out of the
    // change set.
    let tmp = TempDir::new().unwrap();
    let b = tmp.path().join("b");
    let w = tmp.path().join("w");
    fs::create_dir_all(&b).unwrap();
    fs::create_dir_all(&w).unwrap();
    tree_of(
        &b,
        &[("d/a.txt", "x"), ("d/b.txt", "y"), ("keep.txt", "k")],
        &[],
    );
    tree_of(&w, &[("d/b.txt", "y"), ("keep.txt", "k")], &[]);
    let bm = scan(&b);
    let wm = scan(&w);
    let ch = diff::diff(&bm, &wm);
    check_a4(&bm, &wm, &ch).unwrap();
}

#[test]
fn a4_single_modify() {
    let tmp = TempDir::new().unwrap();
    let b = tmp.path().join("b");
    let w = tmp.path().join("w");
    fs::create_dir_all(&b).unwrap();
    fs::create_dir_all(&w).unwrap();
    tree_of(&b, &[("m.txt", "old")], &[]);
    tree_of(&w, &[("m.txt", "new")], &[]);
    let bm = scan(&b);
    let wm = scan(&w);
    let ch = diff::diff(&bm, &wm);
    check_a4(&bm, &wm, &ch).unwrap();
}

#[test]
fn a4_modify_does_not_duplicate_dir() {
    // Modifying a nested file must NOT also report its parent dir as Added or
    // Modified.
    let tmp = TempDir::new().unwrap();
    let b = tmp.path().join("b");
    let w = tmp.path().join("w");
    fs::create_dir_all(&b).unwrap();
    fs::create_dir_all(&w).unwrap();
    tree_of(&b, &[("d/a.txt", "old")], &[]);
    tree_of(&w, &[("d/a.txt", "new")], &[]);
    let bm = scan(&b);
    let wm = scan(&w);
    let ch = diff::diff(&bm, &wm);
    check_a4(&bm, &wm, &ch).unwrap();
}

#[test]
fn a4_kind_migration_file_to_dir() {
    let tmp = TempDir::new().unwrap();
    let b = tmp.path().join("b");
    let w = tmp.path().join("w");
    fs::create_dir_all(&b).unwrap();
    fs::create_dir_all(&w).unwrap();
    tree_of(&b, &[("a", "was-file")], &[]);
    tree_of(&w, &[("a/x.txt", "now-dir")], &[]);
    let bm = scan(&b);
    let wm = scan(&w);
    let ch = diff::diff(&bm, &wm);
    check_a4(&bm, &wm, &ch).unwrap();
}

#[test]
fn a4_kind_migration_dir_to_file() {
    let tmp = TempDir::new().unwrap();
    let b = tmp.path().join("b");
    let w = tmp.path().join("w");
    fs::create_dir_all(&b).unwrap();
    fs::create_dir_all(&w).unwrap();
    tree_of(&b, &[("a/inner.txt", "x")], &[]);
    tree_of(&w, &[("a", "now-file")], &[]);
    let bm = scan(&b);
    let wm = scan(&w);
    let ch = diff::diff(&bm, &wm);
    check_a4(&bm, &wm, &ch).unwrap();
}

#[test]
fn a4_mixed_change_set() {
    let tmp = TempDir::new().unwrap();
    let b = tmp.path().join("b");
    let w = tmp.path().join("w");
    fs::create_dir_all(&b).unwrap();
    fs::create_dir_all(&w).unwrap();
    tree_of(
        &b,
        &[
            ("keep.txt", "k"),
            ("mod.txt", "old"),
            ("del.txt", "gone"),
            ("d/a.txt", "old-a"),
            ("d/b.txt", "b"),
        ],
        &[],
    );
    tree_of(
        &w,
        &[
            ("keep.txt", "k"),
            ("mod.txt", "new"),
            ("add.txt", "fresh"),
            ("d/a.txt", "old-a"),
            ("d/c.txt", "new-c"),
        ],
        &[],
    );
    let bm = scan(&b);
    let wm = scan(&w);
    let ch = diff::diff(&bm, &wm);
    check_a4(&bm, &wm, &ch).unwrap();
}
