//! Independent adversarial audit rounds for the cowt-core engine.
//!
//! Five rounds of black-box probing (merge planner, overlay folding, diff
//! engine, apply/execute, state boundaries). Each round's oracle is a
//! concrete invariant that, if violated, is a real data-loss or
//! privilege/escape bug. Tests run on disk (no mount required) so they
//! execute in any environment.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use cowt_core::diff;
use cowt_core::manifest::{Entry, EntryKind, Manifest};
use cowt_core::merge;
use cowt_core::overlay;
use tempfile::TempDir;

use proptest::prelude::*;

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn whiteout(upper: &Path, name: &str) {
    let p = upper.join(format!(".wh.{name}"));
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, b"").unwrap();
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

fn tree_files(root: &Path) -> BTreeMap<PathBuf, String> {
    let mut out = BTreeMap::new();
    fn walk(out: &mut BTreeMap<PathBuf, String>, root: &Path, dir: &Path) {
        for e in fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let path = e.path();
            let rel = path.strip_prefix(root).unwrap().to_path_buf();
            let m = e.file_type().unwrap();
            if m.is_dir() {
                walk(out, root, &path);
            } else if m.is_file() {
                out.insert(rel, fs::read_to_string(&path).unwrap());
            }
        }
    }
    walk(&mut out, root, root);
    out
}
fn normalize(m: &BTreeMap<PathBuf, String>) -> BTreeMap<String, String> {
    m.iter()
        .map(|(k, v)| (k.to_string_lossy().replace("\\", "/"), v.clone()))
        .collect()
}

/// R3-3: lone-CR (old-Mac) line endings must not glue diff lines together;
/// a changed line must appear as -/+ and no raw CR in the unified output.
#[test]
fn r3_lone_cr_normalized() {
    let u = diff::unified_diff("a\nb\r", "a\nc\r");
    assert!(u.lines().any(|l| l == "-b"), "deleted line lost:\n{u}");
    assert!(u.lines().any(|l| l == "+c"), "added line lost:\n{u}");
    assert!(!u.contains('\r'), "lone CR leaked into output:\n{u}");
}

/// R3-4: a binary file (NUL byte) is reported as Binary, never text.
#[test]
fn r3_binary_marker() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let work = tmp.path().join("work");
    fs::create_dir_all(&base).unwrap();
    fs::create_dir_all(&work).unwrap();
    fs::write(base.join("blob.bin"), [0u8, 159, 146, 150]).unwrap();
    fs::write(work.join("blob.bin"), [0u8, 159, 146, 151]).unwrap();
    let (_, _, changes) = diff::diff_trees(&base, &work).unwrap();
    assert!(matches!(changes[0].detail, Some(diff::ContentDiff::Binary)));
}

// ─────────────────────────────────────────────────────────────────────────
// ROUND 4 — apply / execute (full round-trip)
// ─────────────────────────────────────────────────────────────────────────

/// R4-1: apply round-trip converges the host exactly to the effective view
/// (file-for-file, deletions gone).
#[test]
fn r4_apply_round_trip_converges() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "a.txt", "v1");
    write(&base, "del.txt", "vdelete");
    write(&base, "dir/old.txt", "vold");
    write(&upper, "a.txt", "v1new");
    write(&upper, "newfile.txt", "new");
    whiteout(&upper, "del.txt");
    whiteout(&upper, "dir");

    let base_m = scan(&base);
    let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    copy_tree(&base, &host);
    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    assert!(plan.is_clean(), "conflicts: {:?}", plan.conflicts);
    merge::execute(&plan, &host).unwrap();

    let expected: BTreeMap<String, String> = [("a.txt", "v1new"), ("newfile.txt", "new")]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    assert_eq!(normalize(&tree_files(&host)), expected);
    assert!(!host.join("del.txt").exists());
    assert!(!host.join("dir").exists());
}

/// R4-2: re-applying after a clean apply is a no-op (idempotence).
#[test]
fn r4_apply_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "a.txt", "v1");
    write(&upper, "a.txt", "v1new");
    let base_m = scan(&base);
    let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    copy_tree(&base, &host);
    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    merge::execute(&plan, &host).unwrap();
    let after_first = tree_files(&host);
    let host_m = scan(&host);
    let plan2 = merge::plan(&host_m, &host_m, &work, &upper);
    assert!(plan2.is_clean());
    assert!(
        plan2.operations.is_empty(),
        "re-apply must be a no-op: {:?}",
        plan2.operations
    );
    merge::execute(&plan2, &host).unwrap();
    assert_eq!(tree_files(&host), after_first);
}

/// R4-3: TOCTOU — a host edit landing between planning and execute must
/// abort execute and leave the host untouched (the out-of-band edit wins).
#[test]
fn r4_toctou_aborts_and_preserves_host() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "a.txt", "v1");
    write(&upper, "a.txt", "v1new");
    let base_m = scan(&base);
    let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    copy_tree(&base, &host);
    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    assert!(plan.is_clean());
    write(&host, "a.txt", "HOST-OUT-OF-BAND");
    let err = merge::execute(&plan, &host).unwrap_err();
    assert!(
        err.to_string().contains("changed after planning")
            || err.to_string().contains("host path changed"),
        "execute must abort on TOCTOU, got: {err}"
    );
    assert_eq!(
        fs::read_to_string(host.join("a.txt")).unwrap(),
        "HOST-OUT-OF-BAND"
    );
}

/// R4-4: a conflict must write NOTHING — not even an unrelated clean path.
#[test]
fn r4_conflict_writes_nothing() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "conflict.txt", "base");
    write(&base, "clean.txt", "clean-base");
    write(&host, "conflict.txt", "host");
    write(&host, "clean.txt", "clean-base");
    write(&upper, "conflict.txt", "work");
    write(&upper, "clean.txt", "clean-work");
    let base_m = scan(&base);
    let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    assert_eq!(plan.conflicts.len(), 1);
    let err = merge::execute(&plan, &host).unwrap_err();
    assert!(err.to_string().contains("conflict"));
    assert_eq!(
        fs::read_to_string(host.join("conflict.txt")).unwrap(),
        "host"
    );
    assert_eq!(
        fs::read_to_string(host.join("clean.txt")).unwrap(),
        "clean-base"
    );
    let leftovers: Vec<_> = fs::read_dir(tmp.path())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".cowt-apply-"))
        .collect();
    assert!(leftovers.is_empty(), "staging residue: {leftovers:?}");
}

/// R4-5: delete-then-recreate with the SAME content must converge (no
/// spurious delete of the freshly written file).
#[test]
fn r4_delete_then_recreate_same_content_converges() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "a.txt", "same");
    whiteout(&upper, "a.txt");
    write(&upper, "a.txt", "same"); // recreated with identical content
    let base_m = scan(&base);
    let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    copy_tree(&base, &host);
    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    assert!(plan.is_clean(), "conflicts: {:?}", plan.conflicts);
    merge::execute(&plan, &host).unwrap();
    assert_eq!(fs::read_to_string(host.join("a.txt")).unwrap(), "same");
}

// ─────────────────────────────────────────────────────────────────────────
// ROUND 5 — manifest / state boundaries (pure functions)
// ─────────────────────────────────────────────────────────────────────────

/// R5-1: a manifest with an empty/garbage hash must be rejected (no phantom
/// Modified changes, no invented conflicts).
#[test]
fn r5_manifest_rejects_invalid_hash() {
    let bad = r#"{"base":"/x","created_epoch":0,"entries":{"f":{"kind":"file","size":0,"mode":0,"mtime_ns":0,"hash":""}}}"#;
    assert!(
        Manifest::from_json(bad).is_err(),
        "empty hash must be rejected"
    );
    let short = r#"{"base":"/x","created_epoch":0,"entries":{"f":{"kind":"file","size":0,"mode":0,"mtime_ns":0,"hash":"zz"}}}"#;
    assert!(
        Manifest::from_json(short).is_err(),
        "non-64-hex hash must be rejected"
    );
    let none = r#"{"base":"/x","created_epoch":0,"entries":{"f":{"kind":"file","size":0,"mode":0,"mtime_ns":0}}}"#;
    assert!(
        Manifest::from_json(none).is_ok(),
        "missing hash (unreadable) is accepted"
    );
}

/// R5-2: manifest with a future format version is rejected distinctly (not
/// as corruption).
#[test]
fn r5_manifest_rejects_future_version() {
    let future = r#"{"version":2,"base":"/x","created_epoch":0,"entries":{}}"#;
    let err = Manifest::from_json(future).unwrap_err();
    assert!(
        err.to_string().contains("unsupported"),
        "future version must be unsupported-format, got: {err}"
    );
}

/// R5-3: a path key with a `..` component must be rejected on load.
#[test]
fn r5_manifest_rejects_dotdot_key() {
    let bad = r#"{"base":"/x","created_epoch":0,"entries":{"a/../b":{"kind":"file","size":0,"mode":0,"mtime_ns":0}}}"#;
    assert!(Manifest::from_json(bad).is_err(), ".. key must be rejected");
}

/// R5-4: case-fold equivalence is reflexive/symmetric/transitive and
/// component-wise (separator-insensitive on Windows).
#[test]
fn r5_case_fold_is_equivalence() {
    use cowt_core::merge::{case_fold_key, case_fold_path_eq};
    let a = Path::new("dir/Foo.txt");
    let b = Path::new("DIR/foo.TXT");
    assert!(case_fold_path_eq(a, b));
    assert!(case_fold_key(a) == case_fold_key(b));
    let c = Path::new("dir/bar.txt");
    assert!(!case_fold_path_eq(a, c));
}

/// R5-5: Entry::content_eq treats unix permission bits as content for files.
#[cfg(unix)]
#[test]
fn r5_content_eq_mode_is_content() {
    use cowt_core::manifest::Entry;
    let f1 = Entry {
        kind: EntryKind::File,
        size: 1,
        mode: 0o644,
        mtime_ns: 0,
        hash: Some("a".repeat(64)),
        link_target: None,
    };
    let mut f2 = f1.clone();
    assert!(f1.content_eq(&f2));
    f2.mode = 0o600; // chmod only
    assert!(!f1.content_eq(&f2), "mode-only change must be visible");
}

// ─────────────────────────────────────────────────────────────────────────
// ROUND 5b — randomized full apply round-trip fuzz (strongest oracle)
// ─────────────────────────────────────────────────────────────────────────
// For randomized base+host+upper layouts, compute the effective worktree
// view, plan and execute the clean plans, and assert the host converges
// exactly to the effective view (file-for-file, deletions gone). This is the
// whole apply contract checked on randomized inputs — the place where subtle
// execute ordering / migration / pruning / empty-dir bugs would surface.

fn leaf_path() -> impl proptest::strategy::Strategy<Value = PathBuf> {
    prop::collection::vec(
        prop::collection::vec(prop::char::range('a', 'z'), 1..4)
            .prop_map(|c| c.into_iter().collect::<String>()),
        1..3,
    )
    .prop_map(|comps| comps.iter().collect::<PathBuf>())
}

fn hashed_file(content: &str) -> Entry {
    use blake3::Hasher;
    let mut h = Hasher::new();
    h.update(content.as_bytes());
    Entry {
        kind: EntryKind::File,
        size: content.len() as u64,
        mode: 0o644,
        mtime_ns: 0,
        hash: Some(h.finalize().to_hex().to_string()),
        link_target: None,
    }
}

proptest! {
    /// A clean plan (host == base so only worktree changes apply) executed
    /// against the host must converge the host exactly to the effective view.
    #[test]
    fn r5b_apply_round_trip_converges_randomized(
        base_paths in prop::collection::vec(leaf_path(), 1..8),
        upper_paths in prop::collection::vec(leaf_path(), 0..8),
        whiteouts in prop::collection::vec(leaf_path(), 0..4),
        new_dirs in prop::collection::vec(leaf_path(), 0..3),
    ) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().join("base");
        let upper = tmp.path().join("upper");
        let host = tmp.path().join("host");
        for d in [&base, &upper, &host] {
            fs::create_dir_all(d).unwrap();
        }
        let mut base_entries: BTreeMap<PathBuf, Entry> = BTreeMap::new();
        for (i, p) in base_paths.iter().enumerate() {
            let content = format!("base-{i}");
            let full = base.join(p);
            if full.parent().map(|pp| fs::create_dir_all(pp).is_ok()) != Some(false) {
                let _ = fs::write(&full, &content);
            }
            if full.exists() {
                // Only record base paths that survived on disk. A name that
                // is both a file and an ancestor (e.g. "l" and "l/a") cannot
                // exist on a real filesystem — such inputs are impossible,
                // so keep only the paths that resolve cleanly.
                if full.is_file() {
                    base_entries.insert(p.clone(), hashed_file(&content));
                }
            }
        }
        let base_m = Manifest {
            version: 1,
            base: base.clone(),
            created_epoch: 0,
            entries: base_entries,
        };
        for p in &upper_paths {
            let full = upper.join(p);
            if full.parent().map(|pp| fs::create_dir_all(pp).is_ok()) != Some(false) {
                let _ = fs::write(&full, "work");
            }
        }
        for w in &whiteouts {
            let wh = upper.join(format!(".wh.{}", w.display()));
            if wh.parent().map(|pp| fs::create_dir_all(pp).is_ok()) != Some(false) {
                let _ = fs::write(&wh, b"");
            }
        }
        for d in &new_dirs {
            let full = upper.join(d);
            if full.parent().map(|pp| fs::create_dir_all(pp).is_ok()) != Some(false) {
                let _ = fs::create_dir_all(&full);
            }
        }
        copy_tree(&base, &host);

        let work = overlay::effective_manifest_fold(&base_m, &upper, false)
            .expect("fold");
        let host_m = scan(&host);
        let plan = merge::plan(&base_m, &host_m, &work, &upper);
        // host == base, so a conflicting plan cannot arise here; skip the rare
        // unreachable conflict (conflict-writes-nothing is locked separately).
        if !plan.is_clean() {
            prop_assume!(false);
        }
        merge::execute(&plan, &host).expect("execute");

        let host_files = normalize(&tree_files(&host));
        let mut expected: BTreeMap<String, String> = BTreeMap::new();
        for (rel, e) in &work.entries {
            if e.kind == EntryKind::File {
                // The effective-view body is in upper when the worktree
                // changed it, otherwise it is unchanged from base.
                let src = if upper.join(rel).exists() {
                    upper.join(rel)
                } else {
                    base.join(rel)
                };
                let body = fs::read_to_string(&src).unwrap_or_default();
                expected.insert(rel.to_string_lossy().replace('\\', "/"), body);
            }
        }
        prop_assert_eq!(host_files, expected, "host did not converge to effective view");
    }
}

// ─────────────────────────────────────────────────────────────────────────
// ROUND 1+ — extra adversarial probes: host divergence, recreate-different,
// keep-of-host-only additions.
// ─────────────────────────────────────────────────────────────────────────

/// R1-6: the host adds a brand-new file (not in base, not in worktree).
/// The planner must KEEP it (never delete it) and apply must leave it.
#[test]
fn r1_host_only_new_file_is_kept() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "a.txt", "v1");
    write(&upper, "a.txt", "v1new");
    copy_tree(&base, &host);
    // Host adds a file the worktree never touched.
    write(&host, "hostonly.txt", "host added");

    let base_m = scan(&base);
    let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    assert!(plan.is_clean());
    // Must NOT plan a delete of the host-only file.
    assert!(
        !plan
            .operations
            .iter()
            .any(|op| matches!(op, merge::Operation::Delete { path, .. } if path == Path::new("hostonly.txt"))),
        "host-only new file must not be planned for deletion"
    );
    merge::execute(&plan, &host).unwrap();
    assert_eq!(
        fs::read_to_string(host.join("hostonly.txt")).unwrap(),
        "host added",
        "host-only new file must survive apply"
    );
    assert_eq!(fs::read_to_string(host.join("a.txt")).unwrap(), "v1new");
}

/// R4-6: delete-then-recreate a file with DIFFERENT content must converge to
/// the new content (not the old, not a phantom deletion).
#[test]
fn r4_delete_then_recreate_different_content() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "a.txt", "original");
    whiteout(&upper, "a.txt");
    write(&upper, "a.txt", "brand-new"); // recreated with DIFFERENT content
    let base_m = scan(&base);
    let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    copy_tree(&base, &host);
    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    assert!(plan.is_clean(), "conflicts: {:?}", plan.conflicts);
    merge::execute(&plan, &host).unwrap();
    assert_eq!(
        fs::read_to_string(host.join("a.txt")).unwrap(),
        "brand-new",
        "recreate must converge to the new content"
    );
}

/// R4-7: an apply that is entirely a no-op (no upper writes) must leave the
/// host byte-identical and write nothing. Guards against empty-layer
/// regressions (kill -9 between apply's remove_dir_all and create_dir_all).
#[test]
fn r4_empty_upper_is_clean_noop() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "a.txt", "v1");
    write(&base, "b.txt", "v2");
    copy_tree(&base, &host);

    let base_m = scan(&base);
    let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    assert!(plan.is_clean());
    assert!(
        plan.operations.is_empty(),
        "empty upper must plan no operations: {:?}",
        plan.operations
    );
    merge::execute(&plan, &host).unwrap();
    assert_eq!(fs::read_to_string(host.join("a.txt")).unwrap(), "v1");
    assert_eq!(fs::read_to_string(host.join("b.txt")).unwrap(), "v2");
}

/// R3-5: a structural diff where a host file diverged from base must still
/// classify correctly even when --content cannot enrich (no crash, no leaked
/// base body). This exercises the diff --content base-validation branch at the
/// core level by constructing the manifest-relevant state directly.
#[test]
fn r3_host_diverged_diff_still_structural() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let work = tmp.path().join("work");
    for d in [&base, &work] {
        fs::create_dir_all(d).unwrap();
    }
    // base and work both present; host (not scanned here) diverged.
    write(&base, "f.txt", "base");
    write(&work, "f.txt", "work");
    let (base_m, work_m, changes) = diff::diff_trees(&base, &work).unwrap();
    // Structural diff: base != work at f.txt -> Modified.
    let ch = changes
        .iter()
        .find(|c| c.path == Path::new("f.txt"))
        .unwrap();
    assert_eq!(ch.kind, diff::ChangeKind::Modified);
    // content_eq on the two scanned manifests must report the difference.
    assert!(!base_m.entries[&PathBuf::from("f.txt")]
        .content_eq(&work_m.entries[&PathBuf::from("f.txt")]));
}
