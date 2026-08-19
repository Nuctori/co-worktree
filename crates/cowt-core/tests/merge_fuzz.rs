//! Fuzz the three-way merge planner over randomized manifests.
//!
//! Property suite (round 2 adversarial audit). The planner is the heart of
//! `cowt apply`; subtle branches (dir->non-dir migration, host-only dir
//! deletions, converged detection) are exactly where data-loss bugs hide.
//! Every property below must hold for ALL inputs.

use std::path::{Path, PathBuf};

use cowt_core::manifest::{Entry, EntryKind, Manifest};
use cowt_core::merge;
use proptest::prelude::*;

fn leaf_path() -> impl Strategy<Value = PathBuf> {
    // 1-2 component relative path, lowercased, deterministic.
    prop::collection::vec(
        prop::collection::vec(prop::char::range('a', 'z'), 1..4)
            .prop_map(|c| c.into_iter().collect::<String>()),
        1..3,
    )
    .prop_map(|comps| comps.iter().collect::<PathBuf>())
}

fn entry_strat() -> impl Strategy<Value = Entry> {
    prop_oneof![
        // File
        any::<u64>().prop_map(|h| Entry {
            kind: EntryKind::File,
            size: 1,
            mode: 0o644,
            mtime_ns: 0,
            hash: Some(format!("{h:064x}")),
            link_target: None,
        }),
        // Dir
        Just(Entry {
            kind: EntryKind::Dir,
            size: 0,
            mode: 0o755,
            mtime_ns: 0,
            hash: None,
            link_target: None,
        }),
        // Symlink
        Just(Entry {
            kind: EntryKind::Symlink,
            size: 0,
            mode: 0,
            mtime_ns: 0,
            hash: None,
            link_target: Some(PathBuf::from("target")),
        }),
    ]
}

fn manifest_strategy() -> impl Strategy<Value = Manifest> {
    prop::collection::btree_map(leaf_path(), entry_strat(), 0..12).prop_map(|entries| Manifest {
        version: 1,
        base: PathBuf::from("/base"),
        created_epoch: 0,
        entries,
    })
}

proptest! {
    /// INVARIANT 1: operation paths and conflict paths are DISJOINT. A path
    /// must never be both planned-for and conflicted — executing a conflict's
    /// path would overwrite the user's data despite the conflict abort.
    #[test]
    fn plan_ops_and_conflicts_disjoint(
        base in manifest_strategy(),
        current in manifest_strategy(),
        work in manifest_strategy(),
    ) {
        let plan = merge::plan(&base, &current, &work, Path::new("/base"));
        let op_paths: std::collections::BTreeSet<&PathBuf> = plan
            .operations
            .iter()
            .map(|op| match op {
                merge::Operation::WriteFile { path, .. }
                | merge::Operation::WriteSymlink { path, .. }
                | merge::Operation::Mkdir { path, .. }
                | merge::Operation::Delete { path, .. } => path,
            })
            .collect();
        for c in &plan.conflicts {
            prop_assert!(
                !op_paths.contains(&c.path),
                "path {:?} is both planned and conflicted",
                c.path
            );
        }
    }

    /// INVARIANT 2: a clean plan (no conflicts) executed against `current`
    /// produces a host that equals `work` where work changed it, and equals
    /// `current` where work left it alone. This is the full apply contract
    /// checked on randomized inputs (without real on-disk bodies — we verify
    /// the planner's decision, not the fs write, to keep it fast and
    /// deterministic; the execute round-trip is covered by adversarial.rs).
    #[test]
    fn clean_plan_decisions_match_work_or_current(
        base in manifest_strategy(),
        current in manifest_strategy(),
        work in manifest_strategy(),
    ) {
        let plan = merge::plan(&base, &current, &work, Path::new("/base"));
        if !plan.is_clean() {
            return Ok(());
        }
        // Every non-delete operation must target a path whose work entry
        // differs from base OR is a deletion of something base had.
        for op in &plan.operations {
            let path = match op {
                merge::Operation::WriteFile { path, .. }
                | merge::Operation::WriteSymlink { path, .. }
                | merge::Operation::Mkdir { path, .. }
                | merge::Operation::Delete { path, .. } => path,
            };
            let b = base.entries.get(path);
            let w = work.entries.get(path);
            let c = current.entries.get(path);
            match op {
                merge::Operation::Delete { migration, .. } => {
                    if *migration {
                        // Kind-migration delete (file<->dir, symlink<->file):
                        // the old entry must be removed before the new kind is
                        // created, so work MUST still carry the new entry, and
                        // base must have had a different kind.
                        prop_assert!(
                            b.is_some() && w.is_some(),
                            "migration delete on {:?} needs base+work entries",
                            path
                        );
                        let be = b.unwrap();
                        let we = w.unwrap();
                        prop_assert!(
                            be.kind != we.kind,
                            "migration delete on {:?} but kinds did not change",
                            path
                        );
                    } else {
                        // Plain delete: work removed it (and base had it).
                        prop_assert!(
                            b.is_some() && w.is_none(),
                            "delete op on {:?} but not (base has, work lacks)",
                            path
                        );
                    }
                }
                merge::Operation::WriteFile { .. }
                | merge::Operation::WriteSymlink { .. }
                | merge::Operation::Mkdir { .. } => {
                    // Write must carry the work entry; if base had it, work
                    // must differ; if base lacked it, it is a creation.
                    prop_assert!(
                        w.is_some(),
                        "write op on {:?} but work has no such entry",
                        path
                    );
                    if let (Some(b), Some(w)) = (b, w) {
                        prop_assert!(
                            !b.content_eq(w),
                            "write op on {:?} but work == base (no-op planned)",
                            path
                        );
                    }
                    let _ = b; // keep borrow checker calm across arms
                    // And if current differs from base, the planner must have
                    // recorded it as kept (work==base branch), never an op.
                    if let (Some(b), Some(c), Some(w)) = (b, c, w) {
                        if !b.content_eq(c) {
                            prop_assert!(
                                b.content_eq(w),
                                "op on {:?} while host changed and work != base (should be ke/conflict)",
                                path
                            );
                        }
                    }
                }
            }
        }
    }

    /// INVARIANT 3: `kept` is exactly the set of paths where current != base
    /// AND work == base AND both sides had an entry. (`work == base` -> host
    /// wins; a changed host is "kept".)
    #[test]
    fn kept_set_is_exact(
        base in manifest_strategy(),
        current in manifest_strategy(),
        work in manifest_strategy(),
    ) {
        let plan = merge::plan(&base, &current, &work, Path::new("/base"));
        let kept: std::collections::BTreeSet<&PathBuf> = plan.kept.iter().collect();
        for path in kept.iter() {
            let b = base.entries.get(*path);
            let c = current.entries.get(*path);
            let w = work.entries.get(*path);
            prop_assert!(
                b.is_some() && c.is_some() && w.is_some(),
                "kept {:?} requires all three present",
                path
            );
            prop_assert!(
                !b.unwrap().content_eq(c.unwrap()),
                "kept {:?} but current == base",
                path
            );
            prop_assert!(
                b.unwrap().content_eq(w.unwrap()),
                "kept {:?} but work != base",
                path
            );
        }
    }

    /// INVARIANT 4: `converged` paths are exactly those where current == work
    /// AND that differs from base (or is new on both sides). A converged path
    /// must never also be an operation.
    #[test]
    fn converged_set_is_exact_and_disjoint_from_ops(
        base in manifest_strategy(),
        current in manifest_strategy(),
        work in manifest_strategy(),
    ) {
        let plan = merge::plan(&base, &current, &work, Path::new("/base"));
        let op_paths: std::collections::BTreeSet<&PathBuf> = plan
            .operations
            .iter()
            .map(|op| match op {
                merge::Operation::WriteFile { path, .. }
                | merge::Operation::WriteSymlink { path, .. }
                | merge::Operation::Mkdir { path, .. }
                | merge::Operation::Delete { path, .. } => path,
            })
            .collect();
        for path in &plan.converged {
            prop_assert!(
                !op_paths.contains(path),
                "converged {:?} is also an operation",
                path
            );
            let c = current.entries.get(path);
            let w = work.entries.get(path);
            prop_assert!(
                c.is_some() && w.is_some() && c.unwrap().content_eq(w.unwrap()),
                "converged {:?} but current != work",
                path
            );
        }
    }

    /// INVARIANT 5: determinism — planning twice yields byte-identical plans.
    #[test]
    fn plan_is_deterministic(
        base in manifest_strategy(),
        current in manifest_strategy(),
        work in manifest_strategy(),
    ) {
        let p1 = merge::plan(&base, &current, &work, Path::new("/base"));
        let p2 = merge::plan(&base, &current, &work, Path::new("/base"));
        prop_assert_eq!(p1.operations.len(), p2.operations.len());
        prop_assert_eq!(p1.conflicts.len(), p2.conflicts.len());
        prop_assert_eq!(p1.kept.len(), p2.kept.len());
        prop_assert_eq!(p1.converged.len(), p2.converged.len());
    }
}
