//! Deep adversarial rounds 4 & 5: differential (base != current != work)
//! three-way merge, verify_unchanged forged-metadata guard, migration-delete
//! ordering, and an apply-then-apply chain.

use std::fs;
use std::path::{Path, PathBuf};

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

/// D-1: full differential merge where base != current != work. A path the
/// host DELETED and the worktree MODIFIED -> DeleteVsModify conflict; a
/// host-moved/work-untouched path -> kept; a clean apply refuses.
#[test]
fn d1_full_differential_merge_keeps_and_conflicts() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let host = tmp.path().join("host");
    let upper = tmp.path().join("upper");
    for d in [&base, &host, &upper] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "keep.txt", "base");
    write(&base, "mod.txt", "base");
    write(&base, "del.txt", "base");
    copy_tree(&base, &host);
    write(&host, "keep.txt", "HOST-KEEP");
    write(&host, "mod.txt", "HOST-MOD");
    fs::remove_file(host.join("del.txt")).unwrap();
    write(&upper, "mod.txt", "WORK-MOD");
    write(&upper, "conf.txt", "WORK-CONF");
    write(&upper, "new.txt", "WORK-NEW");
    write(&upper, "del.txt", "WORK-DEL");
    let base_m = scan(&base);
    let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    let host_m = scan(&host);
    let plan = merge::plan(&base_m, &host_m, &work, &upper);
    assert!(
        plan.conflicts.iter().any(
            |c| c.path == Path::new("del.txt") && c.kind == merge::ConflictKind::DeleteVsModify
        ),
        "del.txt must conflict: {:?}",
        plan.conflicts
    );
    assert!(
        plan.kept.contains(&PathBuf::from("keep.txt")),
        "keep.txt must be kept"
    );
    assert!(!plan.is_clean());
    let err = merge::execute(&plan, &host).unwrap_err();
    assert!(err.to_string().contains("conflict"));
    assert_eq!(
        fs::read_to_string(host.join("keep.txt")).unwrap(),
        "HOST-KEEP"
    );
    assert_eq!(
        fs::read_to_string(host.join("mod.txt")).unwrap(),
        "HOST-MOD"
    );
}

/// D-2: verify_unchanged catches a forged host file (same size+mtime,
/// different content) via the hash re-check. Plan first (host == base), then
/// forge, then execute.
#[test]
fn d2_verify_unchanged_catches_forged_content() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "f.txt", "AAAA\n");
    write(&upper, "f.txt", "BBBB\n");
    copy_tree(&base, &host);
    let base_m = scan(&base);
    let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    assert!(plan.is_clean());
    // Forge: same 4-byte size, different content, mtime aligned to base.
    write(&host, "f.txt", "ZZZZ\n");
    let base_meta = fs::metadata(base.join("f.txt")).unwrap();
    let _ = fs::OpenOptions::new()
        .write(true)
        .open(host.join("f.txt"))
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(base_meta.modified().unwrap()));
    let err = merge::execute(&plan, &host).unwrap_err();
    assert!(
        err.to_string().contains("host path changed")
            || err.to_string().contains("changed after planning"),
        "forged host file must abort: {err}"
    );
    assert_eq!(fs::read_to_string(host.join("f.txt")).unwrap(), "ZZZZ\n");
}

/// D-3: file -> dir migration deletes the old file BEFORE creating the dir.
#[test]
fn d3_file_to_dir_migration_delete_ordering() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "x", "old");
    write(&upper, "x/inner.txt", "new");
    let base_m = scan(&base);
    let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    copy_tree(&base, &host);
    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    assert!(plan.is_clean(), "file->dir clean: {:?}", plan.conflicts);
    let del_idx = plan.operations.iter().position(|o| {
        matches!(
        o, merge::Operation::Delete { path, migration: true } if path == Path::new("x"))
    });
    let mkdir_idx = plan.operations.iter().position(|o| {
        matches!(
        o, merge::Operation::Mkdir { path, .. } if path == Path::new("x"))
    });
    assert!(del_idx.is_some() && mkdir_idx.is_some());
    assert!(
        del_idx.unwrap() < mkdir_idx.unwrap(),
        "migration delete before mkdir"
    );
    merge::execute(&plan, &host).unwrap();
    assert!(host.join("x").is_dir());
    assert_eq!(fs::read_to_string(host.join("x/inner.txt")).unwrap(), "new");
}

/// D-4: apply chain — apply, then a second change applied again, converges.
#[test]
fn d4_apply_then_apply_again_converges() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper1 = tmp.path().join("upper1");
    let upper2 = tmp.path().join("upper2");
    let host = tmp.path().join("host");
    for d in [&base, &upper1, &upper2, &host] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "a.txt", "v1");
    write(&base, "b.txt", "v2");
    write(&upper1, "a.txt", "v1a");
    write(&upper1, "c.txt", "new-c");
    write(&upper2, "b.txt", "v2b");
    whiteout(&upper2, "c.txt");
    let base_m = scan(&base);
    let work1 = overlay::effective_manifest_fold(&base_m, &upper1, false).unwrap();
    copy_tree(&base, &host);
    merge::execute(&merge::plan(&base_m, &scan(&host), &work1, &upper1), &host).unwrap();
    assert_eq!(fs::read_to_string(host.join("a.txt")).unwrap(), "v1a");
    assert_eq!(fs::read_to_string(host.join("c.txt")).unwrap(), "new-c");
    let current = scan(&host);
    let work2 = overlay::effective_manifest_fold(&current, &upper2, false).unwrap();
    let plan2 = merge::plan(&current, &scan(&host), &work2, &upper2);
    assert!(
        plan2.is_clean(),
        "second apply clean: {:?}",
        plan2.conflicts
    );
    merge::execute(&plan2, &host).unwrap();
    assert_eq!(fs::read_to_string(host.join("b.txt")).unwrap(), "v2b");
    assert!(!host.join("c.txt").exists(), "second change deleted c.txt");
}

/// D-5: empty-dir addition round-trip.
#[test]
fn d5_empty_dir_addition_round_trip() {
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
    assert!(host.join("emptydir").is_dir());
}

/// D-6: dir->file migration where the old dir has a host-only child must
/// conflict rather than silently overwrite.
#[test]
fn d6_dir_to_file_migration_with_host_only_child_conflicts() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "x/known.txt", "base");
    copy_tree(&base, &host);
    write(&host, "x/hostonly.txt", "host added");
    write(&upper, "x", "now-a-file");
    let base_m = scan(&base);
    let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    assert!(
        !plan.is_clean(),
        "non-empty host dir migration must conflict"
    );
    assert!(
        plan.conflicts
            .iter()
            .any(|c| c.path == Path::new("x/hostonly.txt")),
        "host-only child conflict: {:?}",
        plan.conflicts
    );
}
