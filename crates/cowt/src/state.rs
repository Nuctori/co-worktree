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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ready,
    Applied,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeMeta {
    pub id: String,
    pub name: Option<String>,
    pub target: PathBuf,
    pub created_epoch: u64,
    pub status: Status,
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
            Some(p) => PathBuf::from(p),
            None => {
                let home = home_dir().context("HOME is not set and COWT_HOME was not provided")?;
                default_state_dir(&home)
            }
        };
        fs::create_dir_all(&root)
            .with_context(|| format!("create state root {}", root.display()))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    /// Create a new worktree state directory. Fails if the id already exists.
    pub fn create(&self, meta: &WorktreeMeta, manifest: &Manifest) -> Result<PathBuf> {
        let dir = self.dir(&meta.id);
        if dir.exists() {
            bail!("worktree '{}' already exists", meta.id);
        }
        fs::create_dir_all(dir.join("upper")).context("create upper layer")?;
        fs::create_dir_all(dir.join("work")).context("create overlay work dir")?;
        let manifest_json =
            serde_json::to_string_pretty(manifest).context("serialize base manifest")?;
        fs::write(dir.join("manifest.json"), manifest_json).context("write manifest.json")?;
        Self::write_meta(&dir, meta)?;
        Ok(dir)
    }

    pub fn write_meta(dir: &Path, meta: &WorktreeMeta) -> Result<()> {
        let json = serde_json::to_string_pretty(meta).context("serialize meta")?;
        fs::write(dir.join("meta.json"), json).context("write meta.json")
    }

    /// Resolve an id-or-name to a worktree directory.
    pub fn resolve(&self, id_or_name: &str) -> Result<PathBuf> {
        let direct = self.dir(id_or_name);
        if direct.join("meta.json").exists() {
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
        serde_json::from_str(&s).context("parse meta.json")
    }

    pub fn load_manifest(dir: &Path) -> Result<Manifest> {
        let s = fs::read_to_string(dir.join("manifest.json"))
            .with_context(|| format!("read {}", dir.display()))?;
        Manifest::from_json(&s).context("parse manifest.json")
    }

    pub fn list(&self) -> Result<Vec<WorktreeMeta>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.root).with_context(|| "read state root")? {
            let entry = entry?;
            let dir = entry.path();
            if !dir.is_dir() || !dir.join("meta.json").exists() {
                continue;
            }
            // A `.trash-*` rename-aside from a failed `drop` is not a
            // worktree; hide it from list/resolve so no ghost entries.
            if entry.file_name().to_string_lossy().starts_with(".trash-") {
                continue;
            }
            if let Ok(meta) = Self::load_meta(&dir) {
                out.push(meta);
            }
        }
        out.sort_by_key(|a| a.created_epoch);
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
    pub fn stale_run(dir: &Path) -> bool {
        dir.join("run.pid").is_file() && Self::running_pid(dir).is_none()
    }

    pub fn clear_running(dir: &Path) {
        let _ = fs::remove_file(dir.join("run.pid"));
    }
}

/// Generate a short random id (8 hex chars) from /dev/urandom, time and pid.
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
}
