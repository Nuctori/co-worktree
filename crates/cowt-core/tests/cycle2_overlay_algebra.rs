//! CYCLE 2 adversarial audit — overlay fold algebraic properties A2 + A9.
//!
//! A2 (join semilattice over disjoint layers):
//!   * identity:  fold(base, ∅) == base
//!   * idempotence: fold(fold(base,U),U) == fold(base,U)
//!   * associativity/commutativity of disjoint union:
//!       fold(fold(base,A),B) == fold(base,A∪B)   (A,B share NO keys)
//!
//! A9 (bounded-monotonic + whiteout-exact):
//!   * every base entry upper neither deletes nor overrides survives verbatim
//!   * a whiteout deletes exactly its victim subtree
//!   * a file re-created under its own whiteout in the same layer survives
//!   * an opaque marker (`\.wh\.\.wh\.\.opq`) shadows the ENTIRE base subtree
//!     beneath it
//!   * a zero-size-file opaque marker works (unprivileged backend encoding)
//!
//! All upper-dir writes use overlayfs `.wh.`-encoding (marker next to victim).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use cowt_core::manifest::{Entry, EntryKind, Manifest};
use cowt_core::overlay;
use proptest::prelude::*;
use tempfile::TempDir;

// ── helpers ───────────────────────────────────────────────────────────────

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    if p
        .parent()
        .map(|pp| fs::create_dir_all(pp).is_err())
        .unwrap_or(true)
    {
        return;
    }
    let _ = fs::write(p, content);
}

/// overlayfs `.wh.`-encoding: a whiteout for `sub/c.txt` lives at
/// `upper/sub/.wh.c.txt` (next to the victim).
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

fn opaque_marker(upper: &Path, dir: &str) {
    let p = upper.join(dir).join(".wh..wh..opq");
    if p
        .parent()
        .map(|pp| fs::create_dir_all(pp).is_err())
        .unwrap_or(true)
    {
        return;
    }
    // zero-size regular file — the encoding every unprivileged backend emits.
    let _ = fs::write(p, b"");
}

fn scan(p: &Path) -> Manifest {
    Manifest::scan(p).unwrap().manifest
}

/// Content signature that ignores scan-time metadata (mtime_ns) so that copies
/// made by the test harness (which may touch mtime) do not cause false
/// algebraic mismatches. kind + size + content-hash is the real contract.
fn sig(e: &Entry) -> (EntryKind, u64, Option<String>) {
    (e.kind, e.size, e.hash.clone())
}

fn sig_map(m: &Manifest) -> BTreeMap<PathBuf, (EntryKind, u64, Option<String>)> {
    m.entries.iter().map(|(k, v)| (k.clone(), sig(v))).collect()
}

fn path_key_set(m: &Manifest) -> std::collections::BTreeSet<String> {
    m.entries
        .keys()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect()
}

fn leaf_name() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::char::range('a', 'z'), 1..4)
        .prop_map(|v| v.into_iter().collect())
}

/// (leaf, owner): owner 0 = base-only, 1 = owned by layer A, 2 = owned by B.
/// Per-leaf ownership is exclusive, so A-keys and B-keys are disjoint sets.
fn leaf_owners() -> impl Strategy<Value = Vec<(String, u8)>> {
    prop::collection::vec((leaf_name(), prop::sample::select(&[0u8, 1, 2])), 1..8)
}

// ── A2: identity ───────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    /// A2-identity: folding the empty upper layer reproduces base EXACTLY
    /// (including verbatim scan metadata — no upper entries are scanned).
    #[test]
    fn a2_identity(base_paths in prop::collection::vec(leaf_name(), 1..8)) {
        let tmp = TempDir::new().unwrap();
        let base_dir = tmp.path().join("base");
        let empty = tmp.path().join("empty");
        fs::create_dir_all(&base_dir).unwrap();
        fs::create_dir_all(&empty).unwrap();
        for p in &base_paths {
            write(&base_dir, p, &format!("b-{}", p));
        }
        let base = scan(&base_dir);
        let folded = overlay::effective_manifest_fold(&base, &empty, false)
            .map_err(|e| TestCaseError::fail(format!("fold failed: {e}")))?;
        // Exact equality is valid here: empty upper => base entries are cloned
        // verbatim, no second scan occurs.
        prop_assert_eq!(
            &folded.entries,
            &base.entries,
            "folding the empty upper must be the identity"
        );
    }

    /// A2-idempotence: folding the same random upper twice is identical.
    #[test]
    fn a2_idempotent(
        base_paths in prop::collection::vec(leaf_name(), 1..6),
        upper_paths in prop::collection::vec(leaf_name(), 0..6),
        whiteout_names in prop::collection::vec(leaf_name(), 0..4),
        with_copy_tmp in proptest::bool::ANY,
    ) {
        let tmp = TempDir::new().unwrap();
        let base_dir = tmp.path().join("base");
        let upper = tmp.path().join("upper");
        fs::create_dir_all(&base_dir).unwrap();
        fs::create_dir_all(&upper).unwrap();
        for p in &base_paths {
            write(&base_dir, p, &format!("b-{}", p));
        }
        for p in &upper_paths {
            write(&upper, p, &format!("w-{}", p));
        }
        for p in &whiteout_names {
            whiteout(&upper, p);
        }
        if with_copy_tmp {
            let _ = fs::write(upper.join(".cowt-copy-tmp.residue"), b"torn");
        }
        let base = scan(&base_dir);
        let once = overlay::effective_manifest_fold(&base, &upper, false)
            .map_err(|e| TestCaseError::fail(format!("first fold failed: {e}")))?;
        let twice = overlay::effective_manifest_fold(&base, &upper, false)
            .map_err(|e| TestCaseError::fail(format!("second fold failed: {e}")))?;
        prop_assert_eq!(&once.entries, &twice.entries, "fold must be idempotent");
    }

    /// A2-associativity (disjoint): fold(fold(base,A),B) == fold(base,A∪B)
    /// for layers A,B that own disjoint key sets. Compared on content signature
    /// (kind+size+hash) to absorb copy-induced mtime differences.
    #[test]
    fn a2_associativity_disjoint(spec in leaf_owners()) {
        let tmp = TempDir::new().unwrap();
        let base_dir = tmp.path().join("base");
        let upper_a = tmp.path().join("A");
        let upper_b = tmp.path().join("B");
        let union = tmp.path().join("A_B");
        for d in [&base_dir, &upper_a, &upper_b, &union] {
            fs::create_dir_all(d).unwrap();
        }
        // every leaf starts in base.
        for (name, _) in &spec {
            write(&base_dir, name, &format!("b-{}", name));
        }
        // A owns owner==1, B owns owner==2 (mutually exclusive per leaf).
        for (name, owner) in &spec {
            if *owner == 1 {
                write(&upper_a, name, &format!("A-{}", name));
            } else if *owner == 2 {
                write(&upper_b, name, &format!("B-{}", name));
            }
        }
        let base = scan(&base_dir);

        // direct: base with A∪B merged.
        copy_tree(&upper_a, &union);
        copy_tree(&upper_b, &union);
        let direct = overlay::effective_manifest_fold(&base, &union, false)
            .map_err(|e| TestCaseError::fail(format!("direct fold failed: {e}")))?;

        // indirect: (base ⊔ A) ⊔ B.
        let m1 = overlay::effective_manifest_fold(&base, &upper_a, false)
            .map_err(|e| TestCaseError::fail(format!("inner fold failed: {e}")))?;
        let indirect = overlay::effective_manifest_fold(&m1, &upper_b, false)
            .map_err(|e| TestCaseError::fail(format!("outer fold failed: {e}")))?;

        prop_assert_eq!(
            path_key_set(&direct),
            path_key_set(&indirect),
            "disjoint-layer fold must associate (path set)"
        );
        prop_assert_eq!(
            sig_map(&direct),
            sig_map(&indirect),
            "disjoint-layer fold must associate (content signature)"
        );
    }
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
            let _ = fs::copy(&src_p, &dst_p);
        }
    }
}

// ── A9: concrete assertions ────────────────────────────────────────────────

#[test]
fn a9_base_entry_survives_verbatim() {
    let tmp = TempDir::new().unwrap();
    let base_dir = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    fs::create_dir_all(&base_dir).unwrap();
    fs::create_dir_all(&upper).unwrap();
    write(&base_dir, "keep.txt", "original");
    write(&base_dir, "sub/inner.txt", "inner");
    write(&base_dir, "dir/d.txt", "d");
    // upper only adds an unrelated file; it must NOT perturb base entries.
    write(&upper, "new.txt", "fresh");

    let base = scan(&base_dir);
    let folded = overlay::effective_manifest_fold(&base, &upper, false).unwrap();

    for rel in ["keep.txt", "sub/inner.txt", "dir/d.txt", "dir"] {
        let p = Path::new(rel);
        let be = base.entries.get(p).expect("base entry present");
        let fe = folded.entries.get(p).expect("base entry must survive");
        assert_eq!(
            fe, be,
            "base entry {rel:?} must survive VERBATIM (byte-identical Entry)"
        );
    }
}

#[test]
fn a9_whiteout_deletes_exactly() {
    let tmp = TempDir::new().unwrap();
    let base_dir = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    fs::create_dir_all(&base_dir).unwrap();
    fs::create_dir_all(&upper).unwrap();
    write(&base_dir, "victim_dir/a.txt", "a");
    write(&base_dir, "victim_dir/b.txt", "b");
    write(&base_dir, "sibling/c.txt", "c");
    write(&base_dir, "sibling.txt", "s");
    whiteout(&upper, "victim_dir"); // directory whiteout => whole subtree gone

    let base = scan(&base_dir);
    let folded = overlay::effective_manifest_fold(&base, &upper, false).unwrap();

    // victim subtree exactly removed.
    assert!(!folded.entries.contains_key(Path::new("victim_dir")));
    assert!(!folded.entries.contains_key(Path::new("victim_dir/a.txt")));
    assert!(!folded.entries.contains_key(Path::new("victim_dir/b.txt")));
    // siblings untouched.
    assert!(folded.entries.contains_key(Path::new("sibling")));
    assert!(folded.entries.contains_key(Path::new("sibling/c.txt")));
    assert!(folded.entries.contains_key(Path::new("sibling.txt")));
    // nothing else vanished.
    assert_eq!(
        sig_map(&folded).len(),
        sig_map(&base).len() - 3, // removed: victim_dir, a.txt, b.txt
        "whiteout must delete EXACTLY its victim subtree"
    );

    // File whiteout deletes exactly one path (no over-deletion).
    let tmp2 = TempDir::new().unwrap();
    let b2 = tmp2.path().join("base");
    let u2 = tmp2.path().join("upper");
    fs::create_dir_all(&b2).unwrap();
    fs::create_dir_all(&u2).unwrap();
    write(&b2, "gone.txt", "x");
    write(&b2, "gone2.txt", "y");
    write(&b2, "keep.txt", "z");
    whiteout(&u2, "gone.txt");
    let folded2 = overlay::effective_manifest_fold(&scan(&b2), &u2, false).unwrap();
    assert!(!folded2.entries.contains_key(Path::new("gone.txt")));
    assert!(folded2.entries.contains_key(Path::new("gone2.txt")));
    assert!(folded2.entries.contains_key(Path::new("keep.txt")));
}

#[test]
fn a9_recreate_under_own_whiteout_survives() {
    let tmp = TempDir::new().unwrap();
    let base_dir = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    fs::create_dir_all(&base_dir).unwrap();
    fs::create_dir_all(&upper).unwrap();
    write(&base_dir, "recreate.txt", "base-content");
    // Same layer: whiteout the file AND re-create it with different content.
    whiteout(&upper, "recreate.txt");
    write(&upper, "recreate.txt", "recreated-content");

    let base = scan(&base_dir);
    let folded = overlay::effective_manifest_fold(&base, &upper, false).unwrap();

    let fe = folded
        .entries
        .get(Path::new("recreate.txt"))
        .expect("recreated file must survive its own whiteout");
    assert_eq!(fe.kind, EntryKind::File);
    let upper_scan = scan(&upper);
    let ue = upper_scan
        .entries
        .get(Path::new("recreate.txt"))
        .expect("upper recreated file present");
    assert_eq!(fe.size, ue.size, "must carry the upper recreated content");
    assert_eq!(fe.hash, ue.hash, "must carry the upper recreated content");
    assert_ne!(
        fe.hash,
        base.entries.get(Path::new("recreate.txt")).unwrap().hash,
        "must NOT be the base (deleted+recreated) content"
    );

    // Same semantics for a recreated directory (whiteout dir + re-add child).
    let tmp2 = TempDir::new().unwrap();
    let b2 = tmp2.path().join("base");
    let u2 = tmp2.path().join("upper");
    fs::create_dir_all(&b2).unwrap();
    fs::create_dir_all(&u2).unwrap();
    write(&b2, "dd/old.txt", "old");
    whiteout(&u2, "dd");
    write(&u2, "dd/new.txt", "new");
    let folded2 = overlay::effective_manifest_fold(&scan(&b2), &u2, false).unwrap();
    assert!(folded2.entries.contains_key(Path::new("dd")));
    assert!(folded2.entries.contains_key(Path::new("dd/new.txt")));
    assert!(!folded2.entries.contains_key(Path::new("dd/old.txt")));
}

#[test]
fn a9_opaque_shadows_entire_subtree() {
    let tmp = TempDir::new().unwrap();
    let base_dir = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    fs::create_dir_all(&base_dir).unwrap();
    fs::create_dir_all(&upper).unwrap();
    write(&base_dir, "sub/b.txt", "b");
    write(&base_dir, "sub/c.txt", "c");
    write(&base_dir, "sub/nested/d.txt", "d");
    write(&base_dir, "outside.txt", "o");
    // Opaque marker at `sub` must shadow the ENTIRE base subtree beneath it.
    opaque_marker(&upper, "sub");
    // A re-created file under the opaque dir must survive.
    write(&upper, "sub/recreated.txt", "r");

    let base = scan(&base_dir);
    let folded = overlay::effective_manifest_fold(&base, &upper, false).unwrap();

    // base subtree beneath `sub` must be gone (every descendant).
    for rel in ["sub/b.txt", "sub/c.txt", "sub/nested/d.txt", "sub/nested"] {
        assert!(
            !folded.entries.contains_key(Path::new(rel)),
            "opaque must shadow base {rel:?}"
        );
    }
    // The opaque dir itself survives because upper re-created a child.
    assert!(folded.entries.contains_key(Path::new("sub")));
    assert!(folded.entries.contains_key(Path::new("sub/recreated.txt")));
    // Outside is untouched.
    assert!(folded.entries.contains_key(Path::new("outside.txt")));
}

#[test]
fn a9_zero_size_file_opaque_marker_works() {
    // The unprivileged/winfsp/macos backends encode the opaque marker as a
    // ZERO-SIZE regular file. If detection only accepted a char device, this
    // would be a silent data-loss bug. Assert the zero-size encoding works.
    let tmp = TempDir::new().unwrap();
    let base_dir = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    fs::create_dir_all(&base_dir).unwrap();
    fs::create_dir_all(&upper).unwrap();
    write(&base_dir, "sub/x.txt", "x");
    write(&base_dir, "sub/y.txt", "y");
    // Marker is explicitly a zero-size regular file (verified below).
    opaque_marker(&upper, "sub");
    let meta = fs::symlink_metadata(upper.join("sub/.wh..wh..opq")).unwrap();
    assert!(meta.is_file() && meta.len() == 0, "fixture marker must be zero-size file");

    let base = scan(&base_dir);
    let folded = overlay::effective_manifest_fold(&base, &upper, false).unwrap();
    assert!(
        !folded.entries.contains_key(Path::new("sub/x.txt")),
        "zero-size opaque marker must shadow base subtree"
    );
    assert!(
        !folded.entries.contains_key(Path::new("sub/y.txt")),
        "zero-size opaque marker must shadow base subtree"
    );
}
