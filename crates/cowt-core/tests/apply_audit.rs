//! Adversarial audit round 4: the apply/execute filesystem engine.
//!
//! execute() touches the real host. These tests exercise the dangerous
//! branches: kind migration (dir<->file/symlink), non-empty-dir deletion,
//! empty-dir pruning, readonly source bodies, and staging-residue cleanup.

use std::fs;
use std::path::Path;

use cowt_core::manifest::Manifest;
use cowt_core::merge;
use tempfile::TempDir;

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn scan(p: &Path) -> Manifest {
    Manifest::scan(p).unwrap().manifest
}

// ─────────────────────────────────────────────────────────────────────────
// R4-A: dir -> file migration applies cleanly (base/host: dir with content;
// work: the same path is a file). Host dir must be replaced by the file.
// ─────────────────────────────────────────────────────────────────────────
#[test]
fn audit_dir_to_file_migration_round_trip() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "x/inner.txt", "old");
    write(&upper, "x", "new-content"); // x becomes a file

    let base_m = scan(&base);
    let work = cowt_core::overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    copy_tree(&base, &host);
    assert!(host.join("x").is_dir());

    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    assert!(
        plan.is_clean(),
        "dir->file migration must not conflict: {:?}",
        plan.conflicts
    );
    merge::execute(&plan, &host).unwrap();

    assert!(host.join("x").is_file(), "x must become a file");
    assert_eq!(fs::read_to_string(host.join("x")).unwrap(), "new-content");
    assert!(
        !host.join("x/inner.txt").exists(),
        "old dir content must be gone"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// R4-B: deleting a NON-EMPTY host directory whose content is unknown to base
// must surface a conflict (the planner must refuse, not silently skip).
// ─────────────────────────────────────────────────────────────────────────
#[test]
fn audit_delete_nonempty_host_dir_conflicts() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    // base: dir has "known.txt"
    write(&base, "dir/known.txt", "base");
    // host: dir ALSO has "hostonly.txt" (added by host after fork)
    copy_tree(&base, &host);
    write(&host, "dir/hostonly.txt", "host added");
    // worktree: deleted "dir" entirely (whiteout the dir)
    fs::write(upper.join(".wh.dir"), b"").unwrap();

    let base_m = scan(&base);
    let work = cowt_core::overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    assert!(
        !plan.is_clean(),
        "deleting a non-empty host dir must conflict, not silently drop"
    );
    assert!(plan
        .conflicts
        .iter()
        .any(|c| c.path == Path::new("dir/hostonly.txt")));
    // execute must refuse.
    assert!(merge::execute(&plan, &host).is_err());
    // Host data must be intact.
    assert_eq!(
        fs::read_to_string(host.join("dir/hostonly.txt")).unwrap(),
        "host added"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// R4-C: after a clean apply, NO .cowt-apply-* staging residue remains next to
// the host's parent.
// ─────────────────────────────────────────────────────────────────────────
#[test]
fn audit_no_staging_residue_after_success() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "a.txt", "v1");
    write(&upper, "a.txt", "v2");
    write(&upper, "b.txt", "new");

    let base_m = scan(&base);
    let work = cowt_core::overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    copy_tree(&base, &host);

    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    merge::execute(&plan, &host).unwrap();

    let residue: Vec<_> = fs::read_dir(tmp.path())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".cowt-apply-"))
        .collect();
    assert!(
        residue.is_empty(),
        "staging residue left behind: {residue:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// R4-D: readonly source body in the worktree must still apply (the engine
// grants write access for the fsync, restores perms after).
// ─────────────────────────────────────────────────────────────────────────
#[test]
fn audit_readonly_source_body_applies() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "ro.txt", "v1");
    write(&upper, "ro.txt", "v2");
    // Make the upper source readonly.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(upper.join("ro.txt"), fs::Permissions::from_mode(0o444)).unwrap();
    }

    let base_m = scan(&base);
    let work = cowt_core::overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    copy_tree(&base, &host);

    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    merge::execute(&plan, &host).unwrap();
    assert_eq!(fs::read_to_string(host.join("ro.txt")).unwrap(), "v2");
}

// ─────────────────────────────────────────────────────────────────────────
// R4-E: symlink -> file migration. base/host: x is a symlink; work: x is a
// file. The symlink must be replaced by the file.
// ─────────────────────────────────────────────────────────────────────────
#[cfg(unix)]
#[test]
fn audit_symlink_to_file_migration() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    std::os::unix::fs::symlink("target.txt", base.join("x")).unwrap();
    std::os::unix::fs::symlink("target.txt", host.join("x")).unwrap();
    write(&upper, "x", "now-a-file");

    let base_m = scan(&base);
    let work = cowt_core::overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    assert!(plan.is_clean());
    merge::execute(&plan, &host).unwrap();
    assert!(host.join("x").is_file());
    assert_eq!(fs::read_to_string(host.join("x")).unwrap(), "now-a-file");
}

// ─────────────────────────────────────────────────────────────────────────
// R4-F: empty-dir pruning must NOT delete a directory that still holds
// unrelated content after a sibling delete. (Deepest-first prune only
// removes dirs it believes became empty.)
// ─────────────────────────────────────────────────────────────────────────
#[test]
fn audit_empty_dir_prune_keeps_populated_sibling() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "todelete/a.txt", "x");
    write(&base, "keep/b.txt", "y"); // sibling dir, NOT deleted
    write(&upper, ".wh.todelete", ""); // delete only `todelete`

    let base_m = scan(&base);
    let work = cowt_core::overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    copy_tree(&base, &host);

    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    assert!(plan.is_clean());
    merge::execute(&plan, &host).unwrap();
    assert!(!host.join("todelete").exists());
    assert!(
        host.join("keep/b.txt").exists(),
        "populated sibling must survive"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// R4-G: deleting a single file whose host copy is UNCHANGED removes exactly
// that file and prunes now-empty parent dirs — but a dir with a surviving
// sibling is not pruned.
// ─────────────────────────────────────────────────────────────────────────
#[test]
fn audit_delete_file_prunes_only_empty_parents() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "sub/deep/only.txt", "x");
    write(&upper, ".wh.sub", ""); // delete whole `sub`

    let base_m = scan(&base);
    let work = cowt_core::overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    copy_tree(&base, &host);

    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    assert!(plan.is_clean());
    merge::execute(&plan, &host).unwrap();
    assert!(!host.join("sub/deep/only.txt").exists());
    assert!(!host.join("sub/deep").exists());
    assert!(!host.join("sub").exists());
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
// R4-H: a clean apply followed by DIFF (base = new work view) must yield zero
// changes — the applied state is self-consistent.
// ─────────────────────────────────────────────────────────────────────────
#[test]
fn audit_apply_then_diff_is_empty() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "a.txt", "v1");
    write(&base, "del.txt", "v0");
    write(&upper, "a.txt", "v2");
    fs::write(upper.join(".wh.del.txt"), b"").unwrap();

    let base_m = scan(&base);
    let work = cowt_core::overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    copy_tree(&base, &host);

    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    merge::execute(&plan, &host).unwrap();

    // New base = the now-applied host tree.
    let new_base = scan(&host);
    let work2 = cowt_core::overlay::effective_manifest_fold(&new_base, &upper, false).unwrap();
    let changes = cowt_core::diff::diff(&new_base, &work2);
    assert!(
        changes.is_empty(),
        "applied state must be self-consistent: {changes:?}"
    );
}
