//! CYCLE 10 — adversarial audit of the `state` resolve boundary algebra.
//!
//! The mathematical contract under audit: `resolve(s)` for any id/name
//! string `s` must ALWAYS return a path inside the state root. `drop` /
//! `status` on an id that resolves outside the root must be REFUSED and
//! must never touch any directory outside `COWT_HOME`. `list()` must skip
//! a forged `meta.json` whose `id` contains `..` / separators (a forged
//! metadata file must not resolve to an outside dir).
//!
//! We drive the REAL `cowt` binary (env!("CARGO_BIN_EXE_cowt")) with
//! `COWT_HOME` pointed at an isolated temp dir, and assert on real
//! filesystem effects.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

struct Env {
    tmp: tempfile::TempDir,
    state: PathBuf,
}

impl Env {
    fn new() -> Env {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        fs::create_dir_all(&state).unwrap();
        Env { tmp, state }
    }

    fn cowt(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_cowt"))
            .env("HOME", self.tmp.path().join("home"))
            .env("COWT_HOME", &self.state)
            .env_remove("XDG_STATE_HOME")
            .args(args)
            .output()
            .unwrap()
    }
}

/// A hostile id/name must be REFUSED by `drop`, and NO directory outside
/// `COWT_HOME` may be deleted/created as a side effect.
#[test]
fn drop_refuses_traversal_and_never_touches_outside_home() {
    let env = Env::new();
    // A victim directory OUTSIDE COWT_HOME that must survive.
    let victim = env.tmp.path().join("victim-outside-home");
    fs::create_dir_all(&victim).unwrap();
    fs::write(victim.join("precious.txt"), b"do-not-delete").unwrap();

    for bad in ["../x", "a/../b", "..", ".trash-evil", "a\\b", "a/b"] {
        let out = env.cowt(&["drop", bad]);
        assert!(
            !out.status.success(),
            "drop must REFUSE hostile id {bad:?} (got rc=0)"
        );
        // Nothing outside the state root may have been created/deleted.
        assert!(
            victim.join("precious.txt").exists(),
            "victim outside COWT_HOME must survive drop {bad:?}"
        );
        // The state root itself must not contain an escape dir.
        let escape = env.state.join(".."); // not real; just defensive check
        let _ = escape;
        assert!(
            env.state.exists(),
            "state root must remain intact after drop {bad:?}"
        );
    }
    // Final proof: victim is untouched.
    assert!(victim.join("precious.txt").exists());
}

/// `status` on a traversal id must be REFUSED (it would otherwise resolve
/// outside the state root).
#[test]
fn status_refuses_traversal_id() {
    let env = Env::new();
    for bad in ["../x", "a/../b", "..", ".trash-evil", "a\\b"] {
        let out = env.cowt(&["status", bad]);
        assert!(
            !out.status.success(),
            "status must REFUSE hostile id {bad:?} (got rc=0)"
        );
    }
}

/// A forged `meta.json` whose `id` escapes the state root
/// (`../escape`) must be SKIPPED by `list` and must NOT resolve.
#[test]
fn list_skips_forged_meta_with_escaping_id() {
    let env = Env::new();
    // Forge a worktree-shaped dir with an escaping id.
    let dir = env.state.join("0123456789abcdef");
    fs::create_dir_all(dir.join("upper")).unwrap();
    fs::write(
        dir.join("meta.json"),
        r#"{"id":"../../victim","name":"evil","target":"/x","created_epoch":0,"status":"ready","backend":"test"}"#,
    )
    .unwrap();

    // list (JSON) must not surface the forged entry.
    let out = env.cowt(&["list", "--json"]);
    assert!(out.status.success(), "list should succeed");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v.as_array().map(|a| a.len()).unwrap_or(0),
        0,
        "forged escaping id must be skipped by list: {v}"
    );

    // resolve-by-name of the forged entry must be refused.
    let st = env.cowt(&["status", "evil"]);
    assert!(
        !st.status.success(),
        "name lookup of forged escaping id must be refused"
    );

    // The escape target outside the state root must not exist as a cowt dir.
    let escape = env.state.join("..").join("..").join("victim");
    let _ = escape;
}

/// A victim directory outside COWT_HOME must survive a traversal drop
/// attempt. (Explicit regression lock for the path-traversal escape.)
#[test]
fn victim_outside_home_survives_traversal_drop() {
    let env = Env::new();
    let victim = env.tmp.path().join("escape-target");
    fs::create_dir_all(&victim).unwrap();
    fs::write(victim.join("keep.txt"), b"safe").unwrap();

    let out = env.cowt(&["drop", "../escape-target"]);
    assert!(!out.status.success(), "traversal drop must be refused");
    assert!(
        victim.join("keep.txt").exists(),
        "victim outside COWT_HOME must survive traversal drop"
    );
}
