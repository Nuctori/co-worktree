//! CYCLE 7 of 10 (serial) — apply soundness w.r.t. differential contract A7.
//!
//! A7 (apply functor): in the normal flow host == base, executing
//! merge::plan(base, current == host, work) must realize `work` EXACTLY —
//! after execute, the host tree must equal the effective view of base+work
//! file-for-file: no path in work missing, no path outside (base ∪ work)
//! wrongly created, deletions effective.
//!
//! We proptest over randomized base/work trees (covering file add/modify/
//! delete, directory add/delete, symlink, opaque dir, and kind-recreate),
//! set host = a copy of base, plan(base, host, work), execute, and compare
//! tree_files(host) against tree_files(work) at manifest granularity
//! (content_eq: kind + size + hash + mode for files; link_target for
//! symlinks; mode for dirs). Any mismatch is a real over-write / under-write
//! / missing-file / phantom-file bug.

use std::fs;
use std::path::{Path, PathBuf};

use cowt_core::manifest::Manifest;
use cowt_core::merge;
use cowt_core::overlay;
use proptest::prelude::*;
use tempfile::TempDir;

// ── helpers (borrowed from cycle6) ───────────────────────────────────────
fn write_file(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    if p.parent()
        .map(|pp| fs::create_dir_all(pp).is_err())
        .unwrap_or(true)
    {
        return;
    }
    let _ = fs::write(p, content);
}

#[allow(dead_code)]
fn write_dir(root: &Path, rel: &str) {
    let _ = fs::create_dir_all(root.join(rel));
}

fn whiteout(upper: &Path, rel: &str) {
    let victim = Path::new(rel);
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

#[cfg(unix)]
fn write_symlink(root: &Path, rel: &str, target: &str) {
    let p = root.join(rel);
    if p.parent()
        .map(|pp| fs::create_dir_all(pp).is_err())
        .unwrap_or(true)
    {
        return;
    }
    let _ = std::os::unix::fs::symlink(target, p);
}

fn scan(p: &Path) -> Manifest {
    Manifest::scan(p).unwrap().manifest
}

fn copy_tree(src: &Path, dst: &Path) {
    for e in fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        let name = e.file_name();
        let src_p = e.path();
        let dst_p = dst.join(&name);
        let meta = std::fs::symlink_metadata(&src_p).unwrap();
        if meta.is_symlink() {
            let target = std::fs::read_link(&src_p).unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, &dst_p).unwrap();
            #[cfg(not(unix))]
            {
                let _ = &target;
                fs::copy(&src_p, &dst_p).unwrap();
            }
        } else if meta.is_dir() {
            fs::create_dir_all(&dst_p).unwrap();
            copy_tree(&src_p, &dst_p);
        } else {
            fs::copy(&src_p, &dst_p).unwrap();
        }
    }
}

// ── spec model ────────────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq)]
enum BaseKind {
    Absent,
    File,
    Dir,
    Sym, // symlink (unix only; downgraded to File on non-unix)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Keep,
    Delete,
    Modify,
    RecreateFile,
    RecreateDir,
    RecreateSym,
    Opaque, // opaque dir (only meaningful for a dir path)
}

const PATHS: &[&str] = &[
    "a",
    "b",
    "sub/x",
    "sub/y",
    "dir/c",
    "dir/deep/z",
    "top",
    "emptyd",
];

fn effective_base_kind(k: BaseKind) -> BaseKind {
    if k == BaseKind::Sym && !cfg!(unix) {
        BaseKind::File
    } else {
        k
    }
}

fn build_base(base: &Path, idx: usize, k: BaseKind) {
    let rel = PATHS[idx];
    match effective_base_kind(k) {
        BaseKind::Absent => {}
        BaseKind::File => write_file(base, rel, &format!("BASE-{}", idx)),
        BaseKind::Dir => {
            let _ = fs::create_dir_all(base.join(rel));
        }
        BaseKind::Sym => {
            #[cfg(unix)]
            write_symlink(base, rel, &format!("tgt-{}", idx));
            #[cfg(not(unix))]
            write_file(base, rel, &format!("BASE-{}", idx));
        }
    }
}

fn build_upper(upper: &Path, idx: usize, k: BaseKind, act: Action, opaque_dirs: &[PathBuf]) {
    let rel = PATHS[idx];
    let relp = Path::new(rel);
    // Children of an opaque directory are shadowed; whiteout them so the
    // deletion is explicit.
    if opaque_dirs
        .iter()
        .any(|d| relp != d.as_path() && relp.starts_with(d))
    {
        if effective_base_kind(k) != BaseKind::Absent {
            whiteout(upper, rel);
        }
        return;
    }
    match act {
        Action::Keep => match effective_base_kind(k) {
            BaseKind::Absent => {}
            BaseKind::File => write_file(upper, rel, &format!("BASE-{}", idx)),
            BaseKind::Dir => {
                let _ = fs::create_dir_all(upper.join(rel));
            }
            BaseKind::Sym => {
                #[cfg(unix)]
                write_symlink(upper, rel, &format!("tgt-{}", idx));
                #[cfg(not(unix))]
                write_file(upper, rel, &format!("BASE-{}", idx));
            }
        },
        Action::Delete => {
            if effective_base_kind(k) != BaseKind::Absent {
                whiteout(upper, rel);
            }
        }
        Action::Modify => write_file(upper, rel, &format!("MOD-{}", idx)),
        Action::RecreateFile => {
            if effective_base_kind(k) != BaseKind::Absent {
                whiteout(upper, rel);
            }
            write_file(upper, rel, &format!("RECREATE-{}", idx));
        }
        Action::RecreateDir => {
            if effective_base_kind(k) != BaseKind::Absent {
                whiteout(upper, rel);
            }
            let _ = fs::create_dir_all(upper.join(rel));
        }
        Action::RecreateSym => {
            if effective_base_kind(k) != BaseKind::Absent {
                whiteout(upper, rel);
            }
            #[cfg(unix)]
            write_symlink(upper, rel, &format!("retgt-{}", idx));
            #[cfg(not(unix))]
            write_file(upper, rel, &format!("RECREATE-{}", idx));
        }
        Action::Opaque => {
            // Applies to a directory path: create the dir, mark opaque, drop
            // children (handled by the shadowing loop above).
            let d = upper.join(rel);
            let _ = fs::create_dir_all(&d);
            let _ = fs::write(d.join(".wh..wh..opq"), b"");
        }
    }
}

/// Compare two manifests file-for-file (presence + content_eq).
fn assert_same_tree(host: &Manifest, work: &Manifest) -> Result<(), Vec<String>> {
    let mut errs = Vec::new();
    let mut keys: std::collections::BTreeSet<&PathBuf> = host.entries.keys().collect();
    for k in work.entries.keys() {
        keys.insert(k);
    }
    for k in keys {
        match (host.entries.get(k), work.entries.get(k)) {
            (None, None) => {}
            (Some(_), None) => errs.push(format!("phantom present on host: {k:?}")),
            (None, Some(_)) => errs.push(format!("missing on host, in work: {k:?}")),
            (Some(h), Some(w)) => {
                if !h.content_eq(w) {
                    errs.push(format!("content mismatch at {k:?}: host={h:?} work={w:?}"));
                }
            }
        }
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs)
    }
}

fn arb_spec() -> impl Strategy<Value = (u8, u8)> {
    // (base_kind 0..=3, action 0..=6)
    (0u8..=3, 0u8..=6)
}

fn decode(k: u8) -> BaseKind {
    match k {
        0 => BaseKind::Absent,
        1 => BaseKind::File,
        2 => BaseKind::Dir,
        _ => BaseKind::Sym,
    }
}

fn decode_act(a: u8) -> Action {
    match a {
        0 => Action::Keep,
        1 => Action::Delete,
        2 => Action::Modify,
        3 => Action::RecreateFile,
        4 => Action::RecreateDir,
        5 => Action::RecreateSym,
        _ => Action::Opaque,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn a7_apply_realizes_work_exactly(specs in proptest::collection::vec(arb_spec(), PATHS.len())) {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("base");
        let upper = tmp.path().join("upper");
        let host = tmp.path().join("host");
        for d in [&base, &upper, &host] {
            fs::create_dir_all(d).unwrap();
        }

        // materialize base
        for (i, (bk, _)) in specs.iter().enumerate() {
            build_base(&base, i, decode(*bk));
        }
        // opaque dir set (only dirs may be opaque; non-dir Opaque -> Delete)
        let mut opaque_dirs: Vec<PathBuf> = Vec::new();
        for (i, (_, a)) in specs.iter().enumerate() {
            if decode_act(*a) == Action::Opaque {
                // only treat as opaque if base is a dir
            if decode(specs[i].0) == BaseKind::Dir {
                    opaque_dirs.push(Path::new(PATHS[i]).to_path_buf());
                }
            }
        }
        // materialize upper
        for (i, (bk, a)) in specs.iter().enumerate() {
            let act = decode_act(*a);
            let act = if act == Action::Opaque && decode(*bk) != BaseKind::Dir {
                Action::Delete
            } else {
                act
            };
            build_upper(&upper, i, decode(*bk), act, &opaque_dirs);
        }

        let base_m = scan(&base);
        let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();

        // normal flow: host is a copy of base; current scanned FROM host.
        copy_tree(&base, &host);
        let current = scan(&host);

        let plan = merge::plan(&base_m, &current, &work, &upper);
        prop_assume!(plan.is_clean());

        merge::execute(&plan, &host)
            .unwrap_or_else(|e| panic!("execute failed: {e:?}"));

        let host_scan = scan(&host);
        match assert_same_tree(&host_scan, &work) {
            Ok(()) => {}
            Err(errs) => prop_assert!(false, "A7 violated:\n{}", errs.join("\n")),
        }
    }
}

// ── explicit edge locks ────────────────────────────────────────────────────

#[test]
fn a7_opaque_dir_realizes_exactly() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    write_file(&base, "dir/c.txt", "c");
    write_file(&base, "dir/deep/z.txt", "z");
    fs::create_dir_all(upper.join("dir")).unwrap();
    // opaque: shadow everything under dir, re-add only c.txt (changed).
    let _ = fs::write(upper.join("dir/.wh..wh..opq"), b"");
    whiteout(&upper, "dir/deep");
    write_file(&upper, "dir/c.txt", "c2");

    let base_m = scan(&base);
    let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    copy_tree(&base, &host);
    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    assert!(
        plan.is_clean(),
        "opaque plan must be clean: {:?}",
        plan.conflicts
    );
    merge::execute(&plan, &host).unwrap();
    let host_scan = scan(&host);
    let errs = assert_same_tree(&host_scan, &work)
        .err()
        .unwrap_or_default();
    assert!(errs.is_empty(), "A7 opaque violated: {errs:?}");
    assert!(host.join("dir/c.txt").exists());
    assert!(!host.join("dir/deep/z.txt").exists());
    assert!(host.join("dir").is_dir());
}

#[cfg(unix)]
#[test]
fn a7_symlink_recreate_as_file() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    std::os::unix::fs::symlink("target.txt", base.join("x")).unwrap();
    std::os::unix::fs::symlink("target.txt", host.join("x")).unwrap();
    whiteout(&upper, "x");
    write_file(&upper, "x", "now-a-file");

    let base_m = scan(&base);
    let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    // host already has x as symlink (copy_tree copies symlink via fs::copy)
    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    assert!(plan.is_clean());
    merge::execute(&plan, &host).unwrap();
    let host_scan = scan(&host);
    let errs = assert_same_tree(&host_scan, &work)
        .err()
        .unwrap_or_default();
    assert!(errs.is_empty(), "A7 symlink->file violated: {errs:?}");
    assert!(host.join("x").is_file());
}

#[test]
fn a7_recreate_file_as_dir_with_child() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let upper = tmp.path().join("upper");
    let host = tmp.path().join("host");
    for d in [&base, &upper, &host] {
        fs::create_dir_all(d).unwrap();
    }
    write_file(&base, "x", "orig");
    whiteout(&upper, "x");
    fs::create_dir_all(upper.join("x")).unwrap();
    write_file(&upper, "x/child.txt", "kid");

    let base_m = scan(&base);
    let work = overlay::effective_manifest_fold(&base_m, &upper, false).unwrap();
    copy_tree(&base, &host);
    let plan = merge::plan(&base_m, &scan(&host), &work, &upper);
    assert!(
        plan.is_clean(),
        "file->dir plan must be clean: {:?}",
        plan.conflicts
    );
    merge::execute(&plan, &host).unwrap();
    let host_scan = scan(&host);
    let errs = assert_same_tree(&host_scan, &work)
        .err()
        .unwrap_or_default();
    assert!(errs.is_empty(), "A7 file->dir violated: {errs:?}");
    assert!(host.join("x/child.txt").is_file());
}
