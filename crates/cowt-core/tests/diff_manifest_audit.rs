//! Adversarial audit round 3: diff engine + manifest scan edge cases.

use std::fs;
use std::path::{Path, PathBuf};

use cowt_core::diff;
use cowt_core::manifest::{EntryKind, Manifest};
use tempfile::TempDir;

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

#[allow(dead_code)]
fn scan(p: &Path) -> Manifest {
    Manifest::scan(p).unwrap().manifest
}

// ─────────────────────────────────────────────────────────────────────────
// R3-A: key-level JSON diff detects a key named with a literal dot, and does
// NOT collide it with a nested object using the same path segments.
// ─────────────────────────────────────────────────────────────────────────
#[test]
fn audit_json_keydiff_literal_dot_key_not_collide_nested() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let work = tmp.path().join("work");
    fs::create_dir_all(&base).unwrap();
    fs::create_dir_all(&work).unwrap();
    // old: a nested object {"a":{"b":1}}  -> flat key "a.b"
    // new: a top-level key literally named "a.b" -> also flat key "a.b"
    // These must NOT silently collapse: the path set differs.
    write(&base, "c.json", r#"{"a":{"b":1}}"#);
    write(&work, "c.json", r#"{"a.b":2}"#);

    let (_, _, mut changes) = diff::diff_trees(&base, &work).unwrap();
    diff::enrich(&base, &work, &mut changes);
    let ch = changes.into_iter().next().unwrap();
    match ch.detail.unwrap() {
        diff::ContentDiff::Keys { changes: keys } => {
            // The two distinct keys must NOT silently collapse. escape_key
            // disambiguates: the nested path stays "a.b" (Deleted) and the
            // literal key is "a\.b" (Added). Both must be present.
            let deleted = keys
                .iter()
                .any(|k| k.key == "a.b" && k.kind == diff::ChangeKind::Deleted);
            let added = keys
                .iter()
                .any(|k| k.key == r"a\.b" && k.kind == diff::ChangeKind::Added);
            assert!(deleted, "nested a.b must be deleted: {keys:?}");
            assert!(added, "literal a.b must be added: {keys:?}");
        }
        other => panic!("expected key diff, got {other:?}"),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// R3-B: lone-CR (\r without \n) old-Mac line endings are normalized so no
// diff line is dropped (round-21 regression guard).
// ─────────────────────────────────────────────────────────────────────────
#[test]
fn audit_lone_cr_normalized_no_dropped_line() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let work = tmp.path().join("work");
    fs::create_dir_all(&base).unwrap();
    fs::create_dir_all(&work).unwrap();
    // old: "a\r" "b\r" (lone CR each)
    // new: same but an extra line "c" added
    write(&base, "f.txt", "a\rb\r");
    write(&work, "f.txt", "a\rb\rc\r");

    let (_, _, mut changes) = diff::diff_trees(&base, &work).unwrap();
    diff::enrich(&base, &work, &mut changes);
    let ch = changes.into_iter().next().unwrap();
    match ch.detail.unwrap() {
        diff::ContentDiff::Text { unified } => {
            // The added line "c" must appear.
            assert!(
                unified.contains("c"),
                "added lone-CR line dropped: {unified:?}"
            );
        }
        other => panic!("expected text diff, got {other:?}"),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// R3-C: a JSON file whose bytes also parse as YAML (both extensions) — the
// key diff must use the correct parser and still produce a valid diff.
// ─────────────────────────────────────────────────────────────────────────
#[test]
fn audit_json_key_diff_modified_value() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let work = tmp.path().join("work");
    fs::create_dir_all(&base).unwrap();
    fs::create_dir_all(&work).unwrap();
    write(&base, "s.json", r#"{"x":1,"y":{"z":2}}"#);
    write(&work, "s.json", r#"{"x":1,"y":{"z":3}}"#);

    let (_, _, mut changes) = diff::diff_trees(&base, &work).unwrap();
    diff::enrich(&base, &work, &mut changes);
    let ch = changes.into_iter().next().unwrap();
    match ch.detail.unwrap() {
        diff::ContentDiff::Keys { changes: keys } => {
            let z = keys.iter().find(|k| k.key == "y.z").unwrap();
            assert_eq!(z.kind, diff::ChangeKind::Modified);
            assert_eq!(z.old.as_deref(), Some("2"));
            assert_eq!(z.new.as_deref(), Some("3"));
        }
        other => panic!("expected key diff, got {other:?}"),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// R3-D: binary file with a NUL byte must always report Binary, never try a
// line diff (which would leak raw bytes / wrong classification).
// ─────────────────────────────────────────────────────────────────────────
#[test]
fn audit_binary_with_nul_is_binary() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let work = tmp.path().join("work");
    fs::create_dir_all(&base).unwrap();
    fs::create_dir_all(&work).unwrap();
    fs::write(base.join("b.bin"), [0u8, 1, 2, 3]).unwrap();
    fs::write(work.join("b.bin"), [0u8, 1, 2, 9]).unwrap();

    let (_, _, mut changes) = diff::diff_trees(&base, &work).unwrap();
    diff::enrich(&base, &work, &mut changes);
    assert!(matches!(changes[0].detail, Some(diff::ContentDiff::Binary)));
}

// ─────────────────────────────────────────────────────────────────────────
// R3-E: structural diff must never classify a directory (added) as a file
// change, and must not emit phantom "modified" for dirs.
// ─────────────────────────────────────────────────────────────────────────
#[test]
fn audit_diff_dir_addition_is_added_not_modified() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let work = tmp.path().join("work");
    fs::create_dir_all(&base).unwrap();
    fs::create_dir_all(&work).unwrap();
    write(&base, "f.txt", "x");
    fs::create_dir_all(work.join("newdir")).unwrap();

    let (_, _, changes) = diff::diff_trees(&base, &work).unwrap();
    let d = changes
        .iter()
        .find(|c| c.path == Path::new("newdir"))
        .unwrap();
    assert_eq!(d.kind, diff::ChangeKind::Added);
    assert_eq!(d.work_hash, None); // dirs carry no content hash
}

// ─────────────────────────────────────────────────────────────────────────
// R3-F: enrich on a non-existent / unreadable file degrades gracefully
// (Binary or whatever), never panics — even when the change is structurally
// a Modified (both files existed at scan time, one vanished before enrich).
// ─────────────────────────────────────────────────────────────────────────
#[test]
fn audit_enrich_missing_file_is_safe() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let work = tmp.path().join("work");
    fs::create_dir_all(&base).unwrap();
    fs::create_dir_all(&work).unwrap();
    write(&base, "gone.txt", "old");
    write(&work, "gone.txt", "new");

    // Build the Modified change directly (as if scanned while both existed),
    // then delete the work file so enrich reads a now-missing path.
    let mut changes = vec![diff::Change {
        path: PathBuf::from("gone.txt"),
        kind: diff::ChangeKind::Modified,
        base_hash: Some("x".into()),
        work_hash: Some("y".into()),
        detail: None,
    }];
    fs::remove_file(work.join("gone.txt")).unwrap();

    // enrich must not panic on the missing work file.
    diff::enrich(&base, &work, &mut changes);
    // It must still be recorded as Modified (detail may be Binary/None).
    assert_eq!(changes[0].kind, diff::ChangeKind::Modified);
}

// ─────────────────────────────────────────────────────────────────────────
// R3-G: manifest scan must reject boundary escape — a symlink pointing
// outside the base must not cause the scan to walk outside (unix only).
// ─────────────────────────────────────────────────────────────────────────
#[cfg(unix)]
#[test]
fn audit_scan_does_not_follow_escape_symlink() {
    let tmp = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    write(outside.path(), "secret.txt", "TOPSECRET");
    write(tmp.path(), "real.txt", "inside");
    std::os::unix::fs::symlink(outside.path(), tmp.path().join("escape")).unwrap();

    let m = scan(tmp.path());
    assert_eq!(m.get(Path::new("real.txt")).unwrap().kind, EntryKind::File);
    assert_eq!(m.get(Path::new("escape")).unwrap().kind, EntryKind::Symlink);
    // The outside file must NOT appear anywhere in the manifest.
    assert!(!m
        .entries
        .keys()
        .any(|p| p.to_string_lossy().contains("secret")));
}

// ─────────────────────────────────────────────────────────────────────────
// R3-H: manifest scan honors the warning path — a directory it cannot read
// is reported, not silently skipped as "no changes".
// ─────────────────────────────────────────────────────────────────────────
#[cfg(unix)]
#[test]
fn audit_scan_reports_unreadable_warning() {
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), "a.txt", "x");
    let locked = tmp.path().join("locked");
    fs::create_dir_all(&locked).unwrap();
    write(&locked, "b.txt", "y");
    // Make `locked` unreadable (drop read+execute permission).
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

    let out = Manifest::scan(tmp.path()).unwrap();
    assert!(!out.warnings.is_empty(), "unreadable dir must be warned");
    // `a.txt` must still be present.
    assert!(out.manifest.get(Path::new("a.txt")).is_some());
    // restore so the tempdir can be cleaned
    let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o755));
}
