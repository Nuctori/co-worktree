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

// ---------------------------------------------------------------- R23

/// Round-23: duplicate path keys in a manifest must be rejected loudly
/// (serde_json's map is last-wins; a corrupt second entry silently
/// overriding a good one produced misleading "keep host changed" reports).
#[test]
fn manifest_from_json_rejects_duplicate_path_keys() {
    let dup = r#"{"base":"/x","created_epoch":0,"entries":{
        "a.txt":{"kind":"file","size":3,"mode":0,"mtime_ns":0,"hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
        "a.txt":{"kind":"file","size":9,"mode":0,"mtime_ns":0,"hash":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}
    }}"#;
    assert!(
        Manifest::from_json(dup).is_err(),
        "duplicate path key must be rejected"
    );
}

/// Round-23: manifest path keys must respect the same invariants the
/// scanner enforces — relative, no `.`/`..`/empty/absolute keys — so a
/// corrupt manifest cannot turn a real change into a misleading
/// both_added conflict (base='-').
#[test]
fn manifest_from_json_rejects_invalid_path_keys() {
    let _base = r#"{"base":"/x","created_epoch":0,"entries":{}}"#;
    let entry = r#""kind":"file","size":0,"mode":0,"mtime_ns":0"#;
    for bad_key in ["/etc/passwd", "..\\escape.txt", ".", "./a.txt", ""] {
        let json =
            format!(r#"{{"base":"/x","created_epoch":0,"entries":{{"{bad_key}":{{{entry}}}}}}}"#);
        assert!(
            Manifest::from_json(&json).is_err(),
            "path key {bad_key:?} must be rejected"
        );
    }
    // A normal relative key still round-trips.
    let good = format!(r#"{{"base":"/x","created_epoch":0,"entries":{{"a.txt":{{{entry}}}}}}}"#);
    assert!(Manifest::from_json(&good).is_ok());
}

/// Round-23: the manifest corruption boundary matrix (22 variants audited)
/// must stay stable: parse errors map to CorruptManifest, extreme numeric
/// values are accepted but harmless, unknown enums/fields fail cleanly.
#[test]
fn manifest_from_json_boundary_matrix() {
    let base = r#"{"base":"/x","created_epoch":0,"entries":{}}"#;
    // (corrupt variants -> Err)
    let corrupt = [
        r#"{"base":"/x","created_epoch":0"#,               // truncated
        r#"{"base":"/x","created_epoch":0,"entries":[]}"#, // entries is array
        r#"{"base":"/x","created_epoch":0,"entries":{"f":{"kind":"whatever","size":0,"mode":0,"mtime_ns":0}}}"#, // unknown enum
        r#"{"base":"/x","created_epoch":0,"entries":{"f":{"kind":"file","size":0,"mode":0,"mtime_ns":0},"extra":1}}"#, // ok, unknown field tolerated by serde default
    ];
    // "extra" field is tolerated (serde default) — assert that separately.
    for json in corrupt {
        // The last one is *accepted* (unknown fields are ignored) — handle below.
        if !json.contains("extra") {
            assert!(
                Manifest::from_json(json).is_err(),
                "corrupt variant must be rejected: {json}"
            );
        }
    }
    // Unknown top-level fields are ignored (forward compat), not corruption.
    let unknown_field = r#"{"base":"/x","created_epoch":0,"entries":{},"future":"v2"}"#;
    assert!(Manifest::from_json(unknown_field).is_ok());
    // Extreme numeric values are accepted (scan never produces them, but
    // they are harmless: content_eq compares size+hash+mode, and merge
    // re-scans the live host before any write).
    let extremes = r#"{"base":"/x","created_epoch":0,"entries":{
        "big":{"kind":"file","size":18446744073709551615,"mode":4294967295,"mtime_ns":-5,"hash":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"}
    }}"#;
    assert!(Manifest::from_json(extremes).is_ok());
    assert!(Manifest::from_json(base).is_ok());
}

/// Round-23: `drop` must not depend on manifest.json — a missing or corrupt
/// manifest must not block cleanup (recovery path regression lock).
#[test]
fn manifest_missing_or_corrupt_does_not_block_drop_paths() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let host = tmp.path().join("host");
    fs::create_dir_all(&base).unwrap();
    fs::create_dir_all(&host).unwrap();
    write(&base, "a.txt", "base");
    write(&host, "a.txt", "base");
    let bm = Manifest::scan(&base).unwrap().manifest;
    let em = Manifest::scan(&host).unwrap().manifest;

    // A missing entry in base (semantically corrupt) + whiteout in upper:
    // merge::plan must NOT silently produce an empty plan for the delete —
    // this is the R23-01 discriminator (whiteout victim in current but not
    // in base). The apply-level guard lives in the CLI; here we pin the
    // merge-level signature: b=None,c=Some,w=None must surface as a
    // conflict/kept, never as a silent no-op that clears upper.
    let upper = tmp.path().join("upper");
    fs::create_dir_all(&upper).unwrap();
    write(&upper, ".wh.a.txt", ""); // deletion marker, victim NOT in base
    let wm = overlay::effective_manifest(&bm, &upper).unwrap();
    let plan = merge::plan(&bm, &em, &wm, &upper);
    // b=Some (a.txt in base), c=Some, w=None -> Delete. That is the healthy
    // case. The corrupt case (victim missing from base) is guarded in
    // apply.rs; this test locks the healthy Delete generation.
    assert!(
        plan.operations.iter().any(|op| matches!(
            op,
            merge::Operation::Delete { path, .. } if path == &PathBuf::from("a.txt")
        )),
        "whiteout of a base file must generate Delete: {:?}",
        plan.operations
    );
}

// ---------------------------------------------------------------- R24

/// Round-24: a file→empty-dir migration must leave the freshly created
/// directory in place. The migration Delete (order 0) runs in the later
/// Delete phase, after Mkdir created the dir — removing it would lose the
/// "create empty dir" intent while still reporting written=1, deleted=1.
#[test]
fn merge_file_to_empty_dir_migration_keeps_dir() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let host = tmp.path().join("host");
    let work = tmp.path().join("work");
    for d in [&base, &host, &work] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "x", "v1");
    write(&host, "x", "v1");
    fs::create_dir_all(work.join("x")).unwrap(); // empty dir in work

    let plan = merge::plan(&scan(&base), &scan(&host), &scan(&work), &work);
    assert!(plan.is_clean(), "conflicts: {:?}", plan.conflicts);
    let report = merge::execute(&plan, &host).unwrap();
    assert!(
        host.join("x").is_dir(),
        "file->empty-dir migration must leave the dir (was deleted by migration Delete)"
    );
    assert_eq!(report.written, 1);
    assert_eq!(
        report.deleted, 0,
        "migration delete must not remove the new dir"
    );
}

/// Round-24: deleting a directory in the worktree while the host added a
/// file inside it (absent from base) must conflict — not silently skip the
/// delete, advance the baseline and lose the intent.
#[test]
fn dir_delete_with_host_only_file_conflicts() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let host = tmp.path().join("host");
    let work = tmp.path().join("work");
    for d in [&base, &host, &work] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "d/f.txt", "base");
    write(&host, "d/f.txt", "base");
    write(&host, "d/extra.txt", "host-only"); // not in base, not in work
                                              // worktree deleted the whole dir (work is empty).

    let plan = merge::plan(&scan(&base), &scan(&host), &scan(&work), &work);
    assert!(
        !plan.is_clean(),
        "host-only file under a worktree-deleted dir must conflict: {:?}",
        plan.conflicts
    );
    assert!(plan
        .conflicts
        .iter()
        .any(|c| c.path == *"d/extra.txt" && c.kind == merge::ConflictKind::ModifyVsDelete));
}

/// Round-24: a host edit landing between planning and execution must abort
/// the apply instead of being silently overwritten (TOCTOU).
#[test]
fn execute_toctou_plan_divergence_aborts() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let host = tmp.path().join("host");
    let work = tmp.path().join("work");
    for d in [&base, &host, &work] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "x.txt", "v1");
    write(&host, "x.txt", "v1");
    write(&work, "x.txt", "v2");

    let plan = merge::plan(&scan(&base), &scan(&host), &scan(&work), &work);
    assert!(plan.is_clean());
    // Host edits the target AFTER planning (the apply window).
    fs::write(host.join("x.txt"), "HOST-EDIT").unwrap();
    assert!(
        merge::execute(&plan, &host).is_err(),
        "host change after planning must abort execute"
    );
    assert_eq!(
        fs::read_to_string(host.join("x.txt")).unwrap(),
        "HOST-EDIT",
        "host edit must survive the aborted apply"
    );
}

/// Round-24: a read-only source file (worktree side) must not make apply
/// fail — the staged copy inherits the read-only mode and the fsync write
/// open would EACCES/ACCESS_DENIED.
#[test]
fn apply_readonly_source_file_succeeds() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let host = tmp.path().join("host");
    let work = tmp.path().join("work");
    for d in [&base, &host, &work] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "x.txt", "v1");
    write(&host, "x.txt", "v1");
    write(&work, "x.txt", "v2-readonly");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(work.join("x.txt"), fs::Permissions::from_mode(0o444)).unwrap();
    }
    #[cfg(windows)]
    {
        let mut p = fs::metadata(work.join("x.txt")).unwrap().permissions();
        p.set_readonly(true);
        fs::set_permissions(work.join("x.txt"), p).unwrap();
    }

    let plan = merge::plan(&scan(&base), &scan(&host), &scan(&work), &work);
    assert!(plan.is_clean());
    merge::execute(&plan, &host).unwrap();
    assert_eq!(
        fs::read_to_string(host.join("x.txt")).unwrap(),
        "v2-readonly",
        "read-only source must still be applied"
    );
}

/// Round-24 regression: partial failure retry converges and leaves no
/// staging residue; the staging dir is outside the target and never scanned.
#[test]
fn execute_partial_failure_retry_converges() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let host = tmp.path().join("host");
    let work = tmp.path().join("work");
    for d in [&base, &host, &work] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "a.txt", "v1");
    write(&base, "b.txt", "v1");
    write(&host, "a.txt", "v1");
    write(&host, "b.txt", "v1");
    write(&work, "a.txt", "v2");
    write(&work, "b.txt", "v2");

    let plan = merge::plan(&scan(&base), &scan(&host), &scan(&work), &work);
    assert!(plan.is_clean());
    // Make the second commit fail: block b.txt's destination with a
    // non-empty dir (commit_rename removes only empty dirs).
    fs::remove_file(host.join("b.txt")).unwrap();
    fs::create_dir(host.join("b.txt")).unwrap();
    fs::write(host.join("b.txt/blocker"), "x").unwrap();
    let err = merge::execute(&plan, &host);
    assert!(err.is_err(), "blocked destination must fail the commit");
    // a.txt was already committed before the failure.
    assert_eq!(
        fs::read_to_string(host.join("a.txt")).unwrap(),
        "v2",
        "already-committed file survives the failed apply"
    );
    // Retry after restoring the host file (the failed path stays untouched)
    // converges: the committed file converges, the rest applies.
    fs::remove_dir_all(host.join("b.txt")).unwrap();
    fs::write(host.join("b.txt"), "v1").unwrap();
    let plan2 = merge::plan(&scan(&base), &scan(&host), &scan(&work), &work);
    assert!(
        plan2.is_clean(),
        "retry must plan clean: {:?}",
        plan2.conflicts
    );
    let report = merge::execute(&plan2, &host).unwrap();
    assert_eq!(fs::read_to_string(host.join("b.txt")).unwrap(), "v2");
    assert_eq!(
        report.written, 1,
        "only b.txt needs writing; a.txt converged"
    );
    assert_eq!(report.converged, 1);
    // No staging residue anywhere.
    let leftovers: Vec<_> = fs::read_dir(tmp.path())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".cowt-apply-"))
        .collect();
    assert!(leftovers.is_empty(), "staging residue: {leftovers:?}");
}

/// Round-24: a stray staging dir (crash residue) must never appear in a
/// manifest scan of the target.
#[test]
fn staging_dir_not_scanned_by_manifest_scan() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.path().join("target");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("real.txt"), "x").unwrap();
    // Simulate a crash-left staging sibling.
    let parent = tmp.path();
    fs::create_dir_all(parent.join(".cowt-apply-999-1")).unwrap();
    fs::write(parent.join(".cowt-apply-999-1/leak.txt"), "leak").unwrap();

    let m = Manifest::scan(&target).unwrap().manifest;
    assert!(m.get(Path::new("leak.txt")).is_none());
    assert!(m.get(Path::new("real.txt")).is_some());
}

// ---------------------------------------------------------------- R25

/// Round-25: dir→file migration with a host-only child directly under the
/// migrated directory must conflict at plan time — not plan clean, delete
/// the base child, then fail forever on the non-empty dir (the R24-02
/// host_only check only covered the w=None pure-delete branch).
#[test]
fn dir_to_file_migration_with_host_only_child_conflicts() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let host = tmp.path().join("host");
    let work = tmp.path().join("work");
    for d in [&base, &host, &work] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "d/f.txt", "base");
    write(&host, "d/f.txt", "base");
    write(&host, "d/extra.txt", "host-only"); // not in base, not in work
    write(&work, "d", "now-a-file"); // dir->file migration

    let plan = merge::plan(&scan(&base), &scan(&host), &scan(&work), &work);
    assert!(
        !plan.is_clean(),
        "dir->file migration with host-only child must conflict: {:?}",
        plan.conflicts
    );
    assert!(plan
        .conflicts
        .iter()
        .any(|c| { c.path == *"d/extra.txt" && c.kind == merge::ConflictKind::ModifyVsDelete }));
    // execute must refuse (conflict) and leave the base child intact.
    assert!(merge::execute(&plan, &host).is_err());
    assert!(
        host.join("d/f.txt").exists(),
        "failed apply must not destroy the base child"
    );
    assert_eq!(fs::read_to_string(host.join("d/f.txt")).unwrap(), "base");
}

/// Round-25: dir→symlink migration must apply on unix — write_symlink must
/// remove an empty directory left at the destination (it only removed
/// files, so symlink() hit EEXIST forever).
#[cfg(unix)]
#[test]
fn merge_dir_to_symlink_migration() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let host = tmp.path().join("host");
    let work = tmp.path().join("work");
    for d in [&base, &host, &work] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "x/f.txt", "base");
    write(&host, "x/f.txt", "base");
    // work: x is now a symlink
    std::os::unix::fs::symlink("target-dir", work.join("x")).unwrap();

    let plan = merge::plan(&scan(&base), &scan(&host), &scan(&work), &work);
    assert!(plan.is_clean(), "conflicts: {:?}", plan.conflicts);
    merge::execute(&plan, &host).unwrap();
    let meta = fs::symlink_metadata(host.join("x")).unwrap();
    assert!(meta.file_type().is_symlink(), "x must become a symlink");
    assert_eq!(
        fs::read_link(host.join("x")).unwrap(),
        std::path::PathBuf::from("target-dir")
    );
    // The old child is gone (migration deletes the dir subtree).
    assert!(!host.join("x/f.txt").exists());
}

/// Round-25 regression lock: rename collision matrix (focus 1).
#[test]
fn rename_collision_matrix() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let host = tmp.path().join("host");
    let work = tmp.path().join("work");

    // (1) work a->b + host modified a: a conflicts ModifyVsDelete.
    for d in [&base, &host, &work] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "a.txt", "v1");
    write(&host, "a.txt", "host-edited");
    write(&work, "b.txt", "v1"); // a deleted, b added
    let plan = merge::plan(&scan(&base), &scan(&host), &scan(&work), &work);
    assert!(plan
        .conflicts
        .iter()
        .any(|c| c.path == *"a.txt" && c.kind == merge::ConflictKind::ModifyVsDelete));
    fs::remove_dir_all(&host).unwrap();
    fs::create_dir_all(&host).unwrap();

    // (2) work a->b + host created b with different content: BothAdded.
    fs::remove_dir_all(&work).unwrap();
    fs::create_dir_all(&work).unwrap();
    write(&base, "a.txt", "v1");
    write(&host, "a.txt", "v1");
    write(&host, "b.txt", "host-b");
    write(&work, "b.txt", "work-b");
    let plan = merge::plan(&scan(&base), &scan(&host), &scan(&work), &work);
    assert!(plan
        .conflicts
        .iter()
        .any(|c| c.path == *"b.txt" && c.kind == merge::ConflictKind::BothAdded));
    fs::remove_dir_all(&host).unwrap();
    fs::create_dir_all(&host).unwrap();

    // (3) work a->b + host renamed a->b with SAME content: converged, no ops.
    write(&host, "b.txt", "work-b"); // host's rename matches work's exactly
    let plan = merge::plan(&scan(&base), &scan(&host), &scan(&work), &work);
    assert!(plan.is_clean());
    assert!(
        plan.operations.is_empty(),
        "no ops when converged: {:?}",
        plan.operations
    );
    fs::remove_dir_all(&host).unwrap();
    fs::create_dir_all(&host).unwrap();

    // (4) host renamed a->c (different target), work renamed a->b:
    // independent paths, clean plan (a deleted on both sides converges).
    write(&host, "c.txt", "v1");

    // (4) host renamed a->c, work renamed a->b: independent paths.
    write(&host, "c.txt", "v1");
    let plan = merge::plan(&scan(&base), &scan(&host), &scan(&work), &work);
    assert!(plan.is_clean());
    fs::remove_dir_all(&host).unwrap();
    fs::create_dir_all(&host).unwrap();

    // (5) host built a DIRECTORY at b while work builds a file: BothAdded.
    write(&host, "a.txt", "v1");
    fs::create_dir_all(host.join("b.txt")).unwrap();
    write(&host, "b.txt/inner", "x");
    let plan = merge::plan(&scan(&base), &scan(&host), &scan(&work), &work);
    assert!(plan
        .conflicts
        .iter()
        .any(|c| c.path == *"b.txt" && c.kind == merge::ConflictKind::BothAdded));
}

/// Round-25 regression lock: work a->b + host modified a AND b matches work:
/// a conflicts, b converges, plan refuses (no partial apply).
#[test]
fn rename_collision_matrix_2() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let host = tmp.path().join("host");
    let work = tmp.path().join("work");
    for d in [&base, &host, &work] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "a.txt", "v1");
    write(&host, "a.txt", "host-edited");
    write(&host, "b.txt", "v2");
    write(&work, "b.txt", "v2");
    let plan = merge::plan(&scan(&base), &scan(&host), &scan(&work), &work);
    assert!(plan
        .conflicts
        .iter()
        .any(|c| { c.path == *"a.txt" && c.kind == merge::ConflictKind::ModifyVsDelete }));
    assert!(
        plan.converged.contains(&PathBuf::from("b.txt")),
        "b must converge"
    );
    assert!(plan.operations.is_empty(), "conflict => no ops");
    assert!(
        !plan.is_clean(),
        "a conflict must refuse the plan despite b converging"
    );
}

/// Round-25 regression lock: conflict classification boundaries (focus 3).
#[test]
fn both_added_kind_mismatch_and_converged_dir_children() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let host = tmp.path().join("host");
    let work = tmp.path().join("work");
    for d in [&base, &host, &work] {
        fs::create_dir_all(d).unwrap();
    }
    // (a) both sides create dir b/ with a shared child of different content:
    // dir converges, child conflicts.
    write(&host, "b/shared.txt", "host-version");
    write(&work, "b/shared.txt", "work-version");
    let plan = merge::plan(&scan(&base), &scan(&host), &scan(&work), &work);
    assert!(
        plan.conflicts
            .iter()
            .any(|c| c.path == *"b/shared.txt" && c.kind == merge::ConflictKind::BothAdded),
        "shared child under both-created dir must conflict: {:?}",
        plan.conflicts
    );
    assert!(
        plan.converged.contains(&PathBuf::from("b")),
        "the dir itself converges"
    );
    fs::remove_dir_all(&host).unwrap();
    fs::create_dir_all(&host).unwrap();
    fs::remove_dir_all(&work).unwrap();
    fs::create_dir_all(&work).unwrap();

    // (b) host creates a dir at p, work creates a file at p: BothAdded
    // (kind mismatch is never content-equal).
    fs::create_dir_all(host.join("p")).unwrap();
    write(&work, "p", "file");
    let plan = merge::plan(&scan(&base), &scan(&host), &scan(&work), &work);
    assert!(plan
        .conflicts
        .iter()
        .any(|c| c.path == *"p" && c.kind == merge::ConflictKind::BothAdded));
}

/// Round-25 regression lock: re-planning after an apply converges (the
/// normal retry path) and a work source vanishing after planning fails
/// loudly with the work path, leaving the host untouched (focus 4/5).
#[test]
fn plan_repeat_idempotent_and_work_source_missing() {
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
    write(&work, "b.txt", "v2");

    let plan = merge::plan(&scan(&base), &scan(&host), &scan(&work), &work);
    assert!(plan.is_clean());
    let r1 = merge::execute(&plan, &host).unwrap();
    assert_eq!(r1.written, 2);
    // Re-planning against the applied host converges: no ops, clean.
    let plan2 = merge::plan(&scan(&base), &scan(&host), &scan(&work), &work);
    assert!(plan2.is_clean(), "re-plan after apply must be clean");
    assert!(
        plan2.operations.is_empty(),
        "re-plan after apply must be a no-op: {:?}",
        plan2.operations
    );
    assert_eq!(
        fs::read_to_string(host.join("a.txt")).unwrap(),
        "v2",
        "repeat must not corrupt content"
    );
    assert_eq!(fs::read_to_string(host.join("b.txt")).unwrap(), "v2");
    fs::remove_dir_all(&host).unwrap();
    fs::create_dir_all(&host).unwrap();

    // Work source vanishes after planning: Phase-1 error names the work
    // path and the host stays untouched.
    write(&host, "a.txt", "v1");
    let plan = merge::plan(&scan(&base), &scan(&host), &scan(&work), &work);
    assert!(plan.is_clean());
    fs::remove_file(work.join("a.txt")).unwrap();
    let err = merge::execute(&plan, &host).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("a.txt"),
        "error must name the work source path: {msg}"
    );
    assert_eq!(
        fs::read_to_string(host.join("a.txt")).unwrap(),
        "v1",
        "host must be untouched by a failed phase-1"
    );
}

// ---------------------------------------------------------------- R27

/// Round-27: a non-directory entry (symlink/file) replacing a base
/// directory in upper must shadow the whole subtree — `rm -rf x && ln -s
/// t x` leaves only the symlink, and x/f.txt is unreachable in the merged
/// view. Without this, diff misses the deletion and apply deadlocks on the
/// non-empty dir.
#[cfg(unix)]
#[test]
fn overlay_symlink_replacing_dir_shadows_subtree() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    fs::create_dir_all(&base).unwrap();
    fs::create_dir_all(&upper).unwrap();
    write(&base, "x/f.txt", "base");
    write(&base, "other.txt", "keep");
    std::os::unix::fs::symlink("target-dir", upper.join("x")).unwrap();

    let base_m = Manifest::scan(&base).unwrap().manifest;
    let effective = overlay::effective_manifest(&base_m, &upper).unwrap();
    assert!(
        effective.get(Path::new("x/f.txt")).is_none(),
        "base descendants under a replaced dir must be shadowed"
    );
    assert_eq!(
        effective.get(Path::new("x")).unwrap().kind,
        EntryKind::Symlink
    );
    // Siblings survive.
    assert!(effective.get(Path::new("other.txt")).is_some());
}

/// Round-27: full chain — dir→symlink replacement in upper plans clean
/// (subtree Delete + migration) and applies.
#[cfg(unix)]
#[test]
fn apply_dir_replaced_by_symlink_chain() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let host = tmp.path().join("host");
    let upper = tmp.path().join("upper");
    for d in [&base, &host, &upper] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "x/f.txt", "base");
    write(&host, "x/f.txt", "base");
    std::os::unix::fs::symlink("target-dir", upper.join("x")).unwrap();

    let base_m = Manifest::scan(&base).unwrap().manifest;
    let host_m = Manifest::scan(&host).unwrap().manifest;
    let work = overlay::effective_manifest(&base_m, &upper).unwrap();
    let plan = merge::plan(&base_m, &host_m, &work, &upper);
    assert!(
        plan.is_clean(),
        "dir->symlink replacement must plan clean: {:?}",
        plan.conflicts
    );
    assert!(
        plan.operations.iter().any(|op| matches!(
            op,
            merge::Operation::Delete { path, .. } if path == &PathBuf::from("x/f.txt")
        )),
        "the shadowed subtree must be deleted: {:?}",
        plan.operations
    );
    merge::execute(&plan, &host).unwrap();
    let meta = fs::symlink_metadata(host.join("x")).unwrap();
    assert!(meta.file_type().is_symlink(), "x must become a symlink");
    assert_eq!(
        fs::read_link(host.join("x")).unwrap(),
        std::path::PathBuf::from("target-dir")
    );
    assert!(!host.join("x/f.txt").exists(), "old subtree must be gone");
}

/// Round-27: diff --content on a retargeted symlink must not read through
/// to the target files' contents (fs::read follows links) — it must report
/// Binary or a link-semantic detail without touching targets.
#[cfg(unix)]
#[test]
fn diff_content_does_not_read_through_symlinks() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let work = tmp.path().join("work");
    fs::create_dir_all(&base).unwrap();
    fs::create_dir_all(&work).unwrap();
    write(&base, "a.txt", "AAA-content");
    write(&base, "b.txt", "BBB-content");
    write(&work, "a.txt", "AAA-content");
    write(&work, "b.txt", "BBB-content");
    std::os::unix::fs::symlink("a.txt", base.join("l")).unwrap();
    std::os::unix::fs::symlink("b.txt", work.join("l")).unwrap();

    let (_, _, mut changes) = diff::diff_trees(&base, &work).unwrap();
    let ch = changes
        .iter_mut()
        .find(|c| c.path == Path::new("l"))
        .unwrap();
    assert_eq!(ch.kind, diff::ChangeKind::Modified);
    // enrich must not read through to a.txt/b.txt contents; a link-semantic
    // detail (or Binary) is acceptable.
    match ch.detail.as_ref().unwrap() {
        diff::ContentDiff::Text { unified } => {
            assert!(
                !unified.contains("AAA-content") && !unified.contains("BBB-content"),
                "symlink diff must not show target file contents:\n{unified}"
            );
        }
        diff::ContentDiff::Binary => {}
        other => panic!("unexpected detail for symlink: {other:?}"),
    }
}

/// Round-27 regression lock: symlink manifest round-trip (dangling, absolute,
/// relative-with-.. targets) and whiteout-vs-symlink fold.
#[cfg(unix)]
#[test]
fn symlink_manifest_round_trip() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    fs::create_dir_all(&base).unwrap();
    fs::create_dir_all(&upper).unwrap();
    // Dangling, absolute and relative-with-.. targets all round-trip.
    std::os::unix::fs::symlink("does-not-exist", base.join("dangling")).unwrap();
    std::os::unix::fs::symlink("/etc/hosts", base.join("absolute")).unwrap();
    std::os::unix::fs::symlink("../up", base.join("relup")).unwrap();
    let bm = Manifest::scan(&base).unwrap().manifest;
    for name in ["dangling", "absolute", "relup"] {
        let e = bm.get(Path::new(name)).unwrap();
        assert_eq!(e.kind, EntryKind::Symlink);
        assert!(e.link_target.is_some());
    }
    let json = bm.to_json().unwrap();
    let rt = Manifest::from_json(&json).unwrap();
    assert_eq!(
        rt.get(Path::new("dangling")).unwrap().link_target,
        bm.get(Path::new("dangling")).unwrap().link_target
    );

    // A whiteout deleting a symlink folds the symlink out.
    std::os::unix::fs::symlink("t", base.join("s")).unwrap();
    let bm = Manifest::scan(&base).unwrap().manifest;
    assert_eq!(bm.get(Path::new("s")).unwrap().kind, EntryKind::Symlink);
    write(&upper, ".wh.s", "");
    let effective = overlay::effective_manifest(&bm, &upper).unwrap();
    assert!(
        effective.get(Path::new("s")).is_none(),
        "symlink whiteout must fold"
    );
}

// ---------------------------------------------------------------- R29

/// Round-29: a non-UTF-8 filename must be SKIPPED with a warning (not
/// hard-fail the whole scan/serialization) — fork/apply keep working.
/// Linux-only: macOS APFS refuses to even create such a name (EILSEQ), and
/// ext4 is the filesystem where the bytes are representable.
#[cfg(target_os = "linux")]
#[test]
fn scan_skips_non_utf8_filenames_with_warning() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let tmp = TempDir::new().unwrap();
    let d = tmp.path().join("d");
    fs::create_dir_all(&d).unwrap();
    write(&d, "ok.txt", "fine");
    // bad\xff.txt — invalid UTF-8 filename.
    let bad = OsStr::from_bytes(b"bad\xff.txt");
    fs::write(d.join(bad), "x").unwrap();

    let out = Manifest::scan(&d).unwrap();
    assert!(
        out.manifest.get(Path::new("ok.txt")).is_some(),
        "good file must be scanned"
    );
    assert!(
        out.manifest.entries.keys().all(|k| k.to_str().is_some()),
        "no non-UTF-8 keys may reach the manifest"
    );
    assert!(
        out.warnings.iter().any(|(_, w)| w.contains("non-UTF-8")),
        "the skip must be warned: {:?}",
        out.warnings
    );
    // And serialization must now succeed (it would have hard-failed
    // before the skip).
    let json = out.manifest.to_json().unwrap();
    assert!(json.contains("ok.txt"));
}

/// Round-29: on macOS, the manifest key is NFC-canonicalized even when the
/// file was created with an NFD spelling — so a later NFC-spelled deletion
/// matches the whiteout (APFS itself stores one file regardless).
#[cfg(target_os = "macos")]
#[test]
fn macos_scan_canonicalizes_keys_to_nfc() {
    let tmp = TempDir::new().unwrap();
    let d = tmp.path().join("d");
    fs::create_dir_all(&d).unwrap();
    write(&d, "cafe\u{301}.txt", "nfd"); // NFD spelling
    let m = Manifest::scan(&d).unwrap().manifest;
    assert_eq!(m.entries.len(), 1);
    // The key must be the NFC form ("café" = U+00E9), not NFD.
    assert!(m.get(Path::new("caf\u{e9}.txt")).is_some());
    assert!(m.get(Path::new("cafe\u{301}.txt")).is_none());
}

// ---------------------------------------------------------------- R30

/// Round-30: chmod-only on a FILE reports Modified and apply restores the
/// mode; round-trip is symmetric.
#[cfg(unix)]
#[test]
fn chmod_only_file_reports_and_restores() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let host = tmp.path().join("host");
    let work = tmp.path().join("work");
    for d in [&base, &host, &work] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "f.txt", "content");
    write(&host, "f.txt", "content");
    write(&work, "f.txt", "content");
    fs::set_permissions(work.join("f.txt"), fs::Permissions::from_mode(0o600)).unwrap();

    let (bm, cm, _) = diff::diff_trees(&base, &host).unwrap();
    let wm = Manifest::scan(&work).unwrap().manifest;
    let changes = diff::diff(&bm, &wm);
    assert!(
        changes.iter().any(|c| c.path == Path::new("f.txt")
            && c.kind == diff::ChangeKind::Modified),
        "chmod-only must be Modified: {:?}",
        changes
    );
    let plan = merge::plan(&bm, &cm, &wm, &work);
    assert!(plan.is_clean());
    merge::execute(&plan, &host).unwrap();
    assert_eq!(
        fs::metadata(host.join("f.txt")).unwrap().permissions().mode() & 0o7777,
        0o600,
        "apply must restore the worktree mode"
    );
}

/// Round-30: chmod-only on a DIRECTORY reports Modified and apply restores
/// the mode (was: zero changes, mode never applied — round-30 fix).
#[cfg(unix)]
#[test]
fn chmod_only_dir_reports_and_restores() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let host = tmp.path().join("host");
    let work = tmp.path().join("work");
    for d in [&base, &host, &work] {
        fs::create_dir_all(d).unwrap();
        fs::create_dir_all(d.join("d")).unwrap();
    }
    fs::set_permissions(work.join("d"), fs::Permissions::from_mode(0o700)).unwrap();

    let bm = Manifest::scan(&base).unwrap().manifest;
    let cm = Manifest::scan(&host).unwrap().manifest;
    let wm = Manifest::scan(&work).unwrap().manifest;
    let changes = diff::diff(&bm, &wm);
    assert!(
        changes.iter().any(|c| c.path == Path::new("d")
            && c.kind == diff::ChangeKind::Modified),
        "dir chmod-only must be Modified: {:?}",
        changes
    );
    let plan = merge::plan(&bm, &cm, &wm, &work);
    assert!(plan.is_clean());
    merge::execute(&plan, &host).unwrap();
    assert_eq!(
        fs::metadata(host.join("d")).unwrap().permissions().mode() & 0o7777,
        0o700,
        "apply must restore the worktree dir mode"
    );
}

/// Round-30: touch-only (mtime change, same content) is NOT a change —
/// content equality deliberately ignores mtime, otherwise apply would
/// never converge.
#[cfg(unix)]
#[test]
fn touch_only_is_not_a_change() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let work = tmp.path().join("work");
    fs::create_dir_all(&base).unwrap();
    fs::create_dir_all(&work).unwrap();
    write(&base, "f.txt", "content");
    write(&work, "f.txt", "content");
    // Different mtime, same content+mode.
    fs::set_permissions(work.join("f.txt"), fs::Permissions::from_mode(0o644)).unwrap();
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(10);
    let f = fs::OpenOptions::new()
        .write(true)
        .open(work.join("f.txt"))
        .unwrap();
    let _ = f.set_times(fs::FileTimes::new().set_modified(later));
    drop(f);

    let bm = Manifest::scan(&base).unwrap().manifest;
    let wm = Manifest::scan(&work).unwrap().manifest;
    let changes = diff::diff(&bm, &wm);
    assert!(
        changes.iter().all(|c| c.path != Path::new("f.txt")),
        "touch-only must not report a change: {:?}",
        changes
    );
}

/// Round-30: TOCTOU guard detects a host chmod in the plan->execute window
/// (mode counts as content).
#[cfg(unix)]
#[test]
fn toctou_guard_detects_host_chmod() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let host = tmp.path().join("host");
    let work = tmp.path().join("work");
    for d in [&base, &host, &work] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "f.txt", "v1");
    write(&host, "f.txt", "v1");
    write(&work, "f.txt", "v2");
    fs::set_permissions(host.join("f.txt"), fs::Permissions::from_mode(0o644)).unwrap();

    let plan = merge::plan(&scan(&base), &scan(&host), &scan(&work), &work);
    assert!(plan.is_clean());
    // Host chmod after planning (content unchanged, only mode differs).
    fs::set_permissions(host.join("f.txt"), fs::Permissions::from_mode(0o600)).unwrap();
    assert!(
        merge::execute(&plan, &host).is_err(),
        "host chmod in the window must abort execute"
    );
    assert_eq!(
        fs::metadata(host.join("f.txt")).unwrap().permissions().mode() & 0o7777,
        0o600,
        "host mode must survive the aborted apply"
    );
}

/// Round-29: NFC and NFD spellings of the same name are distinct byte keys
/// on Linux (ext4 normalization-sensitive) — locking that semantics. On
/// macOS, APFS stores them as one file and cowt NFC-canonicalizes keys, so
/// the "two keys" behavior is Linux-specific.
#[cfg(target_os = "linux")]
#[test]
fn nfc_nfd_are_distinct_keys_on_unix() {
    let tmp = TempDir::new().unwrap();
    let d = tmp.path().join("d");
    fs::create_dir_all(&d).unwrap();
    write(&d, "caf\u{e9}.txt", "nfc"); // U+00E9 precomposed
    write(&d, "cafe\u{301}.txt", "nfd"); // e + combining acute
    let m = Manifest::scan(&d).unwrap().manifest;
    assert_eq!(m.entries.len(), 2, "both spellings are separate files");
    assert!(m.get(Path::new("caf\u{e9}.txt")).is_some());
    assert!(m.get(Path::new("cafe\u{301}.txt")).is_some());
}
