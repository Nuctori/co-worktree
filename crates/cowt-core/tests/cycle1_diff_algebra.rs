//! Cycle 1 adversarial audit: diff algebra properties A1 (cancellative
//! inverse under realization) and A3 (antisymmetry / sign reversal).
//!
//! A1: diff(base, work) == D  ==>  diff(base, apply_effect(base, work)) == D
//!     realized through the REAL product path:
//!       work <- overlay::effective_manifest_fold(base, upper)
//!       plan <- merge::plan(base, current=base, work, upper)
//!       merge::execute(plan, host)         (host initialized from base)
//!     then re-diff the realized host against the SAME base. The structural
//!     change set must reproduce exactly. Any phantom change or lost change
//!     is a real data-loss / corruption bug.
//!
//! A3: diff(base, work) == -diff(work, base)
//!     Added<->Deleted swapped, Modified preserved, paths identical.
//!
//! Helpers (write/whiteout/scan/copy_tree/tree_files) are copied from
//! tests/algebraic_audit.rs so this file is self-contained.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use cowt_core::diff::{self, ChangeKind};
use cowt_core::manifest::Manifest;
use cowt_core::merge;
use cowt_core::overlay;
use proptest::prelude::*;
use tempfile::TempDir;

// ── fixtures / helpers ────────────────────────────────────────────────────

/// Write a file; non-panicking. A fuzzer may name one path as the prefix of
/// another (e.g. "a" and "a/b"); if the parent dir can't be made because "a"
/// is already a file, skip the entry rather than crash.
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

/// overlayfs `.wh.`-encoding: a whiteout for `sub/c.txt` lives at
/// `upper/sub/.wh.c.txt`. Skips when the parent can't be made.
fn whiteout(upper: &Path, name: &str) {
    let victim = Path::new(name);
    let dir = victim.parent().unwrap_or_else(|| Path::new(""));
    let base = victim.file_name().unwrap_or(victim.as_os_str());
    let wh = upper
        .join(dir)
        .join(format!(".wh.{}", base.to_string_lossy()));
    if wh
        .parent()
        .map(|p| fs::create_dir_all(p).is_err())
        .unwrap_or(true)
    {
        return;
    }
    let _ = fs::write(wh, b"");
}

fn scan(p: &Path) -> Manifest {
    Manifest::scan(p).unwrap().manifest
}

fn copy_tree(src: &Path, dst: &Path) {
    for e in fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        let name = e.file_name();
        let src_p = e.path();
        let dst_p = dst.join(&name);
        if e.file_type().unwrap().is_dir() {
            fs::create_dir_all(&dst_p).unwrap();
            copy_tree(&src_p, &dst_p);
        } else {
            fs::copy(&src_p, &dst_p).unwrap();
        }
    }
}

#[allow(dead_code)]
fn tree_files(root: &Path) -> BTreeMap<PathBuf, String> {
    let mut out = BTreeMap::new();
    fn walk(out: &mut BTreeMap<PathBuf, String>, root: &Path, dir: &Path) {
        for e in fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let path = e.path();
            let rel = path.strip_prefix(root).unwrap().to_path_buf();
            match e.file_type().unwrap() {
                t if t.is_dir() => walk(out, root, &path),
                t if t.is_file() => {
                    out.insert(rel, fs::read_to_string(&path).unwrap());
                }
                _ => {}
            }
        }
    }
    walk(&mut out, root, root);
    out
}

/// Change-signature set used to compare two diffs regardless of ordering.
/// Paths are normalized (\ -> /) so the comparison is platform-stable.
fn diff_signature(changes: &[diff::Change]) -> BTreeSet<String> {
    changes
        .iter()
        .map(|c| {
            let k = match c.kind {
                ChangeKind::Added => 'A',
                ChangeKind::Modified => 'M',
                ChangeKind::Deleted => 'D',
            };
            format!("{}:{}", k, c.path.to_string_lossy().replace('\\', "/"))
        })
        .collect()
}

// ── A1 round-trip core ─────────────────────────────────────────────────────

/// Realize `work` (folded from base+upper) into a host copied from base, then
/// re-diff the realized host against base and compare to the original diff.
/// Returns the two signatures so callers can assert. Panics (via prop) on a
/// genuine product defect (phantom or lost change on realization).
#[allow(clippy::too_many_arguments)]
fn a1_round_trip(
    base_paths: &[PathBuf],
    base_content: &[String],
    add_paths: &[PathBuf],
    add_content: &[String],
    mods: &[(usize, String)],
    dels: &[usize],
    recreates: &[(usize, String)],
    opaques: &[usize],
) {
    let tmp = TempDir::new().unwrap();
    let base_dir = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base_dir, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }

    for (i, p) in base_paths.iter().enumerate() {
        let c = base_content
            .get(i)
            .cloned()
            .unwrap_or_else(|| format!("base-{}-{}", p.display(), i));
        write(&base_dir, &p.to_string_lossy(), &c);
    }

    // Adds: brand-new files in upper (may create new nested dirs).
    for (i, p) in add_paths.iter().enumerate() {
        let c = add_content
            .get(i)
            .cloned()
            .unwrap_or_else(|| format!("add-{}-{}", p.display(), i));
        write(&upper, &p.to_string_lossy(), &c);
    }

    // Modifies: override an existing base file in upper.
    for (idx, c) in mods {
        if base_paths.is_empty() {
            continue;
        }
        let p = &base_paths[idx % base_paths.len()];
        write(&upper, &p.to_string_lossy(), c);
    }

    // Deletes: whiteout a base file or dir.
    for idx in dels {
        if base_paths.is_empty() {
            continue;
        }
        let p = &base_paths[idx % base_paths.len()];
        whiteout(&upper, &p.to_string_lossy());
    }

    // Recreates: whiteout a base path then write a (possibly different) file
    // at the same name in upper — this must survive as the new file.
    for (idx, c) in recreates {
        if base_paths.is_empty() {
            continue;
        }
        let p = &base_paths[idx % base_paths.len()];
        whiteout(&upper, &p.to_string_lossy());
        write(&upper, &p.to_string_lossy(), c);
    }

    // Opaque markers: mark the parent dir of a base path opaque, shadowing
    // all base entries under it unless re-created in upper.
    for idx in opaques {
        if base_paths.is_empty() {
            continue;
        }
        let p = &base_paths[idx % base_paths.len()];
        let parent = p.parent().unwrap_or_else(|| Path::new(""));
        let marker = upper.join(parent).join(".wh..wh..opq");
        if marker
            .parent()
            .map(|pp| fs::create_dir_all(pp).is_err())
            .unwrap_or(true)
        {
            continue;
        }
        let _ = fs::write(&marker, b"");
    }

    let base_m = scan(&base_dir);
    let work = overlay::effective_manifest_fold(&base_m, &upper, false).expect("fold must succeed");

    let d1 = diff::diff(&base_m, &work);
    let sig1 = diff_signature(&d1);

    // Realize via the real merge path with an untouched host (current == base).
    copy_tree(&base_dir, &host);
    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    assert!(
        plan.is_clean(),
        "A1 fixture with current==base must yield a clean plan; conflicts={:?}",
        plan.conflicts
    );
    merge::execute(&plan, &host).expect("clean plan must apply");

    let host_m = scan(&host);
    let d2 = diff::diff(&base_m, &host_m);
    let sig2 = diff_signature(&d2);

    assert_eq!(
        sig1, sig2,
        "A1: re-diff after realization must reproduce the original change set \
         (phantom or lost change on apply)"
    );
}

// ── A3: antisymmetry (sign reversal) ───────────────────────────────────────

/// Concrete antisymmetry check between two concrete on-disk trees.
fn a3_check(a_dir: &Path, b_dir: &Path) {
    let ma = scan(a_dir);
    let mb = scan(b_dir);
    let d_ab = diff::diff(&ma, &mb);
    let d_ba = diff::diff(&mb, &ma);

    let sign = |c: &diff::Change| match c.kind {
        ChangeKind::Added => 'A',
        ChangeKind::Modified => 'M',
        ChangeKind::Deleted => 'D',
    };
    let invert = |s: char| match s {
        'A' => 'D',
        'D' => 'A',
        'M' => 'M',
        _ => unreachable!(),
    };

    let set_ab: BTreeSet<(char, String)> = d_ab
        .iter()
        .map(|c| (sign(c), c.path.to_string_lossy().replace('\\', "/")))
        .collect();
    let set_ba: BTreeSet<(char, String)> = d_ba
        .iter()
        .map(|c| (invert(sign(c)), c.path.to_string_lossy().replace('\\', "/")))
        .collect();

    assert_eq!(
        set_ab, set_ba,
        "A3: diff(a,b) must invert diff(b,a) (Added<->Deleted swap, same paths)"
    );
}

#[test]
fn a3_antisymmetry_basic_add_del_mod() {
    let tmp = TempDir::new().unwrap();
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    for d in [&a, &b] {
        fs::create_dir_all(d).unwrap();
    }
    // a-only files => Deleted in diff(a,b)
    write(&a, "only_a.txt", "x");
    write(&a, "dir/old.txt", "o");
    // b-only files => Added in diff(a,b)
    write(&b, "only_b.txt", "y");
    write(&b, "dir/new.txt", "n");
    // shared, modified
    write(&a, "both.txt", "a-version");
    write(&b, "both.txt", "b-version");
    // shared, unchanged
    write(&a, "same.txt", "s");
    write(&b, "same.txt", "s");
    a3_check(&a, &b);
}

#[test]
fn a3_antisymmetry_empty_dirs_and_nesting() {
    let tmp = TempDir::new().unwrap();
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    for d in [&a, &b] {
        fs::create_dir_all(d).unwrap();
    }
    // empty dir present only in b
    fs::create_dir_all(a.join("ea")).unwrap();
    fs::create_dir_all(b.join("eb")).unwrap();
    // deep nesting difference
    write(&a, "deep/x/y/a.txt", "1");
    write(&b, "deep/x/y/b.txt", "2");
    a3_check(&a, &b);
}

#[test]
fn a3_antisymmetry_symmetric_under_swap() {
    // Running the check twice with swapped arguments must agree (A3 is its own
    // inverse: -( -D ) == D).
    let tmp = TempDir::new().unwrap();
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    for d in [&a, &b] {
        fs::create_dir_all(d).unwrap();
    }
    write(&a, "f.txt", "A");
    write(&a, "g.txt", "A");
    write(&b, "g.txt", "B");
    write(&b, "h.txt", "B");
    a3_check(&a, &b);
    a3_check(&b, &a);
}

#[cfg(unix)]
#[test]
fn a3_antisymmetry_symlinks() {
    use std::os::unix::fs::symlink;
    let tmp = TempDir::new().unwrap();
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    for d in [&a, &b] {
        fs::create_dir_all(d).unwrap();
    }
    symlink("target_a", a.join("la")).unwrap();
    symlink("target_b", b.join("lb")).unwrap();
    symlink("same", a.join("ls")).unwrap();
    symlink("same", b.join("ls")).unwrap();
    symlink("changed", a.join("lc")).unwrap();
    symlink("changed2", b.join("lc")).unwrap();
    a3_check(&a, &b);
}

// ── A1: property fuzz (>=300 cases) via real overlay + merge path ──────────

fn rel_name() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::char::range('a', 'z'), 1..5)
        .prop_map(|c| c.into_iter().collect::<String>())
}

fn rel_path() -> impl Strategy<Value = PathBuf> {
    prop::collection::vec(rel_name(), 1..3).prop_map(|c| c.iter().collect::<PathBuf>())
}

fn content_str() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::char::range('a', 'z'), 0..20).prop_map(|c| c.into_iter().collect())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Fuzzed A1 cancellative-inverse round-trip. For every randomized
    /// base/worktree pair the structural diff(base, work) must equal the
    /// re-diff of the realized host against the same base. Catches phantom
    /// changes and lost changes introduced by the apply path.
    #[test]
    fn a1_fuzz_round_trip(
        base_paths in prop::collection::vec(rel_path(), 1..6),
        base_content in prop::collection::vec(content_str(), 1..6),
        add_paths in prop::collection::vec(rel_path(), 0..6),
        add_content in prop::collection::vec(content_str(), 0..6),
        mods in prop::collection::vec((prop::num::usize::ANY, content_str()), 0..4),
        dels in prop::collection::vec(prop::num::usize::ANY, 0..4),
        recreates in prop::collection::vec((prop::num::usize::ANY, content_str()), 0..3),
        opaques in prop::collection::vec(prop::num::usize::ANY, 0..3),
    ) {
        a1_round_trip(
            &base_paths,
            &base_content,
            &add_paths,
            &add_content,
            &mods,
            &dels,
            &recreates,
            &opaques,
        );
    }

    /// Fuzzed A3 antisymmetry across randomized tree pairs.
    #[test]
    fn a3_fuzz_antisymmetry(
        a_paths in prop::collection::vec(rel_path(), 0..6),
        a_content in prop::collection::vec(content_str(), 0..6),
        b_paths in prop::collection::vec(rel_path(), 0..6),
        b_content in prop::collection::vec(content_str(), 0..6),
    ) {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        for d in [&a, &b] {
            fs::create_dir_all(d).unwrap();
        }
        for (i, p) in a_paths.iter().enumerate() {
            let c = a_content.get(i).cloned().unwrap_or_else(|| format!("a-{}", i));
            write(&a, &p.to_string_lossy(), &c);
        }
        for (i, p) in b_paths.iter().enumerate() {
            let c = b_content.get(i).cloned().unwrap_or_else(|| format!("b-{}", i));
            write(&b, &p.to_string_lossy(), &c);
        }
        a3_check(&a, &b);
    }
}

// ── A1: hand-crafted adversarial compositions (deterministic) ──────────────

#[test]
fn a1_modify_delete_add_same_dir() {
    a1_round_trip(
        &[
            PathBuf::from("a.txt"),
            PathBuf::from("b.txt"),
            PathBuf::from("c.txt"),
        ],
        &["v1".into(), "v1".into(), "v1".into()],
        &[PathBuf::from("d.txt")],
        &["new".into()],
        &[(0usize, "v2".into())],
        &[1usize],
        &[],
        &[],
    );
}

#[test]
fn a1_recreate_file_in_place() {
    // whiteout + same-name file: must survive as the new file, no phantom.
    a1_round_trip(
        &[PathBuf::from("x.txt")],
        &["old".into()],
        &[],
        &[],
        &[],
        &[],
        &[(0usize, "new".into())],
        &[],
    );
}

#[test]
fn a1_opaque_dir_shadows_base() {
    // Opaque marker on "sub" hides sub/f.txt unless re-created in upper.
    a1_round_trip(
        &[PathBuf::from("sub/f.txt"), PathBuf::from("keep.txt")],
        &["base".into(), "base".into()],
        &[],
        &[],
        &[],
        &[],
        &[],
        &[0usize],
    );
}

#[test]
fn a1_nested_new_dirs() {
    a1_round_trip(
        &[PathBuf::from("keep.txt")],
        &["k".into()],
        &[PathBuf::from("d1/d2/d3/f.txt")],
        &["deep".into()],
        &[],
        &[],
        &[],
        &[],
    );
}

#[test]
fn a1_kind_migration_dir_to_file() {
    // base dir "a" with content, work replaces it with a file "a": whiteout
    // the file under it, then add a plain file named "a".
    a1_round_trip(
        &[PathBuf::from("a/inner.txt")],
        &["x".into()],
        &[PathBuf::from("a")],
        &["file-now".into()],
        &[],
        &[0usize],
        &[],
        &[],
    );
}

#[test]
fn a1_kind_migration_file_to_dir() {
    // base file "a", work replaces it with a dir "a" (whiteout the file, then
    // add a child under "a").
    a1_round_trip(
        &[PathBuf::from("a")],
        &["was-file".into()],
        &[PathBuf::from("a/nested.txt")],
        &["now-dir".into()],
        &[],
        &[0usize],
        &[],
        &[],
    );
}

#[test]
fn a1_delete_then_add_sibling_dirs() {
    a1_round_trip(
        &[PathBuf::from("gone/d.txt"), PathBuf::from("stay/s.txt")],
        &["g".into(), "s".into()],
        &[PathBuf::from("fresh/f.txt")],
        &["f".into()],
        &[],
        &[0usize],
        &[],
        &[],
    );
}
