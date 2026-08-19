//! Adversarial CLI-level E2E without a real mount: fork a real worktree via
//! the `cowt` binary, hand-synthesize the `upper` layer (whiteouts, new
//! files), then drive `cowt diff --json` / `cowt apply` / `cowt drop` through
//! the real command paths. Exercises the genuine apply/diff/state gates and
//! crash-recovery code on every Linux host (no root/fuse needed).

use std::fs;
use std::path::Path;
use std::process::Command;

struct Env {
    tmp: tempfile::TempDir,
    home: std::path::PathBuf,
    state: std::path::PathBuf,
    app: std::path::PathBuf,
}

impl Env {
    fn new(tag: &str) -> Env {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let app = home.join(".config").join(tag);
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&app).unwrap();
        Env {
            tmp,
            home,
            state,
            app,
        }
    }
    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_cowt"))
            .env("HOME", &self.home)
            .env("COWT_HOME", &self.state)
            .env_remove("XDG_STATE_HOME")
            .args(args)
            .output()
            .unwrap()
    }
    fn run_ok(&self, args: &[&str]) -> String {
        let o = self.run(args);
        assert!(
            o.status.success(),
            "cowt {:?} failed: {}",
            args,
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).into_owned()
    }
    fn only_id(&self) -> String {
        let out = self.run_ok(&["list", "--json"]);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        v.as_array().unwrap().first().unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    }
    fn upper(&self, id: &str) -> std::path::PathBuf {
        self.state.join(id).join("upper")
    }
}

fn write_app(env: &Env, rel: &str, content: &str) {
    let p = env.app.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}
fn write_upper(upper: &Path, rel: &str, content: &str) {
    let p = upper.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}
fn whiteout(upper: &Path, name: &str) {
    write_upper(upper, &format!(".wh.{name}"), "");
}

#[test]
fn cli_a1_diff_reflects_simulated_upper() {
    let env = Env::new("app1");
    write_app(&env, "a.txt", "v1\n");
    write_app(&env, "del.txt", "gone\n");
    write_app(&env, "dir/old.txt", "old\n");
    env.run_ok(&["fork", env.app.to_str().unwrap(), "--name", "app1"]);
    let id = env.only_id();
    let upper = env.upper(&id);
    write_upper(&upper, "a.txt", "v1new\n");
    write_upper(&upper, "new.txt", "fresh\n");
    whiteout(&upper, "del.txt");
    whiteout(&upper, "dir");
    let out = env.run_ok(&["diff", &id, "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let kinds: std::collections::HashMap<String, String> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["path"].as_str().unwrap().replace('\\', "/"),
                c["kind"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(kinds.get("a.txt").map(String::as_str), Some("modified"));
    assert_eq!(kinds.get("new.txt").map(String::as_str), Some("added"));
    assert_eq!(kinds.get("del.txt").map(String::as_str), Some("deleted"));
    assert_eq!(
        kinds.get("dir/old.txt").map(String::as_str),
        Some("deleted")
    );
    assert_eq!(fs::read_to_string(env.app.join("a.txt")).unwrap(), "v1\n");
    assert_eq!(
        fs::read_to_string(env.app.join("del.txt")).unwrap(),
        "gone\n"
    );
    env.run_ok(&["drop", &id]);
}

#[test]
fn cli_a2_apply_writes_and_state_clean() {
    let env = Env::new("app2");
    write_app(&env, "a.txt", "v1\n");
    write_app(&env, "del.txt", "gone\n");
    env.run_ok(&["fork", env.app.to_str().unwrap(), "--name", "app2"]);
    let id = env.only_id();
    let upper = env.upper(&id);
    write_upper(&upper, "a.txt", "v1new\n");
    write_upper(&upper, "new.txt", "fresh\n");
    whiteout(&upper, "del.txt");
    env.run_ok(&["apply", &id]);
    assert_eq!(
        fs::read_to_string(env.app.join("a.txt")).unwrap(),
        "v1new\n"
    );
    assert_eq!(
        fs::read_to_string(env.app.join("new.txt")).unwrap(),
        "fresh\n"
    );
    assert!(!env.app.join("del.txt").exists());
    let out = env.run_ok(&["diff", &id, "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(v.as_array().unwrap().is_empty());
    env.run_ok(&["drop", &id]);
    assert_eq!(fs::read_dir(&env.state).unwrap().count(), 0);
}

#[test]
fn cli_a3_conflict_aborts_and_writes_nothing() {
    let env = Env::new("app3");
    write_app(&env, "shared.txt", "base\n");
    write_app(&env, "other.txt", "stable\n");
    env.run_ok(&["fork", env.app.to_str().unwrap(), "--name", "app3"]);
    let id = env.only_id();
    let upper = env.upper(&id);
    write_upper(&upper, "shared.txt", "worktree\n");
    write_upper(&upper, "clean.txt", "clean\n");
    fs::write(env.app.join("shared.txt"), "host\n").unwrap();
    let real = env.run(&["apply", &id]);
    assert_eq!(real.status.code(), Some(3));
    assert_eq!(
        fs::read_to_string(env.app.join("shared.txt")).unwrap(),
        "host\n"
    );
    assert!(!env.app.join("clean.txt").exists());
    env.run_ok(&["drop", &id]);
}

#[test]
fn cli_a4_stale_pidfile_allows_diff() {
    let env = Env::new("app4");
    write_app(&env, "a.txt", "v1\n");
    env.run_ok(&["fork", env.app.to_str().unwrap(), "--name", "app4"]);
    let id = env.only_id();
    let upper = env.upper(&id);
    write_upper(&upper, "a.txt", "v1new\n");
    let dead_pid = 999_999u32;
    fs::write(env.state.join(&id).join("run.pid"), dead_pid.to_string()).unwrap();
    let alive = Command::new("kill")
        .args(["-0", &dead_pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if alive {
        fs::remove_file(env.state.join(&id).join("run.pid")).unwrap();
        env.run_ok(&["drop", &id]);
        return;
    }
    let out = env.run(&["diff", &id, "--json"]);
    assert!(
        out.status.success(),
        "diff must proceed past stale pidfile: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert!(v
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["path"].as_str().unwrap().ends_with("a.txt")));
    fs::remove_file(env.state.join(&id).join("run.pid")).unwrap();
    env.run_ok(&["drop", &id]);
}

#[test]
fn cli_a5_drop_refuses_traversal() {
    let env = Env::new("app5");
    write_app(&env, "a.txt", "v1\n");
    env.run_ok(&["fork", env.app.to_str().unwrap(), "--name", "app5"]);
    let id = env.only_id();
    for bad in ["../victim", "a/../b", "..", ".trash-evil"] {
        let out = env.run(&["drop", bad]);
        assert!(!out.status.success(), "drop must refuse hostile id {bad:?}");
    }
    env.run_ok(&["drop", &id]);
}

#[test]
fn cli_a6_corrupt_meta_reported_not_hidden() {
    let env = Env::new("app6");
    write_app(&env, "a.txt", "v1\n");
    env.run_ok(&["fork", env.app.to_str().unwrap(), "--name", "app6"]);
    let id = env.only_id();
    fs::write(env.state.join(&id).join("meta.json"), "{not json").unwrap();
    let out = env.run(&["drop", &id]);
    assert!(
        !out.status.success(),
        "drop must refuse corrupt meta without --force"
    );
    let res = env.run(&["diff", &id, "--json"]);
    assert!(!res.status.success());
    env.run_ok(&["drop", &id, "--force"]);
    assert_eq!(fs::read_dir(&env.state).unwrap().count(), 0);
}

#[test]
fn cli_a7_fork_refuses_outside_home() {
    let env = Env::new("app7");
    let outside = env.tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("x.txt"), "x\n").unwrap();
    let out = env.run(&["fork", outside.to_str().unwrap(), "--name", "out"]);
    assert!(
        !out.status.success(),
        "fork must refuse target outside $HOME"
    );
}
