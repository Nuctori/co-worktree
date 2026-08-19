//! CYCLE 10 of 10 (serial) — apply execute phase-ordering law.
//!
//! Execute-ordering law: merge::execute performs its phases in a deterministic,
//! conflict-safe sequence — (1) stage WriteFile bodies → (2) Mkdir → (3) Delete
//! (migration-order first, then non-migration, deepest-first) → (4) WriteFile
//! commit → (5) WriteSymlink → (6) prune empty parent dirs. Invariants audited:
//!   - a file recreated under its own whiteout survives (delete-then-create);
//!   - a dir→file migration deletes the dir before creating the file (no ENOTDIR);
//!   - prune never removes a non-empty directory;
//!   - a second execute converges to a no-op (idempotent).
//! - Violation = real ordering / partial-write / wrong-prune bug.

use std::fs;
use std::path::Path;

use cowt_core::merge;
use cowt_core::overlay;
use proptest::prelude::*;
use tempfile::TempDir;

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

fn opaque(upper: &Path, dir: &str) {
    let _ = fs::create_dir_all(upper.join(dir));
    let _ = fs::write(upper.join(dir).join(".wh..wh..opq"), b"");
}

fn scan(p: &Path) -> cowt_core::manifest::Manifest {
    cowt_core::manifest::Manifest::scan(p).unwrap().manifest
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

fn gen_tree(root: &Path, files: &[(String, Option<String>)]) {
    fs::create_dir_all(root).unwrap();
    for (rel, body) in files {
        if let Some(b) = body {
            write(root, rel, b);
        } else {
            whiteout(root, rel);
        }
    }
}

fn arb_path() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("a".to_string()),
        Just("b".to_string()),
        Just("c".to_string()),
        Just("d".to_string()),
        Just("sub".to_string()),
        Just("sub/x".to_string()),
        Just("sub/y".to_string()),
        Just("deep/n".to_string()),
    ]
    .boxed()
}

fn arb_body() -> impl Strategy<Value = Option<String>> {
    prop_oneof![
        Just(Some("v1".to_string())),
        Just(Some("v2-longer".to_string())),
        Just(Some("".to_string())),
        Just(None),
    ]
    .boxed()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn execute_ordering_converges_and_idempotent(
        base_f in proptest::collection::vec((arb_path(), arb_body()), 1..8),
        work_f in proptest::collection::vec((arb_path(), arb_body()), 1..8),
    ) {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("base");
        let upper = tmp.path().join("upper");
        let host = tmp.path().join("host");
        for d in [&base, &upper, &host] {
            fs::create_dir_all(d).unwrap();
        }
        gen_tree(&base, &base_f);
        gen_tree(&upper, &work_f);
        let base_m = scan(&base);
        let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();

        copy_tree(&base, &host);
        let current_m = scan(&host);
        let plan = merge::plan(&base_m, &current_m, &work, &upper);
        prop_assume!(plan.is_clean());

        merge::execute(&plan, &host).unwrap();

        // Invariant: host tree == effective view (file-for-file, content).
        // Ground truth = `work` (the engine's own effective_manifest_fold of
        // base+upper). Re-scanning host into a Manifest and comparing entry
        // bodies via content_eq is the sound oracle (no hand-rolled whiteout
        // replay, which is exactly what A7/functor guarantees).
        let host_m = scan(&host);
        for (path, we) in &work.entries {
            // Directories are not file bodies; compare only file/symlink kinds.
            use cowt_core::manifest::EntryKind;
            match we.kind {
                EntryKind::File | EntryKind::Symlink => {
                    let he = host_m.entries.get(path);
                    prop_assert!(
                        he.map(|e| e.content_eq(we)).unwrap_or(false),
                        "execute: host missing/wrong body for {path:?}"
                    );
                }
                EntryKind::Dir => {}
            }
        }
        // No extra file/symlink in host beyond work.
        for (path, he) in &host_m.entries {
            use cowt_core::manifest::EntryKind;
            if matches!(he.kind, EntryKind::File | EntryKind::Symlink) {
                prop_assert!(
                    work.entries.get(path).map(|e| e.content_eq(he)).unwrap_or(false),
                    "execute: host has extra/unexpected body for {path:?}"
                );
            }
        }

        // Invariant: a second execute is a no-op (idempotent convergence).
        let realized_m = scan(&host);
        let plan2 = merge::plan(&base_m, &realized_m, &work, &upper);
        prop_assert!(plan2.operations.is_empty(), "second execute must be no-op");
    }
}

#[test]
fn dir_to_file_migration_no_enotdir() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    fs::create_dir_all(base.join("d")).unwrap();
    write(&base, "d/old.txt", "x");
    whiteout(&upper, "d");
    write(&upper, "d", "now-a-file");
    let base_m = scan(&base);
    let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    copy_tree(&base, &host);
    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    assert!(plan.is_clean());
    // must not fail with ENOTDIR; `d` must become a file
    merge::execute(&plan, &host).unwrap();
    assert!(host.join("d").is_file(), "dir must have become a file");
    assert_eq!(fs::read_to_string(host.join("d")).unwrap(), "now-a-file");
}

#[test]
fn recreate_under_own_whiteout_survives() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "f.txt", "orig");
    whiteout(&upper, "f.txt");
    write(&upper, "f.txt", "recreated");
    let base_m = scan(&base);
    let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    copy_tree(&base, &host);
    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    assert!(plan.is_clean());
    merge::execute(&plan, &host).unwrap();
    assert_eq!(fs::read_to_string(host.join("f.txt")).unwrap(), "recreated");
}

#[test]
fn whiteout_dir_then_recreate_child_under_opaque() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    fs::create_dir_all(base.join("sub")).unwrap();
    write(&base, "sub/deep.txt", "x");
    opaque(&upper, "sub"); // shadow entire base subtree under sub
    write(&upper, "sub/new.txt", "fresh");
    let base_m = scan(&base);
    let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    copy_tree(&base, &host);
    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    assert!(plan.is_clean());
    merge::execute(&plan, &host).unwrap();
    assert_eq!(fs::read_to_string(host.join("sub/new.txt")).unwrap(), "fresh");
    assert!(!host.join("sub/deep.txt").exists(), "opaque must drop base child");
}
