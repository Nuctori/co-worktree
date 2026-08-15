//! Unit tests for the cowt-core engine: manifest, diff, overlay, merge.

use std::fs;
use std::path::Path;

use cowt_core::diff;
use cowt_core::manifest::{EntryKind, Manifest};
use cowt_core::merge;
use cowt_core::overlay;
use tempfile::TempDir;

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

// ---------------------------------------------------------------- manifest

#[test]
fn manifest_scans_files_dirs_and_hashes() {
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), "a/b/c.txt", "hello");
    write(tmp.path(), "top.json", "{}");

    let m = Manifest::scan(tmp.path()).unwrap().manifest;
    let e = m.get(Path::new("a/b/c.txt")).unwrap();
    assert_eq!(e.kind, EntryKind::File);
    assert_eq!(e.size, 5);
    assert!(e.hash.is_some());
    assert_eq!(m.get(Path::new("a/b")).unwrap().kind, EntryKind::Dir);
}

#[cfg(unix)]
#[test]
fn manifest_never_follows_symlinks() {
    let tmp = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    write(outside.path(), "secret.txt", "outside-content");
    write(tmp.path(), "real.txt", "inside");
    std::os::unix::fs::symlink(outside.path(), tmp.path().join("escape")).unwrap();
    std::os::unix::fs::symlink("real.txt", tmp.path().join("rel-link")).unwrap();

    let m = Manifest::scan(tmp.path()).unwrap().manifest;
    // The symlink to the outside directory must be a leaf entry, not a tree.
    let esc = m.get(Path::new("escape")).unwrap();
    assert_eq!(esc.kind, EntryKind::Symlink);
    assert!(m.get(Path::new("escape/secret.txt")).is_none());
    assert_eq!(
        m.get(Path::new("rel-link")).unwrap().kind,
        EntryKind::Symlink
    );
    // And the outside file must not appear anywhere.
    assert!(!m
        .entries
        .keys()
        .any(|p| p.to_string_lossy().contains("secret")));
}

#[test]
fn manifest_rescan_reuses_hashes_for_untouched_files() {
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), "keep.txt", "stable");
    let first = Manifest::scan(tmp.path()).unwrap().manifest;
    write(tmp.path(), "new.txt", "fresh");
    let second = Manifest::rescan(tmp.path(), &first).unwrap().manifest;
    assert_eq!(
        first.get(Path::new("keep.txt")).unwrap().hash,
        second.get(Path::new("keep.txt")).unwrap().hash
    );
    assert!(second.get(Path::new("new.txt")).unwrap().hash.is_some());
}

#[test]
fn manifest_scales_to_many_files() {
    let tmp = TempDir::new().unwrap();
    for i in 0..2000 {
        write(
            tmp.path(),
            &format!("dir{}/file{}.txt", i % 20, i),
            "payload",
        );
    }
    let start = std::time::Instant::now();
    let m = Manifest::scan(tmp.path()).unwrap().manifest;
    let elapsed = start.elapsed();
    assert_eq!(
        m.entries
            .values()
            .filter(|e| e.kind == EntryKind::File)
            .count(),
        2000
    );
    // Generous bound: CI runners are slow; production target is far below.
    assert!(elapsed.as_secs() < 20, "scan too slow: {elapsed:?}");
}

// ---------------------------------------------------------------- diff

#[test]
fn diff_detects_added_modified_deleted() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let work = tmp.path().join("work");
    fs::create_dir_all(&base).unwrap();
    fs::create_dir_all(&work).unwrap();

    write(&base, "keep.txt", "same");
    write(&base, "mod.txt", "old content");
    write(&base, "del.txt", "gone");

    write(&work, "keep.txt", "same");
    write(&work, "mod.txt", "new content");
    write(&work, "add.txt", "brand new");

    let (_, _, changes) = diff::diff_trees(&base, &work).unwrap();
    let get = |name: &str| changes.iter().find(|c| c.path == Path::new(name)).unwrap();
    assert_eq!(get("add.txt").kind, diff::ChangeKind::Added);
    assert_eq!(get("mod.txt").kind, diff::ChangeKind::Modified);
    assert_eq!(get("del.txt").kind, diff::ChangeKind::Deleted);
    assert!(changes.iter().all(|c| c.path != Path::new("keep.txt")));
}

#[test]
fn diff_produces_myers_line_diff_for_text() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let work = tmp.path().join("work");
    fs::create_dir_all(&base).unwrap();
    fs::create_dir_all(&work).unwrap();
    write(&base, "f.txt", "line1\nline2\nline3\n");
    write(&work, "f.txt", "line1\nline2 changed\nline3\nline4\n");

    let (_, _, changes) = diff::diff_trees(&base, &work).unwrap();
    let ch = &changes[0];
    match ch.detail.as_ref().unwrap() {
        diff::ContentDiff::Text { unified } => {
            assert!(unified.contains("-line2"));
            assert!(unified.contains("+line2 changed"));
            assert!(unified.contains("+line4"));
            assert!(!unified.contains("-line1"));
        }
        other => panic!("expected text diff, got {other:?}"),
    }
}

#[test]
fn diff_produces_key_level_diff_for_json() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let work = tmp.path().join("work");
    fs::create_dir_all(&base).unwrap();
    fs::create_dir_all(&work).unwrap();
    write(
        &base,
        "settings.json",
        r#"{"font": 12, "theme": "dark", "nested": {"a": 1, "b": 2}}"#,
    );
    write(
        &work,
        "settings.json",
        r#"{"font": 14, "theme": "dark", "nested": {"a": 1, "c": 3}}"#,
    );

    let (_, _, changes) = diff::diff_trees(&base, &work).unwrap();
    match changes[0].detail.as_ref().unwrap() {
        diff::ContentDiff::Keys { changes: keys } => {
            let font = keys.iter().find(|k| k.key == "font").unwrap();
            assert_eq!(font.kind, diff::ChangeKind::Modified);
            assert_eq!(font.old.as_deref(), Some("12"));
            assert_eq!(font.new.as_deref(), Some("14"));
            assert_eq!(
                keys.iter().find(|k| k.key == "nested.b").unwrap().kind,
                diff::ChangeKind::Deleted
            );
            assert_eq!(
                keys.iter().find(|k| k.key == "nested.c").unwrap().kind,
                diff::ChangeKind::Added
            );
            // Unchanged keys must not be reported.
            assert!(keys.iter().all(|k| k.key != "theme" && k.key != "nested.a"));
        }
        other => panic!("expected key diff, got {other:?}"),
    }
}

#[test]
fn diff_produces_key_level_diff_for_yaml() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let work = tmp.path().join("work");
    fs::create_dir_all(&base).unwrap();
    fs::create_dir_all(&work).unwrap();
    write(
        &base,
        "app.yaml",
        "server:\n  port: 8080\n  host: localhost\n",
    );
    write(
        &work,
        "app.yaml",
        "server:\n  port: 9090\n  host: localhost\n",
    );

    let (_, _, changes) = diff::diff_trees(&base, &work).unwrap();
    match changes[0].detail.as_ref().unwrap() {
        diff::ContentDiff::Keys { changes: keys } => {
            assert_eq!(keys.len(), 1);
            assert_eq!(keys[0].key, "server.port");
            assert_eq!(keys[0].new.as_deref(), Some("9090"));
        }
        other => panic!("expected key diff, got {other:?}"),
    }
}

#[test]
fn diff_marks_binary_files_as_binary() {
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

// ---------------------------------------------------------------- overlay

/// fuse-overlayfs encodes whiteouts as zero-size regular files when it cannot
/// mknod (unprivileged), and as char devices 0:0 otherwise. Test the portable
/// encoding here; both are accepted by `is_whiteout`.
#[test]
fn overlay_folds_whiteouts_into_base() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    fs::create_dir_all(&base).unwrap();
    fs::create_dir_all(&upper).unwrap();
    write(&base, "keep.txt", "same");
    write(&base, "deleted.txt", "gone");
    write(&base, "mod.txt", "old");
    write(&upper, "mod.txt", "new");
    write(&upper, "added.txt", "new file");
    // Zero-size regular-file whiteout (unprivileged fuse-overlayfs encoding).
    fs::write(upper.join(".wh.deleted.txt"), b"").unwrap();

    let base_m = Manifest::scan(&base).unwrap().manifest;
    let effective = overlay::effective_manifest(&base_m, &upper).unwrap();
    assert!(effective.get(Path::new("deleted.txt")).is_none());
    assert!(effective.get(Path::new("keep.txt")).is_some());
    assert!(effective.get(Path::new("added.txt")).is_some());
    let mod_entry = effective.get(Path::new("mod.txt")).unwrap();
    let base_mod = base_m.get(Path::new("mod.txt")).unwrap();
    assert_ne!(mod_entry.hash, base_mod.hash);
}

#[cfg(unix)]
#[test]
fn overlay_folds_chardev_whiteouts_when_mknod_available() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    fs::create_dir_all(&base).unwrap();
    fs::create_dir_all(&upper).unwrap();
    write(&base, "victim.txt", "gone");

    // Kernel-style encoding: char device 0:0 carrying the victim's own name.
    if !try_mknod_whiteout(&upper.join("victim.txt")) {
        eprintln!("mknod unavailable, skipping char-device whiteout test");
        return;
    }
    let base_m = Manifest::scan(&base).unwrap().manifest;
    let effective = overlay::effective_manifest(&base_m, &upper).unwrap();
    assert!(effective.get(Path::new("victim.txt")).is_none());
}

#[cfg(unix)]
fn try_mknod_whiteout(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // S_IFCHR | 0600, dev 0
    unsafe { libc_mknod(c.as_ptr(), 0o0020000 | 0o600, 0) == 0 }
}

#[cfg(unix)]
unsafe fn libc_mknod(path: *const std::os::raw::c_char, mode: u32, dev: u64) -> i32 {
    extern "C" {
        fn mknod(path: *const std::os::raw::c_char, mode: u32, dev: u64) -> i32;
    }
    unsafe { mknod(path, mode, dev) }
}

// ---------------------------------------------------------------- merge

fn scan(p: &Path) -> Manifest {
    Manifest::scan(p).unwrap().manifest
}

#[test]
fn merge_applies_when_host_untouched() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let host = tmp.path().join("host");
    let work = tmp.path().join("work");
    for d in [&base, &host, &work] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "a.txt", "v1");
    write(&host, "a.txt", "v1");
    write(&work, "a.txt", "v2");
    write(&work, "new.txt", "created");

    let plan = merge::plan(&scan(&base), &scan(&host), &scan(&work), &work);
    assert!(plan.is_clean(), "conflicts: {:?}", plan.conflicts);
    let report = merge::execute(&plan, &host).unwrap();
    assert_eq!(report.written, 2);
    assert_eq!(fs::read_to_string(host.join("a.txt")).unwrap(), "v2");
    assert_eq!(fs::read_to_string(host.join("new.txt")).unwrap(), "created");
}

#[test]
fn merge_keeps_host_changes_when_worktree_untouched() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let host = tmp.path().join("host");
    let work = tmp.path().join("work");
    for d in [&base, &host, &work] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "a.txt", "v1");
    write(&host, "a.txt", "host-moved");
    write(&work, "a.txt", "v1");

    let plan = merge::plan(&scan(&base), &scan(&host), &scan(&work), &work);
    assert!(plan.is_clean());
    assert_eq!(plan.kept, vec![std::path::PathBuf::from("a.txt")]);
    merge::execute(&plan, &host).unwrap();
    assert_eq!(
        fs::read_to_string(host.join("a.txt")).unwrap(),
        "host-moved"
    );
}

#[test]
fn merge_conflict_writes_nothing() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let host = tmp.path().join("host");
    let work = tmp.path().join("work");
    for d in [&base, &host, &work] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "conflict.txt", "base");
    write(&base, "clean.txt", "clean-base");
    write(&host, "conflict.txt", "host");
    write(&host, "clean.txt", "clean-base");
    write(&work, "conflict.txt", "work");
    write(&work, "clean.txt", "clean-work");

    let plan = merge::plan(&scan(&base), &scan(&host), &scan(&work), &work);
    assert_eq!(plan.conflicts.len(), 1);
    let c = &plan.conflicts[0];
    assert_eq!(c.path, std::path::PathBuf::from("conflict.txt"));
    assert_eq!(c.kind, merge::ConflictKind::BothModified);
    assert!(c.base_hash.is_some() && c.current_hash.is_some() && c.work_hash.is_some());

    // execute must refuse and write NOTHING, not even the clean file.
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
    // No staging leftovers.
    let leftovers: Vec<_> = fs::read_dir(tmp.path())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".cowt-apply-"))
        .collect();
    assert!(leftovers.is_empty());
}

#[test]
fn merge_delete_vs_modify_conflicts() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let host = tmp.path().join("host");
    let work = tmp.path().join("work");
    for d in [&base, &host, &work] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "x.txt", "base");
    write(&host, "x.txt", "host-edited");
    // worktree deleted x.txt (absent in work).

    let plan = merge::plan(&scan(&base), &scan(&host), &scan(&work), &work);
    assert_eq!(plan.conflicts.len(), 1);
    assert_eq!(plan.conflicts[0].kind, merge::ConflictKind::ModifyVsDelete);
}

#[test]
fn merge_applies_deletion_when_host_untouched() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let host = tmp.path().join("host");
    let work = tmp.path().join("work");
    for d in [&base, &host, &work] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "sub/old.txt", "delete me");
    write(&host, "sub/old.txt", "delete me");
    // worktree: deleted.

    let plan = merge::plan(&scan(&base), &scan(&host), &scan(&work), &work);
    assert!(plan.is_clean(), "{:?}", plan.conflicts);
    let report = merge::execute(&plan, &host).unwrap();
    // The file and its (now empty) parent directory are both removed.
    assert_eq!(report.deleted, 2);
    assert!(!host.join("sub/old.txt").exists());
    assert!(!host.join("sub").exists(), "empty dir should be pruned");
}

#[test]
fn merge_converged_sides_are_noop() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let host = tmp.path().join("host");
    let work = tmp.path().join("work");
    for d in [&base, &host, &work] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "a.txt", "v1");
    write(&host, "a.txt", "same-change");
    write(&work, "a.txt", "same-change");

    let plan = merge::plan(&scan(&base), &scan(&host), &scan(&work), &work);
    assert!(plan.is_clean());
    assert!(plan.operations.is_empty());
    assert_eq!(plan.converged, vec![std::path::PathBuf::from("a.txt")]);
}

#[test]
fn merge_file_to_dir_migration() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let host = tmp.path().join("host");
    let work = tmp.path().join("work");
    for d in [&base, &host, &work] {
        fs::create_dir_all(d).unwrap();
    }
    // base/host: x is a FILE; work: x became a DIR with a child.
    write(&base, "x", "old");
    write(&host, "x", "old");
    fs::create_dir_all(work.join("x")).unwrap();
    write(&work, "x/inner.txt", "new");

    let plan = merge::plan(&scan(&base), &scan(&host), &scan(&work), &work);
    assert!(plan.is_clean(), "file->dir migration must not conflict");
    let out = merge::execute(&plan, &host).unwrap();
    assert_eq!(out.written, 2);
    assert_eq!(fs::read_to_string(host.join("x/inner.txt")).unwrap(), "new");
    assert!(
        !host.join("x").is_file(),
        "old file must be replaced by dir"
    );
    assert!(!fs::symlink_metadata(host.join("x")).unwrap().is_file());
}

#[test]
fn merge_dir_to_file_migration() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let host = tmp.path().join("host");
    let work = tmp.path().join("work");
    for d in [&base, &host, &work] {
        fs::create_dir_all(d).unwrap();
    }
    // base/host: x is a DIR with inner.txt; work: x became a FILE.
    fs::create_dir_all(base.join("x")).unwrap();
    write(&base, "x/inner.txt", "old");
    fs::create_dir_all(host.join("x")).unwrap();
    write(&host, "x/inner.txt", "old");
    write(&work, "x", "now-a-file");

    let plan = merge::plan(&scan(&base), &scan(&host), &scan(&work), &work);
    assert!(plan.is_clean(), "dir->file migration must not conflict");
    let out = merge::execute(&plan, &host).unwrap();
    assert_eq!(out.written, 1);
    assert_eq!(fs::read_to_string(host.join("x")).unwrap(), "now-a-file");
    assert!(!host.join("x").is_dir(), "old dir must be replaced by file");
}
