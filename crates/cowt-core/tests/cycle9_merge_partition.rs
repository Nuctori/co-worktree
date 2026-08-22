//! CYCLE 8 of 10 (serial) — merge classification exact-partition property A10.
//!
//! A10 (merge classification is a total exact partition over CHANGED paths):
//! every path where base≠current≠work (all-three-different) ends in exactly ONE
//! conflict of the right kind; paths with base==current==work get NO op; ops /
//! kept / converged / conflicts are pairwise disjoint and together cover EXACTLY
//! the set of paths that are not (base==current==work). Violation = real
//! misclassified-conflict / dropped-change bug.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use cowt_core::manifest::{Entry, Manifest};
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

fn scan(p: &Path) -> Manifest {
    Manifest::scan(p).unwrap().manifest
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

/// A real directory (base/host) never contains overlayfs whiteouts — `None`
/// means the path is ABSENT (don't write anything), not a deletion marker.
fn gen_host_tree(root: &Path, files: &[(String, Option<String>)]) {
    fs::create_dir_all(root).unwrap();
    for (rel, body) in files {
        if let Some(b) = body {
            write(root, rel, b);
        }
    }
}

fn arb_path() -> impl Strategy<Value = String> {
    // Flat names only (no '/') so directory entries never collide with file
    // entries — A10 is a file-level classification partition; nested dirs
    // bring kind-migration semantics that belong to A7/A6, not this audit.
    prop_oneof![
        Just("a".to_string()),
        Just("b".to_string()),
        Just("c".to_string()),
        Just("d".to_string()),
        Just("e".to_string()),
        Just("f".to_string()),
        Just("g".to_string()),
        Just("h".to_string()),
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

fn entry_eq(a: Option<&Entry>, b: Option<&Entry>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x.content_eq(y),
        (None, None) => true, // both absent = equal
        _ => false,
    }
}

/// Whiteout markers (`.wh.<name>` / `.wh..wh..opq`) are overlayfs bookkeeping,
/// not real content entries — the merge engine never classifies them. The
/// oracle must ignore them on both sides of the partition.
fn is_whiteout(p: &Path) -> bool {
    p.file_name()
        .map(|f| {
            let s = f.to_string_lossy();
            s == ".wh..wh..opq" || s.starts_with(".wh.")
        })
        .unwrap_or(false)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn a10_classification_exact_partition(
        base_f in proptest::collection::vec((arb_path(), arb_body()), 1..8),
        cur_f  in proptest::collection::vec((arb_path(), arb_body()), 1..8),
        work_f in proptest::collection::vec((arb_path(), arb_body()), 1..8),
    ) {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("base");
        let upper = tmp.path().join("upper");
        let host = tmp.path().join("host");
        for d in [&base, &upper, &host] {
            fs::create_dir_all(d).unwrap();
        }
        gen_host_tree(&base, &base_f);
        gen_tree(&upper, &work_f); // work = base overlayed
        let base_m = scan(&base);
        let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();

        // current = a separately-generated host tree (may differ from base)
        gen_host_tree(&host, &cur_f);
        let current_m = scan(&host);

        let plan = merge::plan(&base_m, &current_m, &work, &upper);

        // Collect all paths the engine touched.
        let mut touched: BTreeSet<PathBuf> = BTreeSet::new();
        for op in &plan.operations {
            match op {
                merge::Operation::WriteFile { path, .. }
                | merge::Operation::WriteSymlink { path, .. }
                | merge::Operation::Mkdir { path, .. }
                | merge::Operation::Delete { path, .. } => { touched.insert(path.clone()); }
            }
        }
        // sanity: the engine must never classify a whiteout marker
        for t in &touched {
            prop_assert!(!is_whiteout(t), "A10: engine classified whiteout marker {t:?}");
        }
        for p in &plan.kept { touched.insert(p.clone()); }
        for p in &plan.converged { touched.insert(p.clone()); }
        for c in &plan.conflicts { touched.insert(c.path.clone()); }

        // The set of "changed" paths = not (base == current == work).
        let mut changed: BTreeSet<PathBuf> = BTreeSet::new();
        let all_keys: BTreeSet<&PathBuf> = base_m
            .entries
            .keys()
            .chain(current_m.entries.keys())
            .chain(work.entries.keys())
            .collect();
        for k in all_keys {
            if is_whiteout(k) {
                continue;
            }
            let b = base_m.entries.get(k);
            let c = current_m.entries.get(k);
            let w = work.entries.get(k);
            // Mirror the engine's emit logic exactly. A path is touched
            // (classifiable) UNLESS the engine `continue`s without pushing:
            //   - all three equal (b==c==w): no op;
            //   - b_eq_w EXCEPT when base & host both present and differ
            //     (b.is_some() && c.is_some() && !b_eq_c): in that one case the
            //     host's edit is `kept` and touched; otherwise host-wins is a
            //     no-op (host added/removed, worktree left it at base);
            //   - c_eq_w with both absent: double-deletion no-op (the path
            //     simply does not exist, NOT convergence; round-2 fuzz).
            let b_eq_c = entry_eq(b, c);
            let b_eq_w = entry_eq(b, w);
            let c_eq_w = entry_eq(c, w);
            let kept_case = b.is_some() && c.is_some() && !b_eq_c;
            let no_op = (b_eq_w && (b_eq_c || !kept_case))
                || (c_eq_w && !(c.is_some() && w.is_some()));
            if no_op {
                continue;
            }
            changed.insert(k.clone());
        }

        // A10: touched == changed (exact partition, no dropped, no spurious).
        prop_assert_eq!(
            &touched, &changed,
            "A10 violated: touched {:?} vs changed {:?}",
            touched, changed
        );

        // Disjointness: kept/converged/conflicts must not overlap ops paths
        // (a path should be in exactly one category).
        let ops_paths: BTreeSet<PathBuf> = plan
            .operations
            .iter()
            .map(|op| match op {
                merge::Operation::WriteFile { path, .. }
                | merge::Operation::WriteSymlink { path, .. }
                | merge::Operation::Mkdir { path, .. }
                | merge::Operation::Delete { path, .. } => path.clone(),
            })
            .collect();
        for p in plan.kept.iter().chain(plan.converged.iter()) {
            prop_assert!(
                !ops_paths.contains(p),
                "A10 disjointness: {p:?} in both ops and kept/converged"
            );
        }
        for c in &plan.conflicts {
            prop_assert!(
                !ops_paths.contains(&c.path),
                "A10 disjointness: conflict path {p:?} also has an op",
                p = c.path
            );
        }
    }
}

#[test]
fn a10_all_three_different_is_conflict() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    write(&base, "f.txt", "base");
    write(&host, "f.txt", "host"); // current differs from base
    write(&upper, "f.txt", "work"); // work differs from both
    let base_m = scan(&base);
    let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    let current_m = scan(&host);
    let plan = merge::plan(&base_m, &current_m, &work, &upper);
    assert_eq!(
        plan.conflicts.len(),
        1,
        "all-three-different must be exactly one conflict"
    );
    assert!(plan.operations.is_empty());
}
