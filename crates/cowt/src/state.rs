//! Worktree state storage.
//!
//! Every forked worktree owns one directory under the state root:
//!
//! ```text
//! $COWT_HOME/  (default ~/.local/state/cowt)
//! └── <id>/
//!     ├── meta.json       worktree metadata (target, status, backend)
//!     ├── manifest.json   base snapshot (metadata only, no file contents)
//!     ├── upper/          overlayfs upper layer (the isolated writes)
//!     ├── work/           overlayfs work dir
//!     └── run.pid         pid of an in-flight `cowt run` (present only then)
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use cowt_core::Manifest;
use serde::{Deserialize, Serialize};

/// Lifecycle status persisted in meta.json. "running" is derived from the
/// pidfile at runtime, never stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    #[default]
    Ready,
    Applied,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeMeta {
    #[serde(default)]
    pub id: String,
    // `Option` is NOT optional in serde — a missing field errors unless
    // defaulted. All fields carry `#[serde(default)]` so adding one never
    // makes existing meta.json files unreadable (round-34).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub target: PathBuf,
    #[serde(default)]
    pub created_epoch: u64,
    #[serde(default)]
    pub status: Status,
    #[serde(default)]
    pub backend: String,
}

pub struct State {
    root: PathBuf,
}

impl State {
    /// State root: `$COWT_HOME` if set, else a per-user default
    /// (`~/.local/state/cowt` on unix, `%LOCALAPPDATA%\cowt` on Windows).
    pub fn open() -> Result<Self> {
        let root = match std::env::var_os("COWT_HOME") {
            // Round-37: `COWT_HOME=""` is a misconfiguration (services/CI
            // commonly export empty vars) — treat it as unset AND say so,
            // instead of letting PathBuf::from("") fail with a bare
            // "create state root : os error 3".
            Some(p) if p.is_empty() => {
                let home = home_dir().context("HOME is not set and COWT_HOME was not provided")?;
                eprintln!("cowt: warning: COWT_HOME is set but empty; using the default state dir");
                default_state_dir(&home)
            }
            Some(p) => PathBuf::from(p),
            None => {
                let home = home_dir().context("HOME is not set and COWT_HOME was not provided")?;
                default_state_dir(&home)
            }
        };
        fs::create_dir_all(&root)
            .with_context(|| format!("create state root {}", root.display()))?;
        // The state dir holds the moved-aside host directory and the upper
        // layer (isolated writes). Lock it to the owner: a world-readable
        // state root would let other local users read the isolation layer
        // and silently misread an unreadable upper as "no changes".
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&root, fs::Permissions::from_mode(0o700));
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    /// Create a new worktree state directory. Fails if the id already exists
    /// (atomically, via exclusive create) or a worktree with the same name
    /// already exists.
    pub fn create(&self, meta: &WorktreeMeta, manifest: &Manifest) -> Result<PathBuf> {
        let dir = self.dir(&meta.id);
        // Exclusive create: closes the check-then-create TOCTOU between two
        // concurrent forks (a hash collision would otherwise silently
        // overwrite the first worktree's state).
        match fs::create_dir(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                bail!("worktree '{}' already exists", meta.id)
            }
            Err(e) => {
                return Err(anyhow::Error::from(e))
                    .with_context(|| format!("create state dir {}", dir.display()))
            }
        }
        if let Some(name) = &meta.name {
            if !valid_id_or_name(name) {
                let _ = fs::remove_dir(&dir); // roll back the exclusive dir
                bail!("invalid worktree name '{name}' (must be a single path component)");
            }
            for other in self.list()? {
                // A name must not shadow an existing worktree's id either:
                // resolve() prefers the id-direct lookup, so `fork --name
                // <existing-id>` would create a permanently unreachable
                // worktree, and `drop <that name>` would hit the wrong one.
                if other.name.as_deref() == Some(name.as_str()) || other.id == *name {
                    let _ = fs::remove_dir(&dir); // roll back the exclusive dir
                    bail!("a worktree named '{name}' already exists; pick a different --name");
                }
            }
        }
        let result = (|| -> Result<()> {
            fs::create_dir_all(dir.join("upper")).context("create upper layer")?;
            fs::create_dir_all(dir.join("work")).context("create overlay work dir")?;
            let manifest_json =
                serde_json::to_string_pretty(manifest).context("serialize base manifest")?;
            atomic_write(&dir.join("manifest.json"), manifest_json.as_bytes())
                .context("write manifest.json")?;
            Self::write_meta(&dir, meta)?;
            Ok(())
        })();
        // Round-35: re-check the name AFTER meta.json is written. The
        // pre-check above runs while a concurrent same-name fork may not
        // have written its meta.json yet (list() skips those dirs), so two
        // forks could both pass. The post-check sees both and rolls back
        // the loser.
        if result.is_ok() {
            if let Some(name) = &meta.name {
                let dups: Vec<String> = self
                    .list()?
                    .into_iter()
                    .filter(|other| {
                        other.id != meta.id && other.name.as_deref() == Some(name.as_str())
                    })
                    .map(|other| other.id)
                    .collect();
                if !dups.is_empty() {
                    let _ = fs::remove_dir_all(&dir); // roll back
                    bail!(
                        "a worktree named '{name}' already exists (id {}); \
                         pick a different --name",
                        dups.join(", ")
                    );
                }
            }
        }
        if result.is_err() {
            let _ = fs::remove_dir_all(&dir); // roll back a half-created state
        }
        result.map(|()| dir)
    }
    pub fn write_meta(dir: &Path, meta: &WorktreeMeta) -> Result<()> {
        let json = serde_json::to_string_pretty(meta).context("serialize meta")?;
        atomic_write(&dir.join("meta.json"), json.as_bytes()).context("write meta.json")
    }

    /// Resolve an id-or-name to a worktree directory.
    ///
    /// Ids/names are single path components: anything with separators or
    /// parent-dir components cannot be a worktree and would resolve outside
    /// the state root (a hostile or mistyped id like `../victim` would let
    /// `drop` delete an arbitrary directory).
    pub fn resolve(&self, id_or_name: &str) -> Result<PathBuf> {
        if !valid_id_or_name(id_or_name) {
            bail!("invalid worktree id or name '{id_or_name}'");
        }
        // A `.trash-*` name is a drop leftover, never a live worktree —
        // direct id lookup must not resurrect it (round-33).
        if id_or_name.starts_with(".trash-") {
            bail!("invalid worktree id or name '{id_or_name}'");
        }
        let direct = self.dir(id_or_name);
        // A worktree-shaped directory (meta.json present, or a half-created
        // fork: manifest.json already written) resolves by id even when
        // meta.json is missing/corrupt — commands decide how to handle the
        // damage (drop --force can discard it; round-23).
        if direct.join("meta.json").exists()
            || (direct.is_dir() && direct.join("manifest.json").is_file())
        {
            return Ok(direct);
        }
        // An EARLIER half-created fork (kill between create_dir and the
        // manifest write) leaves only upper/work — still a cowt dir that
        // must be resolvable so `drop --force` can clean it up (round-31).
        if direct.is_dir() && (direct.join("upper").is_dir() || direct.join("work").is_dir()) {
            return Ok(direct);
        }
        // Try name lookup.
        let mut hits = Vec::new();
        for meta in self.list()? {
            if meta.name.as_deref() == Some(id_or_name) {
                hits.push(meta.id);
            }
        }
        match hits.len() {
            0 => bail!("no worktree named or identified by '{id_or_name}'"),
            1 => Ok(self.dir(&hits[0])),
            _ => bail!("name '{id_or_name}' is ambiguous: {}", hits.join(", ")),
        }
    }

    pub fn load_meta(dir: &Path) -> Result<WorktreeMeta> {
        let s = fs::read_to_string(dir.join("meta.json"))
            .with_context(|| format!("read {}", dir.display()))?;
        serde_json::from_str(&s).with_context(|| {
            // Round-37: the by-id path (status/diff/apply <id>) used to get
            // a bare "parse meta.json" with no way to know WHICH worktree
            // or what to do — the by-name path already got a drop hint from
            // list()'s warning.
            format!(
                "parse meta.json for {} (worktree {}); use `cowt drop {} --force` to discard the damaged worktree",
                dir.display(),
                dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            )
        })
    }

    pub fn load_manifest(dir: &Path) -> Result<Manifest> {
        let s = fs::read_to_string(dir.join("manifest.json"))
            .with_context(|| format!("read {}", dir.display()))?;
        Manifest::from_json(&s).context("parse manifest.json")
    }

    /// Atomically replace the base manifest (apply advances the baseline to
    /// the merged host state so the next run/diff/apply iterates against it).
    pub fn write_manifest(dir: &Path, manifest: &Manifest) -> Result<()> {
        let json = serde_json::to_string_pretty(manifest).context("serialize manifest")?;
        atomic_write(&dir.join("manifest.json"), json.as_bytes()).context("write manifest.json")
    }

    pub fn list(&self) -> Result<Vec<WorktreeMeta>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.root).with_context(|| "read state root")? {
            let entry = entry?;
            let dir = entry.path();
            // A `.trash-*` rename-aside from a failed `drop` is not a
            // worktree; hide it from list/resolve so no ghost entries.
            if entry.file_name().to_string_lossy().starts_with(".trash-") {
                continue;
            }
            if !dir.is_dir() || !dir.join("meta.json").exists() {
                // Round-37: a worktree-shaped dir with NO meta.json (fork
                // killed between create_dir and write_meta, or an external
                // delete of meta.json) is invisible to every command —
                // isolated data in its upper would strand forever with no
                // warning. Surface it (resolve still reaches it by id for
                // `drop --force`).
                if dir.is_dir()
                    && (dir.join("manifest.json").exists() || dir.join("upper").is_dir())
                {
                    eprintln!(
                        "cowt: warning: {} has no meta.json (interrupted fork or missing file); \
                         its isolated data is not listed. Use `cowt drop {} --force` to clean it up",
                        dir.display(),
                        entry.file_name().to_string_lossy()
                    );
                }
                continue;
            }
            match Self::load_meta(&dir) {
                Ok(meta) => out.push(meta),
                // Round-23: a corrupt meta.json must not be silently hidden
                // — the directory still exists and blocks drop; surface it.
                Err(e) => eprintln!(
                    "cowt: warning: unreadable meta.json in {} ({e}); use `cowt drop {} --force` to discard",
                    dir.display(),
                    entry.file_name().to_string_lossy()
                ),
            }
        }
        // Deterministic order: created_epoch first, id as tie-break (the
        // epoch is second-granular, so same-second worktrees otherwise fall
        // back to filesystem read_dir order — round-33).
        out.sort_by(|a, b| (a.created_epoch, &a.id).cmp(&(b.created_epoch, &b.id)));
        Ok(out)
    }

    /// Pid of the running process for this worktree, if alive AND (when the
    /// pidfile carries a starttime) still the same process — a recycled pid
    /// (crash residue whose pid was reused by an unrelated process) is NOT
    /// reported, so `drop --force` never kills an innocent process.
    pub fn running_pid(dir: &Path) -> Option<u32> {
        let s = fs::read_to_string(dir.join("run.pid")).ok()?;
        let s = s.trim();
        let (pid, expected_start) = match s.split_once(':') {
            Some((p, st)) => (p.parse::<u32>().ok()?, Some(st.parse::<u128>().ok()?)),
            None => (s.parse::<u32>().ok()?, None),
        };
        if !pid_alive(pid) {
            return None;
        }
        if let Some(expected) = expected_start {
            if crate::backend::process_starttime(pid) != Some(expected) {
                return None; // pid reused by an unrelated process
            }
        }
        Some(pid)
    }

    /// True when a previous `cowt run` left its pidfile behind but the
    /// process is gone — i.e. the run crashed or was killed. This is the
    /// discriminator that makes stale-mount cleanup safe: the mount at the
    /// target (if any) can only be our own leftover.
    ///
    /// An EMPTY or unparseable pidfile (kill -9 between create and write)
    /// is NOT considered ours: tearing down a mount on that evidence could
    /// unmount a foreign filesystem (round-28). Only a well-formed pidfile
    /// whose pid is verifiably dead is a provable cowt leftover.
    pub fn stale_run(dir: &Path) -> bool {
        let s = match fs::read_to_string(dir.join("run.pid")) {
            Ok(s) => s,
            Err(_) => return false,
        };
        let t = s.trim();
        // `pid:starttime` (round-28 format): a DEAD pid whose number was
        // recycled by an unrelated process must NOT look alive — stale_run
        // would return false and drop --force would misjudge our own
        // leftover mount as foreign (round-37 CI: pid reuse within the
        // drop-vs-teardown race window). Verify starttime when present,
        // exactly like running_pid does.
        let (pid, expected_start) = match t.split_once(':') {
            Some((p, st)) => (p.parse::<u32>().ok(), st.parse::<u128>().ok()),
            None => (t.parse::<u32>().ok(), None),
        };
        match pid {
            Some(p) => {
                if !pid_alive(p) {
                    return true; // verifiably dead
                }
                if let Some(expected) = expected_start {
                    // Alive but with a different starttime: recycled pid —
                    // our process is gone, the number was reused. The
                    // pidfile records OUR run, so it is stale regardless of
                    // what the recycled process is doing.
                    return crate::backend::process_starttime(p) != Some(expected);
                }
                false
            }
            None => false, // empty/garbage: ownership unknown, refuse
        }
    }

    /// Remove the pidfile, but ONLY if it still records `expected_pid` —
    /// a concurrently started run may have replaced it with its own pid,
    /// and deleting that would leave the successor unowned (round-28).
    #[allow(dead_code)] // used by tests; run.rs uses the running_pid guard
    pub fn clear_running_if_owned(dir: &Path, expected_pid: u32) {
        let owned = fs::read_to_string(dir.join("run.pid"))
            .ok()
            .map(|s| {
                s.trim()
                    .split(':')
                    .next()
                    .and_then(|p| p.parse::<u32>().ok())
                    == Some(expected_pid)
            })
            .unwrap_or(false);
        if owned {
            let _ = fs::remove_file(dir.join("run.pid"));
        }
    }

    pub fn clear_running(dir: &Path) {
        let _ = fs::remove_file(dir.join("run.pid"));
    }
}

/// Terminal-safe rendering: control bytes (other than tab/CR/LF) become
/// U+FFFD so hostile file names or contents cannot inject ANSI/OSC
/// sequences via cowt's human output. Also strips Unicode format
/// characters (bidi overrides, zero-width spaces — `is_control()` does not
/// cover the Cf category) that can visually spoof filenames (round-29).
pub fn sanitize_display(s: &str) -> String {
    s.chars()
        .map(|c| {
            let format_char = matches!(
                c as u32,
                // bidi controls
                0x202A..=0x202E
                // zero-width / joiners
                | 0x200B..=0x200F
                // other format controls
                | 0x2060..=0x206F
                // BOM / word joiner
                | 0xFEFF
            );
            if (c.is_control() && !matches!(c, '\t' | '\n' | '\r')) || format_char {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect()
}

/// Shared id/name rule used by both `resolve()` (lookup side) and `create()`
/// (creation side), so the tool never creates a worktree it cannot resolve:
/// a single path component, no separators, no `.`/`..` components.
pub fn valid_id_or_name(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && !s.contains('/')
        && !s.contains('\\')
        && !s.contains("..")
        // `.trash-` is the drop-leftover namespace; creating a worktree
        // with that name would be unaddressable by name (resolve refuses
        // it — round-33/35).
        && !s.starts_with(".trash-")
        // Control characters (newline/tab/ESC...) would break the
        // one-worktree-per-line output contract and allow terminal
        // injection through a user-chosen label (round-33).
        && !s.chars().any(|c| c.is_control())
}

/// Atomic file write: temp file + rename (same filesystem). A kill -9
/// mid-write leaves the old file intact instead of a truncated JSON that
/// would brick the worktree (drop included). The tmp name is
/// process-unique so two concurrent writers never truncate each other's
/// temp file (round-28).
fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!(
        "json.tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    // Sweep stale tmp files from previous crashed writers before writing a
    // fresh one (round-36): a kill -9 between write and rename leaves
    // *.json.tmp-<pid>-<nanos> behind forever otherwise. Only sweep the
    // current directory's own json.tmp-* pattern, and never a live
    // concurrent writer's file (pid-based liveness check; ours is always
    // the just-written one).
    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(idx) = name.find("json.tmp-") else {
                continue;
            };
            let Some(pid) = name[idx + "json.tmp-".len()..]
                .split('-')
                .next()
                .and_then(|p| p.parse::<u32>().ok())
            else {
                continue;
            };
            if pid == std::process::id() {
                continue; // ours (or a concurrent writer from this process — none)
            }
            if pid_alive(pid) {
                continue; // live concurrent writer
            }
            let _ = std::fs::remove_file(dir.join(&name));
        }
    }
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Sweep stale `.cowt-apply-<pid>-<nanos>` staging dirs from a crashed
/// `cowt apply` in `parent` (round-36). Only dirs whose pid is dead are
/// removed — a live concurrent apply keeps its staging area.
pub fn sweep_stale_staging(parent: &std::path::Path) {
    let Ok(rd) = std::fs::read_dir(parent) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix(".cowt-apply-") else {
            continue;
        };
        let Some(pid) = rest.split('-').next().and_then(|p| p.parse::<u32>().ok()) else {
            continue;
        };
        if pid_alive(pid) {
            continue; // live concurrent apply
        }
        if e.path().is_dir() {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Generate a short random id (16 hex chars = 64 bits) from /dev/urandom,
/// falling back to time+pid when it is unavailable (e.g. Windows). A
/// collision is detected by the exclusive state-dir create and reported —
/// no retry, acceptable at 2^-64 for human-scale fork rates (round-35).
pub fn short_id() -> String {
    let mut seed = [0u8; 8];
    let filled = fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut seed))
        .is_ok();
    if !filled {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        seed = (nanos as u64 ^ (std::process::id() as u64) << 32).to_le_bytes();
    }
    hex(&seed)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// User home directory: `$HOME`, falling back to `%USERPROFILE%` on Windows
/// (where `HOME` is commonly unset).
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).or({
        #[cfg(windows)]
        {
            std::env::var_os("USERPROFILE").map(PathBuf::from)
        }
        #[cfg(not(windows))]
        {
            None
        }
    })
}

/// Strip the `\\?\` verbatim prefix. `std::fs::canonicalize` on Windows
/// returns extended-length paths; mixing those with 8.3 short names from the
/// environment (TMP etc.) breaks rename/access consistency.
#[cfg(windows)]
pub fn dos_path(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => p.to_path_buf(),
    }
}

/// Platform default state dir below `home`.
#[cfg(not(windows))]
fn default_state_dir(home: &Path) -> PathBuf {
    home.join(".local/state/cowt")
}

#[cfg(windows)]
fn default_state_dir(home: &Path) -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".cowt"))
        .join("cowt")
}

/// Portable pid liveness probe: `kill -0` on unix (works on macOS too, where
/// there is no /proc), `OpenProcess` on Windows.
#[cfg(unix)]
pub(crate) fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(windows)]
pub(crate) fn pid_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER};
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) };
    match handle {
        Ok(h) => {
            let _ = unsafe { CloseHandle(h) };
            true
        }
        Err(e) => {
            // Only ERROR_INVALID_PARAMETER means "no such pid". Any other
            // failure (e.g. ERROR_ACCESS_DENIED on a protected process)
            // means the process exists — treat as alive.
            e.code() != ERROR_INVALID_PARAMETER.into()
        }
    }
}

/// Derive a worktree name slug from the target path, e.g. `/home/u/.config/code` -> `config-code`.
pub fn default_name(target: &Path) -> String {
    let parts: Vec<String> = target
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => {
                s.to_str().map(|s| s.trim_start_matches('.').to_string())
            }
            _ => None,
        })
        .filter(|s| !s.is_empty())
        .collect();
    let tail: Vec<String> = parts.into_iter().rev().take(2).collect();
    let slug = tail.into_iter().rev().collect::<Vec<_>>().join("-");
    if slug.is_empty() {
        "worktree".into()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_alive_known_and_unknown() {
        // Our own process is always alive.
        assert!(pid_alive(std::process::id()));
        #[cfg(unix)]
        {
            // u32::MAX as i32 is -1, which kill(-1, 0) treats as a broadcast
            // to every process (reports alive). Use a really killed pid.
            let mut child = std::process::Command::new("sh")
                .arg("-c")
                .arg("sleep 30")
                .spawn()
                .unwrap();
            let pid = child.id();
            let _ = std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .status();
            let _ = child.wait();
            assert!(!pid_alive(pid));
        }
        #[cfg(windows)]
        {
            // u32::MAX is simply an invalid pid on Windows (no broadcast).
            assert!(!pid_alive(u32::MAX));
        }
    }

    #[test]
    fn running_pid_rejects_recycled_pid() {
        // A pidfile carrying a bogus starttime must NOT report our own live
        // pid as running (that would let `drop --force` kill an innocent
        // process after a pid reuse).
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("run.pid"),
            format!("{}:1", std::process::id()),
        )
        .unwrap();
        assert!(State::running_pid(tmp.path()).is_none());
        // Legacy plain-pid format still works.
        fs::write(tmp.path().join("run.pid"), std::process::id().to_string()).unwrap();
        assert_eq!(State::running_pid(tmp.path()), Some(std::process::id()));
    }

    #[test]
    fn default_name_slug() {
        assert_eq!(
            default_name(Path::new("/home/u/.config/Code")),
            "config-Code"
        );
        assert_eq!(default_name(Path::new("/")), "worktree");
    }

    /// Round-28: an empty or unparseable pidfile is NOT a provable cowt
    /// leftover — stale_run must refuse it (a crash between create and
    /// write must never authorize unmounting a possibly-foreign mount).
    #[test]
    fn stale_run_refuses_unparseable_pidfile() {
        let tmp = tempfile::tempdir().unwrap();
        // Empty file (kill -9 between create_new and write_all).
        fs::write(tmp.path().join("run.pid"), b"").unwrap();
        assert!(
            !State::stale_run(tmp.path()),
            "empty pidfile must not be treated as our stale run"
        );
        // Garbage content.
        fs::write(tmp.path().join("run.pid"), b"not-a-pid\n").unwrap();
        assert!(!State::stale_run(tmp.path()));
        // A well-formed pidfile with a dead pid IS stale.
        fs::write(tmp.path().join("run.pid"), b"999999999\n").unwrap();
        assert!(State::stale_run(tmp.path()));
    }

    /// Round-28: clear_running_if_owned must not delete a pidfile that a
    /// successor run replaced with its own pid.
    #[test]
    fn clear_running_if_owned_respects_ownership() {
        let tmp = tempfile::tempdir().unwrap();
        // Our pidfile records a live pid (our own process).
        fs::write(tmp.path().join("run.pid"), std::process::id().to_string()).unwrap();
        // A successor replaced it with a different live pid (simulated).
        State::clear_running_if_owned(tmp.path(), 12345);
        assert!(
            tmp.path().join("run.pid").exists(),
            "foreign pidfile must survive clear_running_if_owned"
        );
        // Ours matches -> removed.
        State::clear_running_if_owned(tmp.path(), std::process::id());
        assert!(!tmp.path().join("run.pid").exists());
    }

    /// Round-37: a dead run's pid REUSED by an unrelated live process must
    /// still read as stale (starttime mismatch) — otherwise drop --force
    /// misjudges our own leftover mount as foreign during the teardown
    /// race, and the recycled process could not possibly be our child
    /// (starttime differs).
    #[test]
    fn stale_run_detects_recycled_pid() {
        let tmp = tempfile::tempdir().unwrap();
        // Our own pid with a BOGUS starttime simulates the recycled case:
        // the pid is alive but is not the process we recorded.
        fs::write(
            tmp.path().join("run.pid"),
            format!("{}:1", std::process::id()),
        )
        .unwrap();
        assert!(
            State::stale_run(tmp.path()),
            "alive pid with mismatched starttime is a recycled pid — must be stale"
        );
        // Same pid, CORRECT starttime: genuinely running — not stale.
        let real = crate::backend::process_starttime(std::process::id()).unwrap_or(1);
        fs::write(
            tmp.path().join("run.pid"),
            format!("{}:{real}", std::process::id()),
        )
        .unwrap();
        assert!(
            !State::stale_run(tmp.path()),
            "alive pid with matching starttime is running — not stale"
        );
    }

    /// Round-34: the v1 minimal meta.json contract lock — a meta with only
    /// the original fields must parse; missing optional `name` yields None
    /// (serde's Option-without-default trap is locked so future fields
    /// follow the #[serde(default)] pattern).
    #[test]
    fn v1_minimal_meta_contract_lock() {
        let minimal = r#"{"id":"abc","name":"demo","target":"/x","created_epoch":0,"status":"ready","backend":"test"}"#;
        let m: WorktreeMeta = serde_json::from_str(minimal).unwrap();
        assert_eq!(m.id, "abc");
        assert_eq!(m.status, Status::Ready);
        // Missing name (Option without default previously errored).
        let noname =
            r#"{"id":"abc","target":"/x","created_epoch":0,"status":"ready","backend":"test"}"#;
        let m: WorktreeMeta = serde_json::from_str(noname).unwrap();
        assert_eq!(m.name, None);
    }

    /// Round-36: atomic_write sweeps stale `*.json.tmp-<pid>-<nanos>`
    /// residue from crashed writers (kill -9 between write and rename),
    /// but never a live process's temp file.
    #[test]
    fn atomic_write_sweeps_stale_tmp_but_keeps_live() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("manifest.json");
        // Stale residue (dead pid 999999999) and a "live" residue (our own
        // pid — treated as live concurrent writer, kept).
        fs::write(tmp.path().join("manifest.json.tmp-999999999-1"), b"stale").unwrap();
        fs::write(
            tmp.path()
                .join(format!("manifest.json.tmp-{}-1", std::process::id())),
            b"live",
        )
        .unwrap();
        atomic_write(&path, b"fresh").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"fresh");
        assert!(
            !tmp.path().join("manifest.json.tmp-999999999-1").exists(),
            "stale residue must be swept"
        );
        assert!(
            tmp.path()
                .join(format!("manifest.json.tmp-{}-1", std::process::id()))
                .exists(),
            "live residue must be kept"
        );
    }

    /// Round-36: sweep_stale_staging removes `.cowt-apply-*` dirs whose pid
    /// is dead, and keeps live ones (a concurrent apply's staging area).
    #[test]
    fn sweep_stale_staging_removes_dead_keeps_live() {
        let tmp = tempfile::tempdir().unwrap();
        let dead = tmp.path().join(".cowt-apply-999999999-1");
        let live = tmp
            .path()
            .join(format!(".cowt-apply-{}-1", std::process::id()));
        let other = tmp.path().join("real-user-dir");
        for d in [&dead, &live, &other] {
            fs::create_dir_all(d).unwrap();
        }
        sweep_stale_staging(tmp.path());
        assert!(!dead.exists(), "dead staging dir must be swept");
        assert!(live.exists(), "live staging dir must be kept");
        assert!(other.exists(), "non-staging dirs must never be touched");
    }
}
