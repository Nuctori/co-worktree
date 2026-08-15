//! CLI-level integration tests. FUSE-dependent cases skip automatically when
//! no fuse-overlayfs backend is available (e.g. Windows/macOS CI runners).

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use tempfile::TempDir;

struct Env {
    _tmp: TempDir,
    home: PathBuf,
    state: PathBuf,
    target: PathBuf,
}

impl Env {
    fn new() -> Env {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let target = home.join(".config/demoapp");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("config.txt"), "alpha\nbeta\n").unwrap();
        fs::write(target.join("prefs.json"), r#"{"a": 1, "b": 2}"#).unwrap();
        fs::write(target.join("stale.cache"), "stale").unwrap();
        Env {
            _tmp: tmp,
            home,
            state,
            target,
        }
    }

    fn cowt(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_cowt"));
        c.env("HOME", &self.home)
            .env("COWT_HOME", &self.state)
            // Deterministic PATH: cargo test inherits a full PATH which is
            // fine, but we must not leak a caller's COWT state.
            .env_remove("XDG_STATE_HOME");
        c
    }

    fn cowt_ok(&self, args: &[&str]) -> String {
        let out = self.cowt().args(args).output().unwrap();
        assert!(
            out.status.success(),
            "cowt {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn fork(&self) -> String {
        let out = self
            .cowt()
            .args(["fork", self.target.to_str().unwrap(), "--name", "demo"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "fork failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        "demo".to_string()
    }

    fn upper(&self) -> PathBuf {
        self.state_dir().join("upper")
    }

    /// The state directory of the first (only) worktree — state dirs are
    /// keyed by id, not name.
    fn state_dir(&self) -> PathBuf {
        // Resolve id via list --json.
        let out = self.cowt().args(["list", "--json"]).output().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("list --json parses");
        let id = v[0]["id"].as_str().unwrap();
        self.state.join(id)
    }

    fn fuse_available(&self) -> bool {
        let out = self.cowt().arg("doctor").output().unwrap();
        String::from_utf8_lossy(&out.stdout).contains("available: yes")
    }
}

#[test]
fn fork_creates_metadata_only_worktree() {
    let env = Env::new();
    env.fork();
    let out = env.cowt().args(["list", "--json"]).output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 1);
    assert_eq!(v[0]["name"], "demo");
    assert_eq!(v[0]["status"], "ready");
    // Upper must be empty: no data copied at fork time.
    assert_eq!(fs::read_dir(env.upper()).unwrap().count(), 0);
}

#[test]
fn fork_refuses_directories_outside_home() {
    let env = Env::new();
    let outside = env.state.parent().unwrap().join("etc-like");
    fs::create_dir_all(&outside).unwrap();
    let out = env
        .cowt()
        .args(["fork", outside.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("$HOME"));
}

#[test]
fn diff_and_apply_without_fuse_via_upper_layer() {
    let env = Env::new();
    env.fork();
    let upper = env.upper();

    // Simulate what an isolated process would have written.
    fs::write(upper.join("config.txt"), "alpha\nGAMMA\n").unwrap();
    fs::write(upper.join("prefs.json"), r#"{"a": 1, "b": 3, "c": 4}"#).unwrap();
    fs::write(upper.join(".wh.stale.cache"), "").unwrap(); // whiteout
    fs::create_dir_all(upper.join("newdir")).unwrap();
    fs::write(upper.join("newdir/new.txt"), "fresh").unwrap();

    // diff --json
    let out = env
        .cowt()
        .args(["diff", "demo", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let changes: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let find = |p: &str| {
        changes
            .as_array()
            .unwrap()
            .iter()
            // Windows serializes paths with `\`; normalize both sides.
            .find(|c| c["path"].as_str().unwrap().replace('\\', "/") == p)
            .cloned()
    };
    assert_eq!(find("config.txt").unwrap()["kind"], "modified");
    assert_eq!(find("prefs.json").unwrap()["kind"], "modified");
    assert_eq!(find("stale.cache").unwrap()["kind"], "deleted");
    assert_eq!(find("newdir/new.txt").unwrap()["kind"], "added");

    // apply
    let out = env.cowt().args(["apply", "demo"]).output().unwrap();
    assert!(
        out.status.success(),
        "apply failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read_to_string(env.target.join("config.txt")).unwrap(),
        "alpha\nGAMMA\n"
    );
    assert!(!env.target.join("stale.cache").exists());
    assert_eq!(
        fs::read_to_string(env.target.join("newdir/new.txt")).unwrap(),
        "fresh"
    );
    let prefs: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(env.target.join("prefs.json")).unwrap()).unwrap();
    assert_eq!(prefs["b"], 3);
    assert_eq!(prefs["c"], 4);
}

#[test]
fn apply_conflict_aborts_with_zero_pollution_and_exit_3() {
    let env = Env::new();
    env.fork();
    let upper = env.upper();

    // Worktree changes the file; host changes it too, differently.
    fs::write(upper.join("config.txt"), "worktree-version\n").unwrap();
    fs::write(env.target.join("config.txt"), "host-version\n").unwrap();
    // A clean change that must NOT be written either (atomic abort).
    fs::write(upper.join("clean.txt"), "clean\n").unwrap();

    let out = env
        .cowt()
        .args(["apply", "demo", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["status"], "conflict");
    let conflicts = v["conflicts"].as_array().unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0]["path"], "config.txt");
    assert_eq!(conflicts[0]["kind"], "both_modified");
    assert!(conflicts[0]["base_hash"].is_string());
    assert!(conflicts[0]["current_hash"].is_string());
    assert!(conflicts[0]["work_hash"].is_string());

    // Zero pollution.
    assert_eq!(
        fs::read_to_string(env.target.join("config.txt")).unwrap(),
        "host-version\n"
    );
    assert!(!env.target.join("clean.txt").exists());

    // --dry-run reports the same conflict without writing.
    let out = env
        .cowt()
        .args(["apply", "demo", "--dry-run", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["conflicts"].as_array().unwrap().len(), 1);
}

#[test]
fn drop_removes_all_state() {
    let env = Env::new();
    env.fork();
    let out = env.cowt().args(["drop", "demo"]).output().unwrap();
    assert!(
        out.status.success(),
        "drop failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(fs::read_dir(&env.state).unwrap().count(), 0);
    // Host untouched.
    assert_eq!(
        fs::read_to_string(env.target.join("config.txt")).unwrap(),
        "alpha\nbeta\n"
    );
}

/// Platform-appropriate commands that mutate files inside the isolated view.
fn mutate_cmd(target: &std::path::Path) -> Vec<String> {
    #[cfg(unix)]
    {
        let script = format!(
            "cd \"{}\" || exit 1\nsed -i 's/beta/BETA/' config.txt\nrm stale.cache\necho isolated > isolated.txt",
            target.display()
        );
        vec!["sh".into(), "-c".into(), script]
    }
    #[cfg(windows)]
    {
        let d = target.display().to_string().replace('/', "\\");
        // .NET WriteAllText keeps LF line endings (Set-Content would add CRLF).
        let script = format!(
            "$d='{d}'; $c = (Get-Content \"$d\\config.txt\") -replace 'beta','BETA'; \
             [IO.File]::WriteAllText(\"$d\\config.txt\", ($c -join \"`n\") + \"`n\"); \
             Remove-Item \"$d\\stale.cache\"; \
             [IO.File]::WriteAllText(\"$d\\isolated.txt\", \"isolated`n\")"
        );
        vec![
            "powershell".into(),
            "-NoProfile".into(),
            "-Command".into(),
            script,
        ]
    }
}

/// Command that writes a file then dies without any cleanup (SIGKILL / abort).
fn crash_cmd(target: &std::path::Path) -> Vec<String> {
    #[cfg(unix)]
    {
        vec![
            "sh".into(),
            "-c".into(),
            format!(
                "echo crash-data > \"{}/crash.txt\"; kill -9 $$",
                target.display()
            ),
        ]
    }
    #[cfg(windows)]
    {
        vec![
            "powershell".into(),
            "-NoProfile".into(),
            "-Command".into(),
            format!(
                "Set-Content -Path '{}\\crash.txt' -Value 'crash-data'; exit 99",
                target.display()
            ),
        ]
    }
}

/// A long-running command for background runs.
fn sleep_cmd() -> Vec<String> {
    #[cfg(unix)]
    {
        vec!["sleep".into(), "30".into()]
    }
    #[cfg(windows)]
    {
        vec![
            "powershell".into(),
            "-NoProfile".into(),
            "-Command".into(),
            "Start-Sleep -Seconds 30".into(),
        ]
    }
}

/// Assert that nothing is mounted / junctioned at `target` anymore.
#[cfg(target_os = "linux")]
fn assert_mount_gone(target: &std::path::Path) {
    let mounts = fs::read_to_string("/proc/self/mounts").unwrap();
    assert!(!mounts.contains(target.to_str().unwrap()));
}

#[cfg(target_os = "macos")]
fn assert_mount_gone(target: &std::path::Path) {
    let out = std::process::Command::new("mount").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains(&format!(" on {} ", target.display())));
}

#[cfg(windows)]
fn assert_mount_gone(target: &std::path::Path) {
    // The junction is removed on teardown; the target is a plain dir again.
    use std::os::windows::fs::MetadataExt;
    let meta = fs::symlink_metadata(target).expect("target dir exists");
    assert!(meta.is_dir());
    assert_eq!(
        meta.file_attributes() & 0x400, /* FILE_ATTRIBUTE_REPARSE_POINT */
        0,
        "target still a reparse point (junction)"
    );
}

#[test]
fn fuse_full_lifecycle() {
    let env = Env::new();
    if !env.fuse_available() {
        eprintln!("backend unavailable; skipping");
        return;
    }
    env.fork();

    // Isolated run: modify, delete, create.
    let cmd = mutate_cmd(&env.target);
    let mut cowt = env.cowt();
    cowt.args(["run", "demo", "--"]).args(&cmd);
    let out = cowt.output().unwrap();
    assert!(
        out.status.success(),
        "run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Host must be untouched during/after run.
    assert_eq!(
        fs::read_to_string(env.target.join("config.txt")).unwrap(),
        "alpha\nbeta\n"
    );
    assert!(env.target.join("stale.cache").exists());

    // Diff sees exactly the isolated changes.
    let out = env
        .cowt()
        .args(["diff", "demo", "--json"])
        .output()
        .unwrap();
    let changes: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let kinds: std::collections::BTreeMap<String, String> = changes
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["path"].as_str().unwrap().to_string(),
                c["kind"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        kinds.get("config.txt").map(String::as_str),
        Some("modified")
    );
    assert_eq!(
        kinds.get("stale.cache").map(String::as_str),
        Some("deleted")
    );
    assert_eq!(kinds.get("isolated.txt").map(String::as_str), Some("added"));

    // Apply merges them into the host.
    let out = env.cowt().args(["apply", "demo"]).output().unwrap();
    assert!(out.status.success());
    assert_eq!(
        fs::read_to_string(env.target.join("config.txt")).unwrap(),
        "alpha\nBETA\n"
    );
    assert!(!env.target.join("stale.cache").exists());
    assert_eq!(
        fs::read_to_string(env.target.join("isolated.txt")).unwrap(),
        "isolated\n"
    );

    // Drop leaves zero residue.
    let out = env.cowt().args(["drop", "demo"]).output().unwrap();
    assert!(out.status.success());
    assert_eq!(fs::read_dir(&env.state).unwrap().count(), 0);
}

#[test]
fn fuse_crash_preserves_upper() {
    let env = Env::new();
    if !env.fuse_available() {
        eprintln!("fuse-overlayfs unavailable; skipping");
        return;
    }
    env.fork();
    // Process writes then dies without any cleanup (SIGKILL on unix).
    let cmd = crash_cmd(&env.target);
    let mut cowt = env.cowt();
    cowt.args(["run", "demo", "--"]).args(&cmd);
    let out = cowt.output().unwrap();
    // Non-zero exit (killed), but the run command itself must handle it.
    assert_ne!(out.status.code(), Some(0));

    // Upper data intact and diffable.
    let out = env
        .cowt()
        .args(["diff", "demo", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let changes: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(changes
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["path"] == "crash.txt" && c["kind"] == "added"));
    // Host still untouched.
    assert!(!env.target.join("crash.txt").exists());
}

#[test]
fn drop_refuses_while_running() {
    let env = Env::new();
    if !env.fuse_available() {
        eprintln!("fuse-overlayfs unavailable; skipping");
        return;
    }
    env.fork();

    // Start a long-running process in the background.
    let cmd = sleep_cmd();
    let mut cowt = env.cowt();
    cowt.args(["run", "demo", "--"]).args(&cmd);
    let mut child = cowt.spawn().unwrap();
    // Wait until the pidfile appears.
    let state_dir = env.state.clone();
    let mut pid_seen = false;
    for _ in 0..50 {
        let entries: Vec<_> = fs::read_dir(&state_dir).unwrap().flatten().collect();
        if let Some(d) = entries.first() {
            if d.path().join("run.pid").exists() {
                pid_seen = true;
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(pid_seen, "run.pid never appeared");

    // drop without --force must refuse.
    let out = env.cowt().args(["drop", "demo"]).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("running"));

    // drop --force kills and cleans up.
    let out = env
        .cowt()
        .args(["drop", "demo", "--force"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "force drop failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = child.wait();
    assert_eq!(fs::read_dir(&env.state).unwrap().count(), 0);
    // Mount / junction gone.
    assert_mount_gone(&env.target);
}

#[test]
fn resolve_rejects_path_traversal_ids() {
    let env = Env::new();
    // Seed a victim directory OUTSIDE the state root with a meta.json.
    let victim = env._tmp.path().join("victim");
    fs::create_dir_all(&victim).unwrap();
    fs::write(victim.join("important.txt"), "keep me\n").unwrap();
    let fake_meta = serde_json::json!({
        "id": "victim",
        "name": "victim",
        "target": victim,
        "created_epoch": 0,
        "status": "ready",
        "backend": "test"
    });
    fs::write(
        victim.join("meta.json"),
        serde_json::to_string_pretty(&fake_meta).unwrap(),
    )
    .unwrap();

    // `drop ../victim` must be refused — and must NOT delete the victim.
    let out = env.cowt().args(["drop", "../victim"]).output().unwrap();
    assert!(!out.status.success(), "traversal drop must be refused");
    assert!(
        victim.join("important.txt").exists(),
        "victim directory must survive"
    );
    // Same for diff/apply/status.
    for cmd in ["diff", "apply", "status"] {
        let out = env.cowt().args([cmd, "../victim"]).output().unwrap();
        assert!(
            !out.status.success(),
            "{cmd} with traversal id must be refused"
        );
    }
}

#[test]
fn iterative_apply_run_apply_workflow() {
    // Regression for round-19: after apply, the baseline advances, so a
    // second run+apply cycle must not false-conflict against the stale fork
    // snapshot, and deleting a previously-applied file must land.
    let env = Env::new();
    let _ = env.fork();
    let upper = env.upper();
    // Simulate run 1: modify config.txt through the view (write upper).
    fs::create_dir_all(&upper).unwrap();
    fs::write(upper.join("config.txt"), "BETA\n").unwrap();
    let out = env.cowt().args(["apply", "demo"]).output().unwrap();
    assert!(out.status.success(), "apply 1 failed");
    assert_eq!(
        fs::read_to_string(env.target.join("config.txt")).unwrap(),
        "BETA\n"
    );

    // Run 2: edit again + add a file; apply must succeed (no false conflict).
    fs::write(upper.join("config.txt"), "BETA2\n").unwrap();
    fs::write(upper.join("new.txt"), "added\n").unwrap();
    let out = env.cowt().args(["apply", "demo"]).output().unwrap();
    assert!(
        out.status.success(),
        "second apply must succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read_to_string(env.target.join("config.txt")).unwrap(),
        "BETA2\n"
    );
    assert_eq!(
        fs::read_to_string(env.target.join("new.txt")).unwrap(),
        "added\n"
    );

    // Run 3: delete the previously-applied file through the view. The layer
    // was reset by apply 2, so the file now lives in the host (lower) —
    // deleting it through the view produces a whiteout.
    fs::write(upper.join(".wh.new.txt"), b"").unwrap();
    let out = env.cowt().args(["apply", "demo"]).output().unwrap();
    assert!(out.status.success(), "apply 3 failed");
    assert!(
        !env.target.join("new.txt").exists(),
        "deletion of a previously-applied file must reach the host"
    );
    env.cowt_ok(&["drop", "demo"]);
}

// ---------------------------------------------------------------- R22

/// Round-22: `fork --name` must reject names the resolver itself would
/// refuse (empty, separators, `..` substrings) — the tool must never create
/// a worktree it cannot resolve by name.
#[test]
fn fork_rejects_invalid_names() {
    let env = Env::new();
    for bad in ["a..b", "dir/name", "..evil", ""] {
        let out = env
            .cowt()
            .args(["fork", env.target.to_str().unwrap(), "--name", bad])
            .output()
            .unwrap();
        assert!(!out.status.success(), "fork --name {bad:?} must be refused");
    }
    // A name that merely contains a dot (not `..`) is still fine.
    let out = env
        .cowt()
        .args(["fork", env.target.to_str().unwrap(), "--name", "a.b"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "fork --name a.b must succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Round-22: a worktree name must not shadow an existing worktree id —
/// resolve() prefers the id-direct lookup, so `fork --name <existing-id>`
/// would create a worktree that is permanently unreachable by name, and
/// `drop <that name>` would hit the WRONG worktree.
#[test]
fn fork_rejects_name_colliding_with_existing_id() {
    let env = Env::new();
    env.fork();
    // Grab the existing worktree's id (list --json).
    let out = env.cowt().args(["list", "--json"]).output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let id = v[0]["id"].as_str().unwrap().to_string();
    assert_eq!(id.len(), 16, "ids are 16 hex chars, got {id}");

    // Forking with --name == existing id must be refused.
    let out = env
        .cowt()
        .args(["fork", env.target.to_str().unwrap(), "--name", &id])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "fork --name <existing-id> must be refused: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Round-22: `resolve(".")` must be rejected exactly like `".."` (it would
/// otherwise resolve to the state root itself and, under a misconfigured
/// COWT_HOME pointing at a worktree dir, be treated as a real worktree).
#[test]
fn resolve_rejects_dot_id() {
    let env = Env::new();
    for bad in [".", ".."] {
        let out = env.cowt().args(["diff", bad]).output().unwrap();
        assert!(!out.status.success(), "diff {bad:?} must be refused");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("invalid worktree id or name"),
            "stderr for {bad:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// Round-22: clap parse-boundary exit codes (regression lock — these were
/// verified by audit but had zero test coverage).
#[test]
fn clap_parse_errors_exit_2_and_help_exits_0() {
    let env = Env::new();
    // Parse errors: clap's convention is exit code 2.
    let cases: &[&[&str]] = &[
        &[],                           // no subcommand
        &["frobnicate"],               // unknown subcommand
        &["diff", "--frobnicate"],     // unknown flag
        &["diff", "demo", "extra"],    // extra positional
        &["run"],                      // missing required cmd
        &["run", "demo"],              // `--` + cmd missing
        &["diff", "--json", "--json"], // duplicate flag
    ];
    for args in cases {
        let out = env.cowt().args(*args).output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(2),
            "args {args:?} must exit 2, stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    // Help/version: exit 0.
    for args in [&["--help"][..], &["--version"][..]] {
        let out = env.cowt().args(args).output().unwrap();
        assert!(
            out.status.success(),
            "args {args:?} must exit 0: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// Round-22: a child killed by a signal must be reported as such (not as a
/// bogus "exited with code 1"); the run command still exits non-zero.
#[cfg(unix)]
#[test]
fn run_reports_signal_killed_child() {
    let env = Env::new();
    if !env.fuse_available() {
        eprintln!("backend unavailable; skipping");
        return;
    }
    env.fork();
    // Child kills itself with SIGKILL — no exit code, only a signal.
    let mut cowt = env.cowt();
    cowt.args(["run", "demo", "--", "sh", "-c", "kill -9 $$"]);
    let out = cowt.output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("killed by signal"),
        "signal death must be reported, stderr: {err}"
    );
    assert_ne!(
        out.status.code(),
        Some(0),
        "signal death must exit non-zero"
    );
}

// ---------------------------------------------------------------- R23

/// Round-23: apply must refuse a semantically-corrupted base manifest —
/// entries wiped (or from another tree) + a whiteout in upper means the
/// deletion intent would be silently dropped (0 ops, rc=0), then upper
/// cleared and baseline advanced, destroying the only record of the intent.
#[test]
fn apply_refuses_corrupted_base_with_whiteout() {
    let env = Env::new();
    env.fork();
    // Corrupt the base manifest: wipe entries (valid JSON, wrong semantics).
    let dir = env.state_dir();
    let manifest = dir.join("manifest.json");
    let v: serde_json::Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    let mut v = v;
    v["entries"] = serde_json::json!({});
    fs::write(&manifest, serde_json::to_string_pretty(&v).unwrap()).unwrap();
    // Plant a deletion marker for a file that IS on the host but NOT in base.
    fs::create_dir_all(dir.join("upper")).unwrap();
    fs::write(dir.join("upper/.wh.config.txt"), b"").unwrap();

    let out = env.cowt().args(["apply", "demo"]).output().unwrap();
    assert!(
        !out.status.success(),
        "apply must refuse corrupted base (was: rc=0 '0 written, 0 deleted')"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("corrupt") || err.contains("base"),
        "refusal must explain the base corruption: {err}"
    );
    // The upper layer must NOT have been cleared (intent preserved).
    assert!(
        dir.join("upper/.wh.config.txt").exists(),
        "upper must survive a refused apply"
    );
}

/// Round-23: drop must be able to clean up a worktree whose meta.json is
/// corrupt or missing (half-created fork) — currently it is blocked even
/// with --force, and list silently hides the directory.
#[test]
fn drop_recovers_corrupt_meta() {
    let env = Env::new();
    env.fork();
    let id = env
        .state_dir()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let dir = env.state_dir();

    // Corrupt meta.json -> list must NOT silently hide it, drop --force
    // must still remove the directory (by id; the name lives in the corrupt
    // meta.json and is unrecoverable).
    fs::write(dir.join("meta.json"), "{\"id\":").unwrap();
    let out = env.cowt().args(["list"]).output().unwrap();
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("unreadable meta.json"),
        "list must warn about the corrupt worktree, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = env.cowt().args(["drop", &id]).output().unwrap();
    assert!(
        !out.status.success(),
        "drop without --force must refuse damaged meta"
    );
    let out = env.cowt().args(["drop", &id, "--force"]).output().unwrap();
    assert!(
        out.status.success(),
        "drop --force must clean damaged meta: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!dir.exists(), "damaged worktree dir must be removed");
}

/// Round-23: recovery paths — missing manifest / missing upper / garbage
/// run.pid must not block drop or break diff (regression lock).
#[test]
fn corrupted_state_recovery_paths() {
    let env = Env::new();
    env.fork();
    let dir = env.state_dir();
    let upper = dir.join("upper");

    // Garbage run.pid: treated as not-running (safe no-op).
    fs::write(dir.join("run.pid"), "not-a-pid\n").unwrap();
    let out = env.cowt().args(["diff", "demo"]).output().unwrap();
    assert!(out.status.success(), "garbage run.pid must not break diff");

    // Missing upper: diff degrades (io error) but drop still works.
    fs::remove_dir_all(&upper).unwrap();
    fs::remove_file(dir.join("run.pid")).unwrap();
    let out = env.cowt().args(["diff", "demo"]).output().unwrap();
    assert!(
        !out.status.success(),
        "missing upper must be reported by diff"
    );
    let out = env
        .cowt()
        .args(["drop", "demo", "--force"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "drop must work with missing upper: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!dir.exists(), "worktree dir must be removed");
}
