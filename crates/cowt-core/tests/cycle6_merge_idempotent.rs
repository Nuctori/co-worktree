//! CYCLE 5 of 10 (serial) — merge idempotence-on-host property A6.
//!
//! A6 (merge idempotent on host): build base/current/work where plan is
//! clean (no conflicts). Execute the plan on host == current. Then re-plan
//! against the realized host (plan(base, realized_host, work)). The second
//! plan MUST be the identity (zero operations) — applying twice must not
//! drift, must not re-stage already-applied changes, must not leave phantom
//! operations. Violation = real repeated-apply drift / phantom-op bug.

use std::fs;
use std::path::Path;

use cowt_core::merge;
use cowt_core::overlay;
use proptest::prelude::*;
use tempfile::TempDir;

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    if p.parent()
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

/// Generate a random-ish tree layout from a Vec of (relpath, content-or-deleted)
/// using a small alphabet so keys collide and nest realistically.
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
    // small alphabet; allow nesting with '/'
    let _atom = "[a-z]";
    prop_oneof![
        Just("a".to_string()),
        Just("b".to_string()),
        Just("c".to_string()),
        Just("d".to_string()),
        Just("sub".to_string()),
        Just("sub/x".to_string()),
        Just("sub/y".to_string()),
        Just("deep/n".to_string()),
        Just("a/b".to_string()),
    ]
    .boxed()
}

fn arb_body() -> impl Strategy<Value = Option<String>> {
    prop_oneof![
        Just(Some("v1".to_string())),
        Just(Some("v2-longer".to_string())),
        Just(Some("".to_string())),
        Just(None), // delete via whiteout
    ]
    .boxed()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn a6_idempotent_on_host(files in proptest::collection::vec((arb_path(), arb_body()), 1..10)) {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("base");
        let upper = tmp.path().join("upper");
        let host = tmp.path().join("host");
        for d in [&base, &upper, &host] {
            fs::create_dir_all(d).unwrap();
        }
        gen_tree(&base, &files);
        // work = base overlayed with upper (the worktree view)
        gen_tree(&upper, &files);
        let base_m = scan(&base);
        let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();

        // host starts as a copy of base (current == base == host)
        copy_tree(&base, &host);
        let current_m = scan(&host);

        let plan = merge::plan(&base_m, &current_m, &work, &upper);
        prop_assume!(plan.is_clean());

        // First apply: host == current (base copy). Realize work on host.
        merge::execute(&plan, &host).unwrap();
        let realized = scan(&host);

        // Re-plan against the realized host: must be identity.
        let plan2 = merge::plan(&base_m, &realized, &work, &upper);
        // A6: zero operations.
        prop_assert!(
            plan2.operations.is_empty(),
            "A6 violated: re-plan produced {} ops: {:?}",
            plan2.operations.len(),
            plan2.operations
        );
        prop_assert!(plan2.conflicts.is_empty());
    }
}

#[test]
fn a6_delete_then_recreate_idempotent() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "f.txt", "orig");
    whiteout(&upper, "f.txt");
    write(&upper, "f.txt", "new");
    let base_m = scan(&base);
    let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    copy_tree(&base, &host);
    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    assert!(plan.is_clean());
    merge::execute(&plan, &host).unwrap();
    let realized = scan(&host);
    let plan2 = merge::plan(&base_m, &realized, &work, &upper);
    assert!(plan2.operations.is_empty(), "recreate must be idempotent");
}

#[test]
fn a6_dir_to_symlink_idempotent() {
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
    fs::write(upper.join("d"), b"target").unwrap(); // symlink encoded as file here
    let base_m = scan(&base);
    let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    copy_tree(&base, &host);
    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    assert!(plan.is_clean());
    merge::execute(&plan, &host).unwrap();
    let realized = scan(&host);
    let plan2 = merge::plan(&base_m, &realized, &work, &upper);
    assert!(
        plan2.operations.is_empty(),
        "dir->symlink must be idempotent"
    );
}

#[test]
fn a6_host_only_addition_idempotent() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "a.txt", "v1");
    write(&upper, "a.txt", "v2");
    let base_m = scan(&base);
    let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    copy_tree(&base, &host);
    // host adds a file the worktree never touched
    write(&host, "hostonly.txt", "kept");
    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    assert!(plan.is_clean());
    merge::execute(&plan, &host).unwrap();
    assert_eq!(
        fs::read_to_string(host.join("hostonly.txt")).unwrap(),
        "kept"
    );
    let realized = scan(&host);
    let plan2 = merge::plan(&base_m, &realized, &work, &upper);
    assert!(
        plan2.operations.is_empty(),
        "host-only addition must be idempotent"
    );
}
