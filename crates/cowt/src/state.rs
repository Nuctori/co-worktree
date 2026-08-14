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
    /// State root: `$COWT_HOME` if set, else `~/.local/state/cowt`.
    pub fn open() -> Result<Self> {
        let root = match std::env::var_os("COWT_HOME") {
            Some(p) => PathBuf::from(p),
            None => {
                let home = std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .context("HOME is not set and COWT_HOME was not provided")?;
                home.join(".local/state/cowt")
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
            if let Ok(meta) = Self::load_meta(&dir) {
                out.push(meta);
            }
        }
        out.sort_by_key(|a| a.created_epoch);
        Ok(out)
    }

    /// Pid of the running process for this worktree, if alive.
    pub fn running_pid(dir: &Path) -> Option<u32> {
        let s = fs::read_to_string(dir.join("run.pid")).ok()?;
        let pid: u32 = s.trim().parse().ok()?;
        if Path::new(&format!("/proc/{pid}")).exists() {
            Some(pid)
        } else {
            None
        }
    }

    pub fn set_running(dir: &Path, pid: u32) -> Result<()> {
        fs::write(dir.join("run.pid"), pid.to_string()).context("write run.pid")
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
