//! Property tests (proptest) for cowt-core pure functions.
//!
//! Added in the coverage-hardening pass: randomized properties complement
//! the hand-written regression locks by fuzzing the case-fold path
//! semantics and the Windows name validator against arbitrary inputs.

use std::path::{Path, PathBuf};

use cowt_core::manifest::{case_fold_collision_keys, windows_inexpressible_keys, Entry, EntryKind};
use cowt_core::merge::{case_fold_key, case_fold_path_eq};
use proptest::prelude::*;

/// Random relative path: 1-3 components of 1-4 mixed-case letters
/// (deterministic case flip: alternate chars uppercase).
fn arb_rel_path() -> impl Strategy<Value = PathBuf> {
    prop::collection::vec(
        prop::collection::vec(prop::char::range('a', 'z'), 1..4).prop_map(|chars| {
            chars
                .into_iter()
                .enumerate()
                .map(|(i, c)| {
                    if i % 2 == 0 {
                        c.to_ascii_uppercase()
                    } else {
                        c
                    }
                })
                .collect::<String>()
        }),
        1..3,
    )
    .prop_map(|comps| comps.iter().collect::<PathBuf>())
}

/// Random fully-lowercase relative path (for the validator properties).
fn arb_lower_rel_path() -> impl Strategy<Value = PathBuf> {
    prop::collection::vec(
        prop::collection::vec(prop::char::range('a', 'z'), 1..4)
            .prop_map(|chars| chars.into_iter().collect::<String>()),
        1..3,
    )
    .prop_map(|comps| comps.iter().collect::<PathBuf>())
}

fn entry() -> Entry {
    Entry {
        kind: EntryKind::File,
        size: 0,
        mode: 0o644,
        mtime_ns: 0,
        hash: None,
        link_target: None,
    }
}

proptest! {
    /// case_fold_key/case_fold_path_eq must be an equivalence relation and
    /// the key must be the canonical fingerprint (key equality iff path
    /// equality).
    #[test]
    fn case_fold_is_equivalence_relation(
        p1 in arb_rel_path(),
        p2 in arb_rel_path(),
        p3 in arb_rel_path(),
    ) {
        // Determinism.
        prop_assert_eq!(case_fold_key(&p1), case_fold_key(&p1));
        // Reflexivity.
        prop_assert!(case_fold_path_eq(&p1, &p1));
        // Symmetry.
        prop_assert_eq!(
            case_fold_path_eq(&p1, &p2),
            case_fold_path_eq(&p2, &p1)
        );
        // Transitivity.
        if case_fold_path_eq(&p1, &p2) && case_fold_path_eq(&p2, &p3) {
            prop_assert!(case_fold_path_eq(&p1, &p3));
        }
        // Key equality iff path equality (key is the fingerprint).
        prop_assert_eq!(
            case_fold_key(&p1) == case_fold_key(&p2),
            case_fold_path_eq(&p1, &p2)
        );
        // Case-only variants fold equal.
        let upper: PathBuf = p1
            .components()
            .map(|c| {
                PathBuf::from(
                    c.as_os_str()
                        .to_string_lossy()
                        .to_ascii_uppercase(),
                )
            })
            .collect();
        prop_assert!(case_fold_path_eq(&p1, &upper));
    }

    /// case_fold_collision_keys: every reported key must have a collision
    /// partner (same fold key); keys with distinct fold keys are never
    /// reported.
    #[test]
    fn collision_keys_are_consistent(
        p1 in arb_rel_path(),
        p2 in arb_rel_path(),
        p3 in arb_rel_path(),
    ) {
        let mut entries = std::collections::BTreeMap::new();
        entries.insert(p1.clone(), entry());
        entries.insert(p2.clone(), entry());
        entries.insert(p3.clone(), entry());
        let collisions = case_fold_collision_keys(&entries);
        for c in &collisions {
            prop_assert!(
                entries.keys().any(|k| {
                    k != c && case_fold_path_eq(k, c)
                }),
                "reported key {:?} must have a collision partner",
                c
            );
        }
        for k in entries.keys() {
            if collisions.iter().any(|c| c == k) {
                continue;
            }
            prop_assert!(
                !entries.keys().any(|o| o != k && case_fold_path_eq(o, k)),
                "unreported key {:?} must not collide with anything",
                k
            );
        }
    }

    /// windows_inexpressible_keys: reserved-name components (CON/NUL/PRN/
    /// AUX/COM1-9/LPT1-9, any case, any extension) are always reported;
    /// ordinary lowercase names never are.
    #[test]
    fn windows_validator_detects_reserved_and_only_those(
        p in arb_lower_rel_path(),
    ) {
        let mut entries = std::collections::BTreeMap::new();
        entries.insert(p.clone(), entry());
        let bad = windows_inexpressible_keys(&entries);
        let component_is_reserved = p.components().any(|c| {
            let s = c.as_os_str().to_string_lossy();
            let base = s.split('.').next().unwrap_or("");
            let b = base.to_ascii_uppercase();
            b == "CON" || b == "PRN" || b == "AUX" || b == "NUL"
                || ((b.starts_with("COM") || b.starts_with("LPT"))
                    && b.len() == 4
                    && b.as_bytes()[3].is_ascii_digit()
                    && b.as_bytes()[3] != b'0')
        });
        prop_assert_eq!(bad.is_empty(), !component_is_reserved);
        // A reserved name in ANY case/extension position is caught.
        for name in ["CON", "con", "Nul", "aux.log", "COM1", "lpt9.dat"] {
            let mut e2 = entries.clone();
            e2.insert(PathBuf::from(name), entry());
            let bad2 = windows_inexpressible_keys(&e2);
            prop_assert!(
                bad2.iter().any(|k| k == Path::new(name)),
                "{} must be reported",
                name
            );
        }
    }

    /// effective_manifest_fold is idempotent under case folding: folding
    /// the same upper twice yields the same manifest (upper entries,
    /// whiteouts and copy-tmp residues all settle on the first pass).
    /// Requires a real upper dir; the tempfile is created per case and the
    /// random paths are lowercased so NTFS/APFS cannot collapse fixtures.
    #[test]
    fn effective_manifest_fold_is_idempotent(
        base_paths in prop::collection::vec(arb_lower_rel_path(), 1..5),
        upper_paths in prop::collection::vec(arb_lower_rel_path(), 0..5),
        whiteout_names in prop::collection::vec(arb_lower_rel_path(), 0..3),
        with_copy_tmp in proptest::bool::ANY,
    ) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base_dir = tmp.path().join("base");
        let upper = tmp.path().join("upper");
        std::fs::create_dir_all(&base_dir).expect("mkdir base");
        std::fs::create_dir_all(&upper).expect("mkdir upper");

        let mut base_entries = std::collections::BTreeMap::new();
        for p in &base_paths {
            let full = base_dir.join(p);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).expect("mkdir parent");
            }
            std::fs::write(&full, b"x").expect("write base file");
            base_entries.insert(p.clone(), entry());
        }
        let base = cowt_core::Manifest {
            version: 1,
            base: base_dir.clone(),
            created_epoch: 0,
            entries: base_entries,
        };
        // Upper: real files (maybe a case variant of a base file), plus
        // whiteouts and copy-tmp residues.
        for p in &upper_paths {
            let full = upper.join(p);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).expect("mkdir parent");
            }
            std::fs::write(&full, b"y").expect("write upper file");
        }
        for w in &whiteout_names {
            let wh_path = upper.join(format!(".wh.{}", w.display()));
            if let Some(parent) = wh_path.parent() {
                std::fs::create_dir_all(parent).expect("mkdir parent");
            }
            std::fs::write(&wh_path, b"").expect("whiteout");
        }
        if with_copy_tmp {
            std::fs::write(upper.join(".cowt-copy-tmp.residue"), b"torn").expect("residue");
        }

        let once = cowt_core::overlay::effective_manifest_fold(&base, &upper, true)
            .expect("first fold");
        let twice = cowt_core::overlay::effective_manifest_fold(&base, &upper, true)
            .expect("second fold");
        prop_assert_eq!(&once.entries, &twice.entries);
        // Case-fold invariant: no two entries collide by case.
        let keys: Vec<_> = once.entries.keys().cloned().collect();
        for (i, a) in keys.iter().enumerate() {
            for b in &keys[i + 1..] {
                prop_assert!(
                    !case_fold_path_eq(a, b),
                    "folded manifest must not contain case-colliding keys: {:?} vs {:?}",
                    a,
                    b
                );
            }
        }
    }
}
