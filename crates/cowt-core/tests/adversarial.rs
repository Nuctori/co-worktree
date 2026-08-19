//! Adversarial audits for the cowt-core engine.
//!
//! Each test is an independent black-box probe of a correctness invariant.
//! The central oracle is the APPLY ROUND-TRIP: after `merge::execute`, the
//! host directory tree must converge exactly to the worktree's effective view
//! (the result of `overlay::effective_manifest_fold`). Any divergence is a
//! real bug — either data is lost or phantom changes survive.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use cowt_core::diff;
use cowt_core::manifest::Manifest;
use cowt_core::merge;
use cowt_core::overlay;
use tempfile::TempDir;

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn whiteout(upper: &Path, name: &str) {
    // Portable `.wh.` zero-size regular-file encoding (fuse-overlayfs fallback).
    let p = upper.join(format!(".wh.{name}"));
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, b"").unwrap();
}

fn opaque(upper: &Path, dir: &str) {
    // Opaque marker: shadows the whole base subtree below `dir`.
    let p = upper.join(dir).join(".wh..wh..opq");
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, b"").unwrap();
}

fn scan(p: &Path) -> Manifest {
    Manifest::scan(p).unwrap().manifest
}

/// Recursively list every file (relative path -> content) under `root`.
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

// ─────────────────────────────────────────────────────────────────────────
// AUDIT 1: apply round-trip with a rich mixed fixture (modify/add/delete/
// dir-delete/opaque + add). Host starts equal to base (the normal fork→run→
// apply workflow). The host tree must converge exactly to the effective view.
// ─────────────────────────────────────────────────────────────────────────
#[test]
fn audit_apply_round_trip_converges_to_effective_view() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }

    // Base tree.
    write(&base, "a.txt", "v1");
    write(&base, "keep.txt", "v3");
    write(&base, "sub/b.txt", "v2");
    write(&base, "del.txt", "vdelete");
    write(&base, "dir/old.txt", "vold");
    write(&base, "dir2/keep_base.txt", "vb");

    // Upper (worktree) mutations over the base.
    write(&upper, "a.txt", "v1new"); // modify
    write(&upper, "newfile.txt", "new"); // add
    write(&upper, "sub/new2.txt", "n2"); // add under existing dir
    write(&upper, "sub/b.txt", "v2new"); // modify nested
    whiteout(&upper, "del.txt"); // delete file
    whiteout(&upper, "dir"); // delete whole dir (and subtree)
    opaque(&upper, "dir2"); // shadow dir2's base content...
    write(&upper, "dir2/added.txt", "added"); // ...but keep a new file in it

    let base_m = scan(&base);
    let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    // Host starts exactly as base (no host changes yet).
    copy_tree(&base, &host);

    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    assert!(
        plan.is_clean(),
        "unexpected conflicts: {:?}",
        plan.conflicts
    );
    merge::execute(&plan, &host).unwrap();

    // Host must now equal the effective worktree view, file-for-file.
    let host_files = tree_files(&host);
    // Normalize paths to forward-slash keys so the comparison is
    // platform-independent (Windows uses backslash on disk).
    let normalize = |m: &BTreeMap<PathBuf, String>| -> BTreeMap<String, String> {
        m.iter()
            .map(|(k, v)| (k.to_string_lossy().replace('\\', "/"), v.clone()))
            .collect()
    };
    let host_files = normalize(&host_files);
    let mut expected: BTreeMap<String, String> = BTreeMap::new();
    expected.insert("a.txt".into(), "v1new".into());
    expected.insert("keep.txt".into(), "v3".into());
    expected.insert("sub/b.txt".into(), "v2new".into());
    expected.insert("sub/new2.txt".into(), "n2".into());
    expected.insert("newfile.txt".into(), "new".into());
    expected.insert("dir2/added.txt".into(), "added".into());
    assert_eq!(
        host_files, expected,
        "host did not converge to effective view"
    );
    // Explicitly: deleted paths must be GONE, not merely empty.
    assert!(!host.join("del.txt").exists());
    assert!(!host.join("dir").exists());
    assert!(!host.join("dir/old.txt").exists());
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

// ─────────────────────────────────────────────────────────────────────────
// AUDIT 2: re-applying must be a no-op (idempotence). After a clean apply,
// plan(base=work_now, current=work_now, work=work) must produce zero ops and
// leave the host byte-identical.
// ─────────────────────────────────────────────────────────────────────────
#[test]
fn audit_apply_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "a.txt", "v1");
    write(&base, "b.txt", "v2");
    write(&upper, "a.txt", "v1new");

    let base_m = scan(&base);
    let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    copy_tree(&base, &host);

    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    merge::execute(&plan, &host).unwrap();
    let after_first = tree_files(&host);

    // Re-plan against the now-applied host.
    let host_m = scan(&host);
    let plan2 = merge::plan(&host_m, &host_m, &work, &upper);
    assert!(plan2.is_clean());
    assert!(
        plan2.operations.is_empty(),
        "re-apply must be a no-op: {:?}",
        plan2.operations
    );
    merge::execute(&plan2, &host).unwrap();
    assert_eq!(tree_files(&host), after_first, "host changed on re-apply");
}

// ─────────────────────────────────────────────────────────────────────────
// AUDIT 3: structural diff is the exact set of paths where base != work.
// For every path, diff reports it iff the byte/meta view differs.
// ─────────────────────────────────────────────────────────────────────────
#[test]
fn audit_diff_is_exact_set_of_differing_paths() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let work = tmp.path().join("work");
    for d in [&base, &work] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "same.txt", "x");
    write(&work, "same.txt", "x");
    write(&base, "mod.txt", "old");
    write(&work, "mod.txt", "new");
    write(&base, "del.txt", "gone");
    write(&work, "add.txt", "fresh");

    let (base_m, work_m, changes) = diff::diff_trees(&base, &work).unwrap();
    let changed: std::collections::BTreeSet<PathBuf> =
        changes.iter().map(|c| c.path.clone()).collect();

    // Brute-force the ground truth by comparing entries one by one.
    let mut truth = std::collections::BTreeSet::new();
    for (p, w) in &work_m.entries {
        match base_m.entries.get(p) {
            None => {
                truth.insert(p.clone());
            }
            Some(b) => {
                if !b.content_eq(w) {
                    truth.insert(p.clone());
                }
            }
        }
    }
    for (p, b) in &base_m.entries {
        if !work_m.entries.contains_key(p) {
            truth.insert(p.clone());
        }
        let _ = b;
    }
    assert_eq!(changed, truth, "structural diff diverges from ground truth");
}

// ─────────────────────────────────────────────────────────────────────────
// AUDIT 4: opaque marker must shadow base subtree EXCEPT base entries that
// are explicitly re-created in the upper layer.
// ─────────────────────────────────────────────────────────────────────────
#[test]
fn audit_opaque_shadows_base_except_readds() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    fs::create_dir_all(&base).unwrap();
    fs::create_dir_all(&upper).unwrap();
    write(&base, "op/sub/keep_base.txt", "vb");
    write(&base, "op/sub/drop_base.txt", "vd");

    opaque(&upper, "op"); // shadow everything below op/ ...
    write(&upper, "op/sub/keep_base.txt", "vb2"); // ...but this re-add survives
                                                  // Note: drop_base.txt is not re-added -> must be shadowed.

    let base_m = scan(&base);
    let eff = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    assert!(eff.get(Path::new("op/sub/keep_base.txt")).is_some());
    assert_eq!(eff.get(Path::new("op/sub/keep_base.txt")).unwrap().size, 3);
    assert!(
        eff.get(Path::new("op/sub/drop_base.txt")).is_none(),
        "opaque must shadow un-re-added base entry"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// AUDIT 5: directory whiteout must shadow the ENTIRE base subtree below it,
// not just the directory entry itself.
// ─────────────────────────────────────────────────────────────────────────
#[test]
fn audit_dir_whiteout_shadows_whole_subtree() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    fs::create_dir_all(&base).unwrap();
    fs::create_dir_all(&upper).unwrap();
    write(&base, "big/deep/a.txt", "a");
    write(&base, "big/deep/b.txt", "b");
    write(&base, "big/c.txt", "c");

    whiteout(&upper, "big"); // delete the whole `big` directory

    let base_m = scan(&base);
    let eff = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    assert!(eff.get(Path::new("big")).is_none());
    assert!(eff.get(Path::new("big/deep/a.txt")).is_none());
    assert!(eff.get(Path::new("big/deep/b.txt")).is_none());
    assert!(eff.get(Path::new("big/c.txt")).is_none());
}

// ─────────────────────────────────────────────────────────────────────────
// AUDIT 6: TOCTOU guard — if the host file changes between planning and
// execution, execute must abort and leave the host untouched.
// ─────────────────────────────────────────────────────────────────────────
#[test]
fn audit_verify_unchanged_aborts_on_host_change() {
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

    // Simulate a host edit landing between plan and execute.
    write(&host, "a.txt", "HOST-CHANGED-OUT-OF-BAND");

    let err = merge::execute(&plan, &host).unwrap_err();
    assert!(
        err.to_string().contains("changed after planning")
            || err.to_string().contains("host path changed"),
        "execute must abort on TOCTOU host change, got: {err}"
    );
    // Host file must remain the out-of-band edit, not the staged v1new.
    assert_eq!(
        fs::read_to_string(host.join("a.txt")).unwrap(),
        "HOST-CHANGED-OUT-OF-BAND"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// AUDIT 7: a conflict must write NOTHING to the host (atomicity of intent).
// Even an unrelated clean path must not be touched.
// ─────────────────────────────────────────────────────────────────────────
#[test]
fn audit_conflict_writes_nothing() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "conflict.txt", "base");
    write(&base, "clean.txt", "clean-base");
    write(&host, "conflict.txt", "host"); // host moved on
    write(&host, "clean.txt", "clean-base");
    write(&upper, "conflict.txt", "work"); // worktree moved on -> conflict
    write(&upper, "clean.txt", "clean-work"); // clean apply

    let base_m = scan(&base);
    let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    assert_eq!(plan.conflicts.len(), 1);

    let err = merge::execute(&plan, &host).unwrap_err();
    assert!(err.to_string().contains("conflict"));
    // Neither file must be touched.
    assert_eq!(
        fs::read_to_string(host.join("conflict.txt")).unwrap(),
        "host"
    );
    assert_eq!(
        fs::read_to_string(host.join("clean.txt")).unwrap(),
        "clean-base"
    );
    // No staging residue.
    let leftovers: Vec<_> = fs::read_dir(tmp.path())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".cowt-apply-"))
        .collect();
    assert!(leftovers.is_empty());
}

// ─────────────────────────────────────────────────────────────────────────
// AUDIT 8: kind migration file->dir must apply cleanly (delete file, create
// dir + child) and converge.
// ─────────────────────────────────────────────────────────────────────────
#[test]
fn audit_file_to_dir_migration_round_trip() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "x", "old");
    write(&upper, "x/inner.txt", "new"); // x becomes a dir

    let base_m = scan(&base);
    let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    copy_tree(&base, &host);
    // Host has `x` as a file currently.
    assert!(host.join("x").is_file());

    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    assert!(
        plan.is_clean(),
        "file->dir migration must not conflict: {:?}",
        plan.conflicts
    );
    merge::execute(&plan, &host).unwrap();

    assert!(host.join("x").is_dir());
    assert_eq!(fs::read_to_string(host.join("x/inner.txt")).unwrap(), "new");
    assert!(!host.join("x").is_file());
}

// ─────────────────────────────────────────────────────────────────────────
// AUDIT 9: empty-directory addition round-trip. An explicitly created empty
// dir in the worktree must appear on the host after apply.
// ─────────────────────────────────────────────────────────────────────────
#[test]
fn audit_empty_dir_addition_round_trip() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "a.txt", "v1");
    fs::create_dir_all(upper.join("emptydir")).unwrap();

    let base_m = scan(&base);
    let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    assert!(work.get(Path::new("emptydir")).is_some());
    copy_tree(&base, &host);

    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    assert!(plan.is_clean());
    merge::execute(&plan, &host).unwrap();
    assert!(
        host.join("emptydir").is_dir(),
        "empty dir must be created on host"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// AUDIT 10: worktree deletion of a file whose host copy is byte-identical to
// base must still actually remove it from the host (regression for a "kept"
// mislabel: work==base must not win when work DELETED the file).
// ─────────────────────────────────────────────────────────────────────────
#[test]
fn audit_delete_with_untouched_host_removes_file() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "gone.txt", "same");
    whiteout(&upper, "gone.txt"); // worktree deleted it (no recreated copy)

    let base_m = scan(&base);
    let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    assert!(
        work.get(Path::new("gone.txt")).is_none(),
        "whiteout must remove base entry"
    );
    copy_tree(&base, &host);

    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    assert!(
        plan.is_clean(),
        "clean delete must not conflict: {:?}",
        plan.conflicts
    );
    merge::execute(&plan, &host).unwrap();
    assert!(
        !host.join("gone.txt").exists(),
        "deleted file must be removed from host"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// AUDIT 11: an added file inside a directory that is ALSO added must apply
// (parent Mkdir must precede the child WriteFile).
// ─────────────────────────────────────────────────────────────────────────
#[test]
fn audit_nested_add_under_new_dir() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "a.txt", "v1");
    fs::create_dir_all(upper.join("newdir/sub")).unwrap();
    write(&upper, "newdir/sub/deep.txt", "deep");

    let base_m = scan(&base);
    let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    copy_tree(&base, &host);

    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    assert!(
        plan.is_clean(),
        "nested add must not conflict: {:?}",
        plan.conflicts
    );
    merge::execute(&plan, &host).unwrap();
    assert_eq!(
        fs::read_to_string(host.join("newdir/sub/deep.txt")).unwrap(),
        "deep"
    );
}
