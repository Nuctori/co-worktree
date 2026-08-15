//! Unit tests for the cowt-core engine: manifest, diff, overlay, merge.

use std::fs;
use std::path::{Path, PathBuf};

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

#[test]
fn key_diff_escapes_dotted_keys() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let work = tmp.path().join("work");
    fs::create_dir_all(&base).unwrap();
    fs::create_dir_all(&work).unwrap();
    // Literal "a.b" key vs nested {"a":{"b":...}} must NOT collide.
    write(&base, "s.json", r#"{"a.b": 1}"#);
    write(&work, "s.json", r#"{"a": {"b": 1}}"#);
    let (_, _, changes) = diff::diff_trees(&base, &work).unwrap();
    match changes[0].detail.as_ref().unwrap() {
        diff::ContentDiff::Keys { changes: keys } => {
            assert!(
                !keys.is_empty(),
                "dotted-key collision must not hide changes"
            );
            let escaped = keys.iter().find(|k| k.key == r"a\.b").is_some();
            let nested = keys.iter().find(|k| k.key == "a.b").is_some();
            assert!(
                escaped || nested,
                "keys: {:?}",
                keys.iter().map(|k| &k.key).collect::<Vec<_>>()
            );
        }
        other => panic!("expected Keys, got {other:?}"),
    }
}

#[test]
fn key_diff_type_change_visible() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let work = tmp.path().join("work");
    fs::create_dir_all(&base).unwrap();
    fs::create_dir_all(&work).unwrap();
    write(&base, "s.json", r#"{"n": 123, "b": true}"#);
    write(&work, "s.json", r#"{"n": "123", "b": "true"}"#);
    let (_, _, changes) = diff::diff_trees(&base, &work).unwrap();
    match changes[0].detail.as_ref().unwrap() {
        diff::ContentDiff::Keys { changes: keys } => {
            assert_eq!(keys.len(), 2, "type changes must be visible: {keys:?}");
            let n = keys.iter().find(|k| k.key == "n").unwrap();
            assert_eq!(n.old.as_deref(), Some("123"));
            assert_eq!(n.new.as_deref(), Some("\"123\""));
        }
        other => panic!("expected Keys, got {other:?}"),
    }
}

#[test]
fn key_diff_root_container_swap_visible() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let work = tmp.path().join("work");
    fs::create_dir_all(&base).unwrap();
    fs::create_dir_all(&work).unwrap();
    write(&base, "s.json", "{}");
    write(&work, "s.json", "[]");
    let (_, _, changes) = diff::diff_trees(&base, &work).unwrap();
    match changes[0].detail.as_ref().unwrap() {
        diff::ContentDiff::Keys { changes: keys } => {
            assert_eq!(keys.len(), 1, "root swap must be reported: {keys:?}");
            assert_eq!(keys[0].key, "root");
            assert_eq!(keys[0].old.as_deref(), Some("{}"));
            assert_eq!(keys[0].new.as_deref(), Some("[]"));
        }
        other => panic!("expected Keys, got {other:?}"),
    }
}

#[test]
fn scan_detects_same_size_same_mtime_rewrite() {
    // Round-11 regression: an external tool rewriting a file while
    // preserving size AND mtime (touch -r / rsync -t) must still be
    // detected by a full scan (hash differs) — the stat_eq fast path
    // must never be used for merge decisions.
    let tmp = TempDir::new().unwrap();
    let d = tmp.path().join("d");
    fs::create_dir_all(&d).unwrap();
    let p = d.join("f.txt");
    fs::write(&p, "aaaa\nbbbb\n").unwrap();
    let before = fs::metadata(&p).unwrap();
    let mtime = before.modified().unwrap();
    let m1 = Manifest::scan(&d).unwrap().manifest;

    // Same size, same mtime, different content.
    fs::write(&p, "aaaa\ncccc\n").unwrap();
    set_mtime(&p, mtime);
    let m2 = Manifest::scan(&d).unwrap().manifest;
    assert_ne!(
        m1.entries[&PathBuf::from("f.txt")].hash,
        m2.entries[&PathBuf::from("f.txt")].hash,
        "full scan must re-hash: same size+mtime rewrite is a real change"
    );
    // And the merge planner must see a conflict (host changed).
    let base = m1;
    let host = m2;
    let work_dir = tmp.path().join("w");
    fs::create_dir_all(&work_dir).unwrap();
    fs::write(work_dir.join("f.txt"), "WWWW\nWWWW\n").unwrap();
    let work = Manifest::scan(&work_dir).unwrap().manifest;
    let plan = merge::plan(&base, &host, &work, &work_dir);
    assert!(
        !plan.is_clean(),
        "same-size+mtime host rewrite must conflict"
    );
}

fn set_mtime(p: &std::path::Path, t: std::time::SystemTime) {
    let f = std::fs::OpenOptions::new().write(true).open(p).unwrap();
    let _ = f.set_times(std::fs::FileTimes::new().set_modified(t));
}

// ---------------------------------------------------------------- R21

/// Round-21: a non-empty `.wh.*`-prefixed file in the upper layer is a user
/// file, NOT a deletion marker — it must stay visible in the effective
/// manifest (a plain name-prefix skip would silently drop real changes).
#[test]
fn overlay_keeps_non_whiteout_wh_prefixed_entries() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    fs::create_dir_all(&base).unwrap();
    fs::create_dir_all(&upper).unwrap();
    write(&base, "notes.txt", "base");
    write(&upper, ".wh.kept", "user content");

    let base_m = Manifest::scan(&base).unwrap().manifest;
    let effective = overlay::effective_manifest(&base_m, &upper).unwrap();
    let kept = effective
        .get(Path::new(".wh.kept"))
        .expect(".wh.kept (non-empty) must be a visible entry, not skipped");
    assert_eq!(kept.kind, EntryKind::File);
    assert!(kept.hash.is_some());

    // The real deletion encoding (0-size .wh.<name>) still folds.
    write(&upper, ".wh.notes.txt", "");
    let effective = overlay::effective_manifest(&base_m, &upper).unwrap();
    assert!(effective.get(Path::new("notes.txt")).is_none());
}

/// Round-21: old-Mac lone-\r line endings must not glue unified diff lines
/// together (a deleted line hidden by a carriage-return overwrite).
#[test]
fn unified_diff_lone_cr_terminates_lines() {
    let u = diff::unified_diff("a\nb\r", "a\nc\r");
    assert!(u.lines().any(|l| l == "-b"), "deleted line lost:\n{u}");
    assert!(u.lines().any(|l| l == "+c"), "added line lost:\n{u}");
    assert!(
        !u.contains('\r'),
        "lone CR leaked into unified output:\n{u}"
    );
}

/// Round-21: corrupted manifests with an empty/garbage hash must fail loudly
/// instead of producing phantom Modified changes.
#[test]
fn manifest_from_json_rejects_invalid_hash() {
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
    // Missing hash (unreadable file) is still accepted.
    let none = r#"{"base":"/x","created_epoch":0,"entries":{"f":{"kind":"file","size":0,"mode":0,"mtime_ns":0}}}"#;
    assert!(Manifest::from_json(none).is_ok());
}

/// Round-21 regression lock: CRLF / LF / trailing-newline / BOM / mixed
/// line-ending handling must stay correct (currently all green).
#[test]
fn diff_line_endings_regressions() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let work = tmp.path().join("work");
    fs::create_dir_all(&base).unwrap();
    fs::create_dir_all(&work).unwrap();

    // (a) CRLF file, one line changed -> minimal hunk, no full-file noise.
    write(&base, "a.txt", "l1\r\nl2\r\nl3\r\n");
    write(&work, "a.txt", "l1\r\nl2x\r\nl3\r\n");
    let (_, _, changes) = diff::diff_trees(&base, &work).unwrap();
    let ch = changes
        .iter()
        .find(|c| c.path == Path::new("a.txt"))
        .unwrap();
    match ch.detail.as_ref().unwrap() {
        diff::ContentDiff::Text { unified } => {
            assert!(unified.contains("+l2x"), "minimal hunk missing:\n{unified}");
            assert!(!unified.contains("-l1") && !unified.contains("-l3"));
        }
        other => panic!("expected text diff, got {other:?}"),
    }

    // (b) same content LF->CRLF -> Modified, every line visible.
    write(&base, "b.txt", "l1\nl2\nl3\n");
    write(&work, "b.txt", "l1\r\nl2\r\nl3\r\n");
    let (_, _, changes) = diff::diff_trees(&base, &work).unwrap();
    let ch = changes
        .iter()
        .find(|c| c.path == Path::new("b.txt"))
        .unwrap();
    assert_eq!(ch.kind, diff::ChangeKind::Modified);
    match ch.detail.as_ref().unwrap() {
        diff::ContentDiff::Text { unified } => {
            assert!(
                unified.contains("-l1") && unified.contains("+l1"),
                "LF->CRLF must show the line change:\n{unified}"
            );
        }
        other => panic!("expected text diff, got {other:?}"),
    }

    // (c) trailing-newline add -> Modified + explicit no-newline marker.
    write(&base, "c.txt", "a\nb");
    write(&work, "c.txt", "a\nb\n");
    let (_, _, changes) = diff::diff_trees(&base, &work).unwrap();
    let ch = changes
        .iter()
        .find(|c| c.path == Path::new("c.txt"))
        .unwrap();
    match ch.detail.as_ref().unwrap() {
        diff::ContentDiff::Text { unified } => {
            assert!(
                unified.contains("No newline at end of file"),
                "missing no-newline marker:\n{unified}"
            );
        }
        other => panic!("expected text diff, got {other:?}"),
    }

    // (d) BOM add/remove -> first line visible.
    write(&base, "d.txt", "x\n");
    write(&work, "d.txt", "\u{feff}x\n");
    let (_, _, changes) = diff::diff_trees(&base, &work).unwrap();
    let ch = changes
        .iter()
        .find(|c| c.path == Path::new("d.txt"))
        .unwrap();
    match ch.detail.as_ref().unwrap() {
        diff::ContentDiff::Text { unified } => {
            assert!(
                unified.contains("-x") || unified.contains("+x"),
                "BOM change must be visible:\n{unified}"
            );
        }
        other => panic!("expected text diff, got {other:?}"),
    }

    // (e) mixed endings: lone \r mid-file, one line changed -> only that line.
    write(&base, "e.txt", "l1\nl2\rl3\n");
    write(&work, "e.txt", "l1\nl2X\rl3\n");
    let (_, _, changes) = diff::diff_trees(&base, &work).unwrap();
    let ch = changes
        .iter()
        .find(|c| c.path == Path::new("e.txt"))
        .unwrap();
    match ch.detail.as_ref().unwrap() {
        diff::ContentDiff::Text { unified } => {
            assert!(unified.contains("l2X"), "changed line missing:\n{unified}");
            assert!(!unified.contains("-l1") && !unified.contains("-l3"));
        }
        other => panic!("expected text diff, got {other:?}"),
    }
}

/// Round-21 regression lock: empty-file (0-byte) boundaries across
/// scan / diff / merge / apply.
#[test]
fn empty_file_boundaries() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let work = tmp.path().join("work");
    fs::create_dir_all(&base).unwrap();
    fs::create_dir_all(&work).unwrap();

    // Scan: a 0-byte file gets a real (non-empty) BLAKE3 hash.
    write(&base, "e.txt", "");
    let bm = Manifest::scan(&base).unwrap().manifest;
    let e = bm.get(Path::new("e.txt")).unwrap();
    assert_eq!(e.size, 0);
    let h = e.hash.as_ref().expect("0-byte file must be hashed");
    assert_eq!(h.len(), 64, "hash must be 64 hex chars, got {h:?}");

    // "" vs "\n" -> Modified.
    write(&work, "e.txt", "\n");
    let wm = Manifest::scan(&work).unwrap().manifest;
    let changes = diff::diff(&bm, &wm);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].kind, diff::ChangeKind::Modified);

    // "" vs missing -> Deleted.
    let edir = tmp.path().join("empty");
    fs::create_dir_all(&edir).unwrap();
    let em = Manifest::scan(&edir).unwrap().manifest;
    let changes = diff::diff(&bm, &em);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].kind, diff::ChangeKind::Deleted);

    // missing vs "" -> Added.
    let changes = diff::diff(&em, &bm);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].kind, diff::ChangeKind::Added);

    // Merge: base empty file + host deleted + work non-empty -> DeleteVsModify.
    let host = tmp.path().join("host");
    let work2 = tmp.path().join("work2");
    fs::create_dir_all(&host).unwrap();
    fs::create_dir_all(&work2).unwrap();
    write(&work2, "e.txt", "content"); // work modified the empty base file
    write(&work2, "f.txt", "content");
    let plan = merge::plan(&bm, &em, &scan(&work2), &work2);
    assert!(
        plan.conflicts
            .iter()
            .any(|c| c.path == Path::new("e.txt") && c.kind == merge::ConflictKind::DeleteVsModify),
        "empty-file delete-vs-modify must conflict: {:?}",
        plan.conflicts
    );

    // WriteFile of a 0-byte source -> target exists with len 0.
    let host2 = tmp.path().join("host2");
    let work3 = tmp.path().join("work3");
    fs::create_dir_all(&host2).unwrap();
    fs::create_dir_all(&work3).unwrap();
    write(&work3, "g.txt", "");
    let plan = merge::plan(&em, &scan(&host2), &scan(&work3), &work3);
    assert!(plan.is_clean());
    merge::execute(&plan, &host2).unwrap();
    let meta = fs::metadata(host2.join("g.txt")).unwrap();
    assert_eq!(meta.len(), 0);
}
