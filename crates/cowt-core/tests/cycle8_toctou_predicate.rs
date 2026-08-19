//! CYCLE 8 of 10 (serial) — adversarial audit of the A8 verify_unchanged
//! TOCTOU identity predicate.
//!
//! A8 (verify_unchanged is a true identity predicate): between plan and
//! execute, if the host moves out-of-band (content OR mode OR mtime differs
//! from the snapshot taken at plan time), execute MUST abort; and on abort the
//! host's edited content must be preserved INTACT (no partial write, no
//! clobber). The predicate must key on content identity (hash), not merely
//! size/mtime.
//!
//! This file locks:
//!   (a) host edits a TARGETED file (different CONTENT, same size+mtime) ->
//!       execute aborts AND host content preserved;
//!   (b) host edits a NON-targeted file -> execute still proceeds (path-scoped
//!       guard, not global);
//!   (c) identical host -> execute proceeds;
//!   (d) fuzz over which host file moves + what changes (content vs mode) ->
//!       assert abort-vs-proceed and content-intact on abort.

use std::fs;
use std::path::{Path, PathBuf};

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

fn set_mtime(p: &Path, t: std::time::SystemTime) {
    filetime::set_file_mtime(p, t.into()).unwrap();
}

#[cfg(unix)]
fn chmod_readonly(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(p, fs::Permissions::from_mode(0o444)).unwrap();
}

// ---------------------------------------------------------------- (c)

/// Identical host (no out-of-band change) -> execute proceeds and applies the
/// planned write.
#[test]
fn a8_identical_host_proceeds() {
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
    assert!(plan.is_clean(), "conflicts: {:?}", plan.conflicts);
    let report = merge::execute(&plan, &host).unwrap();
    assert_eq!(report.written, 1);
    assert_eq!(fs::read_to_string(host.join("x.txt")).unwrap(), "v2");
}

// ---------------------------------------------------------------- (a)

/// Host edits a TARGETED file with DIFFERENT content same size + same mtime
/// (forged) -> execute must abort and host content must be preserved intact.
#[test]
fn a8_targeted_content_same_size_mtime_aborts_and_preserves() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let host = tmp.path().join("host");
    let work = tmp.path().join("work");
    for d in [&base, &host, &work] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "x.txt", "v1"); // 2 bytes
    write(&host, "x.txt", "v1");
    write(&work, "x.txt", "v2");

    let current = scan(&host);
    let plan = merge::plan(&scan(&base), &current, &scan(&work), &work);
    assert!(plan.is_clean());
    assert!(plan.expected_current.contains_key(&PathBuf::from("x.txt")));

    // Host edits x.txt to different content, SAME size, SAME mtime (forged via
    // touch -r) — a content-only rewrite that size/mtime alone cannot detect.
    let orig_mtime = fs::symlink_metadata(host.join("x.txt"))
        .unwrap()
        .modified()
        .unwrap();
    fs::write(host.join("x.txt"), "A9").unwrap(); // 2 bytes, different content
    set_mtime(&host.join("x.txt"), orig_mtime);

    let err = merge::execute(&plan, &host);
    assert!(err.is_err(), "content-only (same size+mtime) host edit must abort");
    assert_eq!(
        fs::read_to_string(host.join("x.txt")).unwrap(),
        "A9",
        "host edited content must survive the aborted apply (no clobber)"
    );
}

/// Host edits a TARGETED file with DIFFERENT content AND different size ->
/// execute must abort and host content preserved.
#[test]
fn a8_targeted_content_diff_size_aborts_and_preserves() {
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

    fs::write(host.join("x.txt"), "HOST-MUCH-LONGER-EDIT").unwrap();
    let err = merge::execute(&plan, &host);
    assert!(err.is_err(), "host size change must abort");
    assert_eq!(
        fs::read_to_string(host.join("x.txt")).unwrap(),
        "HOST-MUCH-LONGER-EDIT"
    );
}

/// Host chmods a TARGETED file (mode change, same content) -> execute must
/// abort on unix (mode counts as content per content_eq / round-30).
#[cfg(unix)]
#[test]
fn a8_targeted_mode_change_aborts_and_preserves() {
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

    chmod_readonly(host.join("x.txt"));
    let err = merge::execute(&plan, &host);
    assert!(err.is_err(), "host chmod in plan->execute window must abort");
    assert_eq!(fs::read_to_string(host.join("x.txt")).unwrap(), "v1");
}

// ---------------------------------------------------------------- (b)

/// Host edits a NON-targeted file -> execute still proceeds (guard is
/// path-scoped). The non-targeted file is left untouched; the targeted write
/// still applies.
#[test]
fn a8_nontargeted_edit_still_proceeds() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let host = tmp.path().join("host");
    let work = tmp.path().join("work");
    for d in [&base, &host, &work] {
        fs::create_dir_all(d).unwrap();
    }
    // base has x and y; only x is changed by the worktree.
    write(&base, "x.txt", "v1");
    write(&base, "y.txt", "v1");
    write(&host, "x.txt", "v1");
    write(&host, "y.txt", "v1");
    write(&work, "x.txt", "v2");
    write(&work, "y.txt", "v1");

    let plan = merge::plan(&scan(&base), &scan(&host), &scan(&work), &work);
    assert!(plan.is_clean());
    // Plan touches only x.txt (y.txt converges host==work==base).
    assert!(plan.expected_current.contains_key(&PathBuf::from("x.txt")));
    assert!(!plan.expected_current.contains_key(&PathBuf::from("y.txt")));

    // Host edits the NON-targeted y.txt out-of-band.
    let orig_mtime = fs::symlink_metadata(host.join("y.txt"))
        .unwrap()
        .modified()
        .unwrap();
    fs::write(host.join("y.txt"), "HOST-Y-EDIT").unwrap();
    set_mtime(&host.join("y.txt"), orig_mtime);

    // Execute must still proceed (guard is path-scoped, not global).
    let report = merge::execute(&plan, &host).unwrap();
    assert_eq!(report.written, 1);
    // Targeted write applied.
    assert_eq!(fs::read_to_string(host.join("x.txt")).unwrap(), "v2");
    // Non-targeted host edit preserved (never touched).
    assert_eq!(
        fs::read_to_string(host.join("y.txt")).unwrap(),
        "HOST-Y-EDIT"
    );
}

// ---------------------------------------------------------------- (d) fuzz

use proptest::prelude::*;

fn arb_target() -> impl Strategy<Value = u8> {
    prop_oneof![Just(0u8), Just(1u8), Just(2u8)]
}

fn arb_mutate_file() -> impl Strategy<Value = u8> {
    prop_oneof![Just(0u8), Just(1u8), Just(2u8)]
}

fn arb_mutation() -> impl Strategy<Value = u8> {
    // 0 = none, 1 = content different size, 2 = content same size,
    // 3 = mode change (unix only; on non-unix treated as no-op anyway).
    prop_oneof![Just(0u8), Just(1u8), Just(2u8), Just(3u8)]
}

fn rel_of(i: u8) -> &'static str {
    match i {
        0 => "a.txt",
        1 => "b.txt",
        _ => "c.txt",
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(250))]

    #[test]
    fn a8_fuzz_which_file_moves_and_what_changes(
        target in arb_target(),
        mutate_file in arb_mutate_file(),
        mutation in arb_mutation(),
    ) {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("base");
        let host = tmp.path().join("host");
        let work = tmp.path().join("work");
        for d in [&base, &host, &work] {
            fs::create_dir_all(d).unwrap();
        }
        // base: a/b/c = "v1". work = full base with `target` changed to "v2".
        for i in 0..3u8 {
            let r = rel_of(i);
            write(&base, r, "v1");
            write(&host, r, "v1");
            write(&work, r, if i == target { "v2" } else { "v1" });
        }

        // Plan against the host BEFORE the out-of-band edit.
        let current = scan(&host);
        let plan = merge::plan(&scan(&base), &current, &scan(&work), &work);
        prop_assume!(plan.is_clean());
        prop_assume!(plan.expected_current.contains_key(&PathBuf::from(rel_of(target))));

        let target_rel = rel_of(target);
        let mutate_rel = rel_of(mutate_file);
        let mut expected_abort = false;

        // Apply the out-of-band mutation to the host.
        match mutation {
            0 => {} // no change
            1 => {
                // different size -> len check fails -> abort iff targeted.
                fs::write(host.join(mutate_rel), "HOST-MUCH-LONGER-EDIT").unwrap();
                if mutate_file == target {
                    expected_abort = true;
                }
            }
            2 => {
                // same size (2 bytes), different content, forged mtime.
                let orig = fs::symlink_metadata(host.join(mutate_rel))
                    .unwrap()
                    .modified()
                    .unwrap();
                fs::write(host.join(mutate_rel), "z9").unwrap(); // 2 bytes
                set_mtime(&host.join(mutate_rel), orig);
                if mutate_file == target {
                    expected_abort = true; // hash must catch the content change
                }
            }
            3 => {
                #[cfg(unix)]
                {
                    chmod_readonly(host.join(mutate_rel));
                    if mutate_file == target {
                        expected_abort = true;
                    }
                }
                #[cfg(not(unix))]
                {
                    // mode is not tracked off unix; treat as no change.
                }
            }
            _ => unreachable!(),
        }

        let result = merge::execute(&plan, &host);

        if expected_abort {
            prop_assert!(
                result.is_err(),
                "host moved targeted file {mutate_rel} (mutation {mutation}); execute must abort"
            );
            // Host edited content preserved intact on abort.
            if mutation == 1 {
                prop_assert_eq!(
                    fs::read_to_string(host.join(mutate_rel)).unwrap(),
                    "HOST-MUCH-LONGER-EDIT"
                );
            } else if mutation == 2 {
                prop_assert_eq!(fs::read_to_string(host.join(mutate_rel)).unwrap(), "z9");
            }
        } else {
            prop_assert!(
                result.is_ok(),
                "host only moved non-targeted file {mutate_rel}; execute must proceed: {:?}",
                result.err()
            );
            // Targeted write still applied.
            prop_assert_eq!(fs::read_to_string(host.join(target_rel)).unwrap(), "v2");
        }
    }
}

/// Edge probe (unix, non-root): a host file that was unreadable at plan time
/// carries hash=None in the snapshot. verify_unchanged then SKIPS the content
/// hash gate (`is_none_or`) and falls back to size+mtime — so a content-only
/// rewrite (same size+mtime) of a previously-unreadable file is NOT detected.
/// This documents the known degradation of A8 for unreadable-at-plan-time
/// files; it is a suspected (not confirmed-in-CI) guard gap, not the readable
/// contract which the tests above lock.
#[cfg(unix)]
#[test]
fn a8_probe_unreadable_hash_none_degradation() {
    use std::os::unix::fs::PermissionsExt;
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
    // Make x.txt unreadable so the plan-time scan cannot hash it.
    fs::set_permissions(host.join("x.txt"), fs::Permissions::from_mode(0o000)).unwrap();
    let scan_err = Manifest::scan(&host).is_err();
    // If we are root, chmod 000 is still readable -> hash present -> predicate
    // still holds. Treat root runs as not-applicable.
    if scan_err {
        // scanner failed entirely; cannot plan. skip (not a predicate bug).
        return;
    }
    let host2 = scan(&host);
    let e = host2.get(Path::new("x.txt")).unwrap();
    if e.hash.is_some() {
        // root/readable: predicate has a hash, A8 holds — nothing to probe.
        return;
    }
    let plan = merge::plan(&scan(&base), &host2, &scan(&work), &work);
    if !plan.is_clean() {
        return;
    }
    // Restore readability and rewrite content, same size, forged mtime.
    fs::set_permissions(host.join("x.txt"), fs::Permissions::from_mode(0o644)).unwrap();
    let orig = fs::symlink_metadata(host.join("x.txt")).unwrap().modified().unwrap();
    fs::write(host.join("x.txt"), "A9").unwrap();
    set_mtime(&host.join("x.txt"), orig);
    let result = merge::execute(&plan, &host);
    // SUSPECTED BUG: with hash=None the predicate ignores the content change
    // and proceeds, overwriting the host edit. We assert the STRONG contract
    // (must abort); if it fails, A8 degrades for unreadable-at-plan files.
    assert!(
        result.is_err(),
        "A8 degraded: unreadable-at-plan file changed content (same size+mtime) but execute proceeded"
    );
}
