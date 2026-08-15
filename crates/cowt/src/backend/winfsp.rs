//! Windows backend: a WinFsp user-mode filesystem with copy-on-write.
//!
//! WinFsp (a signed kernel driver, installed via the official installer or
//! `choco install winfsp`) lets a user-mode process host a real filesystem.
//! This backend implements a passthrough-with-copy-up FS:
//!
//! ```text
//! target ──▶ WinFsp mount at state/<id>/view
//!            ├── lower ─▶ state/<id>/real   (host dir, moved aside)
//!            └── upper ─▶ state/<id>/upper  (isolated writes)
//! ```
//!
//! While a worktree runs, the host directory is *moved aside* to `real` and
//! WinFsp mounts directly onto the original path (junction chaining was
//! abandoned: WinFsp does not resolve a mounted view through a junction
//! reparse chain). Reads pass through to `real`; writes copy files up into
//! `upper` first; deletions of lower-only files leave `.wh.<name>` whiteouts
//! (the same encoding cowt-core parses on Linux).
//! ```
//!
//! While a worktree runs, the host directory is *moved aside* to `real` and
//! the original path becomes a junction to the mounted view (Windows has no
//! bind mounts). Reads pass through to `real`; writes copy files up into
//! `upper` first; deletions of lower-only files leave `.wh.<name>` whiteouts
//! (the same encoding cowt-core parses on Linux).
//!
//! Windows-specific caveats (documented in the README):
//!   * the WinFsp DLL must be installed (`cowt doctor` reports it);
//!   * junctions need no privileges, so no admin is required;
//!   * user-mode I/O makes the write path slower than kernel backends;
//!   * a hard-killed `cowt run` leaves the junction + `real` behind —
//!     the next `cowt run` or `cowt drop --force` restores it.

use std::ffi::c_void;
use std::fs;
use std::io::{self, ErrorKind};
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{bail, Context, Result};
use winfsp::filesystem::{
    DirInfo, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo, VolumeInfo, WideNameInfo,
};
use winfsp::host::{FileSystemHost, FileSystemParams, OperationGuardStrategy, VolumeParams};
use winfsp::{FspError, Result as FspResult, U16CStr};

use super::{Backend, MountGuard};

/// The concrete host type, exported so `backend::mod` can store it in
/// `MountGuard` without naming private imports.
pub type CowtHost = FileSystemHost<CowFs>;

// --- NT constants (stable SDK values) ---
const FILE_GENERIC_WRITE: u32 = 0x0012_0116;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
/// WinFsp cleanup flag: the handle was opened for deletion.
const FSP_CLEANUP_DELETE: u32 = 1;
const WHITEOUT_PREFIX: &str = ".wh.";

pub struct WinFspBackend;

/// The user context WinFsp hands back on every operation: an open file
/// handle (with its relative path, so delete-on-cleanup works even when the
/// kernel does not pass a name), or a directory path (re-listed on demand).
pub enum Handle {
    File {
        f: std::fs::File,
        rel: PathBuf,
        writable: bool,
    },
    Dir(PathBuf),
}

/// Paths derived from the worktree state dir (upper's parent): the moved-aside
/// host dir (`real`) and the (unused now) view dir.
struct Layout {
    real: PathBuf,
    #[allow(dead_code)]
    view: PathBuf,
}

impl Layout {
    fn from_upper(upper: &Path) -> Layout {
        let state = upper
            .parent()
            .expect("upper always sits directly in the state dir");
        Layout {
            real: state.join("real"),
            view: state.join("view"),
        }
    }
}

impl Backend for WinFspBackend {
    fn name(&self) -> &'static str {
        "winfsp"
    }

    fn available(&self) -> Result<()> {
        // The crate's winfsp_init only probes a DLL next to the binary; the
        // `system` feature (registry lookup) would fix that but breaks Linux
        // cross-compile builds, so the installer lookup is done here.
        if winfsp::winfsp_init().is_ok() {
            return Ok(());
        }
        if let Some(dll) = installed_winfsp_dll() {
            load_library(&dll);
            if winfsp::winfsp_init().is_ok() {
                return Ok(());
            }
        }
        bail!(
            "WinFsp is not installed or its DLL failed to load \
             (install WinFsp from https://winfsp.dev or `choco install winfsp`)"
        )
    }

    fn mount(
        &self,
        _lower: &Path,
        upper: &Path,
        _work: &Path,
        mountpoint: &Path,
    ) -> Result<MountGuard> {
        self.available()?;
        let layout = Layout::from_upper(upper);

        // Stale state from a hard-killed `cowt run`: the host dir sits in
        // `real`, and the target is either a dead WinFsp mount point or a
        // missing directory (the driver deletes the mountpoint on process
        // death). Restore first in both cases.
        if is_reparse(mountpoint) || layout.real.exists() {
            if layout.real.exists() {
                eprintln!(
                    "cowt: recovering stale mount state at {}",
                    mountpoint.display()
                );
                restore(mountpoint, &layout)?;
            } else {
                bail!(
                    "{} is a mount point but no moved-aside directory was found; \
                     refusing to touch a foreign mount",
                    mountpoint.display()
                );
            }
        }

        // Move the host dir aside, then mount WinFsp directly at the original
        // path (WinFsp creates the mountpoint directory itself — pre-creating
        // it fails with STATUS_OBJECT_NAME_COLLISION). The host dir now lives
        // at `real`, a plain path outside the mount, so the filesystem reads
        // it without recursion. On any later failure the dance is rolled back.

        fs::rename(mountpoint, &layout.real).map_err(|e| {
            if e.kind() == io::ErrorKind::CrossesDevices {
                anyhow::anyhow!(
                    "cannot move {} to {}: state dir is on a different volume. \
                     Put COWT_HOME on the same drive as the app directory \
                     (Windows cannot rename across volumes)",
                    mountpoint.display(),
                    layout.real.display()
                )
            } else {
                anyhow::Error::from(e).context(format!(
                    "move {} aside to {}",
                    mountpoint.display(),
                    layout.real.display()
                ))
            }
        })?;
        let result = (|| -> Result<MountGuard> {
            let mut vp = VolumeParams::new();
            vp.filesystem_name("cowt")
                .post_cleanup_when_modified_only(true)
                .unicode_on_disk(true)
                // No attribute caching: a deleted file must disappear from
                // GetFileAttributes immediately (the default 1s timeout made
                // e2e deletions visible only after a delay).
                .file_info_timeout(0);
            let mut host = FileSystemHost::new_with_options(
                FileSystemParams {
                    use_dir_info_by_name: false,
                    volume_params: vp,
                    guard_strategy: OperationGuardStrategy::Coarse,
                    debug_mode: Default::default(),
                },
                CowFs {
                    lower: layout.real.clone(),
                    upper: upper.to_path_buf(),
                },
            )
            .context("create WinFsp filesystem")?;
            // WinFsp's mount manager rejects \\?\ verbatim paths; the target
            // was canonicalized to extended form at fork time.
            let mount_dos = dos_path(mountpoint);
            host.mount(&mount_dos)
                .with_context(|| format!("mount WinFsp volume at {}", mountpoint.display()))?;
            host.start().context("start WinFsp dispatcher")?;
            eprintln!(
                "cowt: WinFsp mounted at {} (upper: {}, host dir moved to {})",
                mountpoint.display(),
                upper.display(),
                layout.real.display()
            );
            Ok(MountGuard::with_host(mountpoint.to_path_buf(), host))
        })();
        if result.is_err() {
            // Roll back: drop the mountpoint (if any) and move the host dir
            // back into place.
            let _ = cleanup_mountpoint(mountpoint);
            let _ = restore(mountpoint, &layout);
        }
        result
    }

    fn unmount(&self, mountpoint: &Path) -> Result<()> {
        // Idempotent: works from a fresh process (`cowt drop --force`) or from
        // the owning `cowt run` after its child exited. The WinFsp volume dies
        // with its host process; here we only clear the mountpoint residue and
        // move the host dir back.
        cleanup_mountpoint(mountpoint)?;
        if let Some(real) = find_real_for(mountpoint) {
            let layout = Layout {
                real,
                view: PathBuf::new(),
            };
            restore(mountpoint, &layout)?;
        }
        Ok(())
    }

    fn is_mounted(&self, mountpoint: &Path) -> bool {
        // A WinFsp mount dies with its host process and the driver deletes
        // the mountpoint directory — leaving the moved-aside host dir in
        // `real` as the only trace. Treat that as still "mounted" so stale
        // recovery (run/diff/apply/drop) restores it.
        is_reparse(mountpoint) || find_real_for(mountpoint).is_some()
    }
}

/// Restore the host directory: move `state/real` back to `mountpoint`.
/// Tolerates the case where a concurrent `cowt run` already did it.
fn restore(mountpoint: &Path, layout: &Layout) -> Result<()> {
    if !layout.real.exists() {
        return Ok(()); // already restored
    }
    if let Ok(m) = fs::symlink_metadata(mountpoint) {
        if m.is_dir() {
            // Empty leftover dir from a torn-down mount: drop it.
            let _ = fs::remove_dir(mountpoint);
        } else {
            bail!(
                "{} exists and is not a directory; refusing to restore",
                mountpoint.display()
            );
        }
    }
    fs::rename(&layout.real, mountpoint).with_context(|| {
        format!(
            "restore {} from {}",
            mountpoint.display(),
            layout.real.display()
        )
    })
}

/// Is `path` a reparse point (WinFsp mount point, junction or symlink)?
/// std reports all of these as symlinks.
fn is_reparse(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

/// Strip the `\\?\` verbatim prefix (WinFsp's mount manager rejects it).
fn dos_path(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => p.to_path_buf(),
    }
}

/// Remove a WinFsp mountpoint left behind (the driver deletes the directory
/// when the owning process dies; if it lingers as an empty dir, drop it).
/// Never touches a live mount (remove on a reparse point fails).
fn cleanup_mountpoint(mountpoint: &Path) -> Result<()> {
    if is_reparse(mountpoint) {
        bail!(
            "{} still has a live mount; refusing to remove it",
            mountpoint.display()
        );
    }
    if let Ok(m) = fs::symlink_metadata(mountpoint) {
        if m.is_dir() {
            let _ = fs::remove_dir(mountpoint);
        }
    }
    Ok(())
}

/// Locate the moved-aside host dir (`<state>/<id>/real`) for a mountpoint,
/// by matching `meta.json` targets. Used by `cowt drop --force`, where the
/// state dir is not otherwise reachable from the mountpoint (no junction
/// anymore — WinFsp mounts directly at the target).
fn find_real_for(mountpoint: &Path) -> Option<PathBuf> {
    let state = crate::state::State::open().ok()?;
    for meta in state.list().ok()? {
        if meta.target == mountpoint {
            let real = state.dir(&meta.id).join("real");
            if real.exists() {
                return Some(real);
            }
        }
    }
    None
}

/// Locate the WinFsp installer DLL: registry `InstallDir` first (mirroring
/// the winfsp crate's `system` feature — without the feature, so Linux
/// cross-compile builds keep working), then well-known install locations
/// (choco's WinFsp 2.x SxS layout may leave no registry key).
fn installed_winfsp_dll() -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};

    let arch = if cfg!(target_arch = "x86_64") {
        "winfsp-x64.dll"
    } else if cfg!(target_arch = "x86") {
        "winfsp-x86.dll"
    } else {
        "winfsp-a64.dll"
    };

    let mut candidates: Vec<PathBuf> = Vec::new();
    for subkey in [
        windows::core::w!("SOFTWARE\\WOW6432Node\\WinFsp"),
        windows::core::w!("SOFTWARE\\WinFsp"),
    ] {
        let mut buf = [0u16; 1024];
        let mut size = (buf.len() * 2) as u32;
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                subkey,
                windows::core::w!("InstallDir"),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut size),
            )
        };
        if status.is_ok() {
            let len = (size as usize) / 2;
            candidates.push(PathBuf::from(std::ffi::OsString::from_wide(&buf[..len])));
        }
    }
    for base in [
        "C:\\Program Files (x86)\\WinFsp",
        "C:\\Program Files\\WinFsp",
        "C:\\ProgramData\\WinFsp",
    ] {
        candidates.push(PathBuf::from(base));
    }
    for dir in candidates {
        let dll = dir.join("bin").join(arch);
        if dll.is_file() {
            return Some(dll);
        }
    }
    None
}

/// Load a DLL into the process so the delay-load helper of `winfsp_init`
/// finds it by name.
fn load_library(path: &Path) {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::System::LibraryLoader::LoadLibraryW;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let _ = unsafe { LoadLibraryW(windows::core::PCWSTR(wide.as_ptr())) };
}

// ====================================================================== FS ==

/// The copy-on-write filesystem: lower = moved-aside host dir, upper = the
/// isolated write layer. `FileContext` is an open `std::fs::File` (or a dir
/// path), matching WinFsp's "file descriptor" mode.
pub struct CowFs {
    lower: PathBuf,
    upper: PathBuf,
}

impl CowFs {
    fn upper_of(&self, rel: &Path) -> PathBuf {
        self.upper.join(rel)
    }

    fn lower_of(&self, rel: &Path) -> PathBuf {
        self.lower.join(rel)
    }

    /// Where the merged entry lives; upper wins. The empty path is the
    /// volume root, served from the lower (host) dir. A whiteout in upper
    /// shadows the lower entry — but an *upper* entry wins over its own
    /// whiteout (delete-then-recreate must reopen the new file).
    fn resolve(&self, rel: &Path) -> Option<PathBuf> {
        if rel.as_os_str().is_empty() {
            return Some(self.lower.clone());
        }
        let (parent, name) = (rel.parent().unwrap_or(Path::new("")), rel.file_name()?);
        let up = self.upper_of(rel);
        if fs::symlink_metadata(&up).is_ok() {
            return Some(up);
        }
        // Whiteout check (case-insensitive: WinFsp may pass an uppercase name).
        if let Ok(rd) = fs::read_dir(self.upper_of(parent)) {
            let needle = name.to_string_lossy().to_lowercase();
            for e in rd.flatten() {
                let n = e.file_name();
                let n = n.to_string_lossy();
                if let Some(victim) = n.strip_prefix(WHITEOUT_PREFIX) {
                    if victim.to_lowercase() == needle {
                        return None; // deleted in the worktree
                    }
                }
            }
        }
        let low = self.lower_of(rel);
        if fs::symlink_metadata(&low).is_ok() {
            return Some(low);
        }
        // Case-insensitive fallback: NTFS-like lookup by scanning the parent.
        let lower_name = name.to_string_lossy().to_lowercase();
        for (entry, _) in self.merged_dir_entries(parent) {
            if entry.to_string_lossy().to_lowercase() == lower_name {
                return self.resolve(&parent.join(entry));
            }
        }
        None
    }

    /// Copy a lower-only file into upper (parents included). Safe when the
    /// file is already in upper (no-op-ish: skips the copy).
    fn copy_up(&self, rel: &Path) -> io::Result<PathBuf> {
        let src = self.lower_of(rel);
        let dst = self.upper_of(rel);
        if let Some(p) = dst.parent() {
            fs::create_dir_all(p)?;
        }
        if fs::symlink_metadata(&dst).is_err() {
            fs::copy(&src, &dst)?;
        }
        Ok(dst)
    }

    /// Recursively copy a lower-only file or directory tree into upper.
    fn copy_up_tree(&self, rel: &Path) -> io::Result<()> {
        let meta = fs::symlink_metadata(self.lower_of(rel))?;
        if !meta.is_dir() {
            self.copy_up(rel)?;
            return Ok(());
        }
        for (name, _) in self.merged_dir_entries(rel) {
            self.copy_up_tree(&rel.join(name))?;
        }
        Ok(())
    }

    /// Merged directory entries: upper entries win over same-named lower
    /// ones; whiteouts and the lower entries they shadow are excluded.
    fn merged_dir_entries(&self, rel: &Path) -> Vec<(std::ffi::OsString, bool)> {
        let mut names: Vec<(std::ffi::OsString, bool, bool)> = Vec::new(); // (name, is_dir, from_upper)
        let mut whiteouts: Vec<std::ffi::OsString> = Vec::new();
        if let Ok(rd) = fs::read_dir(self.upper_of(rel)) {
            for e in rd.flatten() {
                let name = e.file_name();
                let s = name.to_string_lossy();
                if let Some(victim) = s.strip_prefix(WHITEOUT_PREFIX) {
                    whiteouts.push(std::ffi::OsString::from(victim));
                    continue;
                }
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                names.push((name, is_dir, true));
            }
        }
        if let Ok(rd) = fs::read_dir(self.lower_of(rel)) {
            for e in rd.flatten() {
                let name = e.file_name();
                if names.iter().any(|(n, _, _)| *n == name) {
                    continue; // shadowed by an upper entry
                }
                // Case-insensitive shadowing: WinFsp may normalize the
                // whiteout victim name to uppercase.
                if whiteouts.iter().any(|w| {
                    w.to_string_lossy().to_lowercase() == name.to_string_lossy().to_lowercase()
                }) {
                    continue;
                }
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                names.push((name, is_dir, false));
            }
        }
        names.sort_by(|a, b| a.0.cmp(&b.0));
        names.into_iter().map(|(n, d, _)| (n, d)).collect()
    }

    /// Delete a merged path: remove the upper copy (if any), then whiteout
    /// the lower one (if any) so it cannot reappear.
    fn delete_merged(&self, rel: &Path) -> io::Result<()> {
        let up = self.upper_of(rel);
        if let Ok(m) = fs::symlink_metadata(&up) {
            if m.is_dir() {
                fs::remove_dir_all(&up)?;
            } else {
                fs::remove_file(&up)?;
            }
        }
        if fs::symlink_metadata(self.lower_of(rel)).is_err() {
            return Ok(()); // nothing left to hide
        }
        // The whiteout must carry the *actual* on-disk name: WinFsp passes
        // the name normalized to uppercase, and Windows' case-insensitive
        // filesystem would happily resolve the uppercase path and keep its
        // spelling. Enumerate the parent to find the real name.
        let needle = rel
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let actual = {
            let parent = rel.parent().unwrap_or(Path::new(""));
            let find = |dir: &Path| {
                fs::read_dir(dir).ok().and_then(|rd| {
                    rd.flatten()
                        .find(|e| e.file_name().to_string_lossy().to_lowercase() == needle)
                })
            };
            find(&self.upper_of(parent))
                .or_else(|| find(&self.lower_of(parent)))
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .unwrap_or_else(|| {
                    rel.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                })
        };
        let wh = up.with_file_name(format!("{WHITEOUT_PREFIX}{actual}"));
        if let Some(p) = wh.parent() {
            fs::create_dir_all(p)?;
        }
        fs::write(&wh, b"")?;
        Ok(())
    }

    /// Remove the whiteout for `rel`, if any (case-insensitively).
    fn clear_whiteout(&self, rel: &Path) {
        let (parent, name) = (rel.parent().unwrap_or(Path::new("")), rel.file_name());
        let Some(name) = name else { return };
        let needle = name.to_string_lossy().to_lowercase();
        if let Ok(rd) = fs::read_dir(self.upper_of(parent)) {
            for e in rd.flatten() {
                let n = e.file_name();
                let s = n.to_string_lossy();
                if let Some(victim) = s.strip_prefix(WHITEOUT_PREFIX) {
                    if victim.to_lowercase() == needle {
                        let _ = fs::remove_file(e.path());
                    }
                }
            }
        }
    }
    fn fill_file_info(meta: &fs::Metadata, attrs: u32, info: &mut FileInfo) {
        info.file_attributes = attrs;
        info.allocation_size = meta.len();
        info.file_size = meta.len();
        info.creation_time = to_filetime(meta.created().ok());
        info.last_access_time = to_filetime(meta.accessed().ok());
        info.last_write_time = to_filetime(meta.modified().ok());
        info.change_time = info.last_write_time;
        info.hard_links = 1;
    }
}

/// SystemTime -> FILETIME (100ns intervals since 1601-01-01).
fn to_filetime(t: Option<SystemTime>) -> u64 {
    let Some(t) = t else { return 0 };
    let dur = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    let secs = dur.as_secs() as i128 + 11_644_473_600; // 1601..1970 offset
    let sub = dur.subsec_nanos() as i128 / 100;
    (secs * 10_000_000 + sub) as u64
}

/// Volume-relative name ("\a\b.txt") -> relative path ("a\b.txt").
fn rel_of(name: &U16CStr) -> PathBuf {
    let p = PathBuf::from(name.to_os_string());
    p.components()
        .filter(|c| {
            !matches!(
                c,
                std::path::Component::Prefix(_) | std::path::Component::RootDir
            )
        })
        .collect()
}

impl FileSystemContext for CowFs {
    type FileContext = Handle;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        _security_descriptor: Option<&mut [c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> FspResult<FileSecurity> {
        let rel = rel_of(file_name);
        let is_dir = self
            .resolve(&rel)
            .and_then(|p| fs::symlink_metadata(p).ok())
            .map(|m| m.is_dir())
            .unwrap_or(false);
        let attrs = if is_dir {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            FILE_ATTRIBUTE_NORMAL
        };
        // This filesystem does not implement ACLs: report a zero-size
        // descriptor (WinFsp grants default access in that case). Returning a
        // real descriptor would require a self-contained SD (DACL inside the
        // buffer); a dangling pointer here makes the kernel refuse the path.
        Ok(FileSecurity {
            reparse: false,
            sz_security_descriptor: 0,
            attributes: attrs,
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        granted_access: u32,
        file_info: &mut OpenFileInfo,
    ) -> FspResult<Self::FileContext> {
        let rel = rel_of(file_name);
        let path = self
            .resolve(&rel)
            .ok_or(FspError::IO(ErrorKind::NotFound))?;
        let meta = fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            CowFs::fill_file_info(&meta, FILE_ATTRIBUTE_DIRECTORY, file_info.as_mut());
            return Ok(Handle::Dir(rel));
        }
        // Write opens copy the file up first, so host data is never touched.
        let wants_write = granted_access & FILE_GENERIC_WRITE != 0;
        let path = if wants_write && path.starts_with(&self.lower) {
            self.copy_up(&rel)?
        } else {
            path
        };
        let f = fs::OpenOptions::new()
            .read(true)
            .write(wants_write)
            .open(&path)?;
        CowFs::fill_file_info(&f.metadata()?, FILE_ATTRIBUTE_NORMAL, file_info.as_mut());
        Ok(Handle::File {
            f,
            rel: rel.clone(),
            writable: wants_write,
        })
    }

    fn close(&self, context: Self::FileContext) {
        drop(context);
    }

    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        granted_access: u32,
        file_attributes: u32,
        _security_descriptor: Option<&[c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> FspResult<Self::FileContext> {
        let rel = rel_of(file_name);
        let dst = self.upper_of(&rel);
        // A leftover whiteout (delete-then-recreate) must not shadow the
        // freshly created entry.
        self.clear_whiteout(&rel);
        if file_attributes & FILE_ATTRIBUTE_DIRECTORY != 0 || create_options & 0x1 != 0
        /* FILE_DIRECTORY_FILE */
        {
            fs::create_dir_all(&dst)?;
            let meta = fs::symlink_metadata(&dst)?;
            CowFs::fill_file_info(&meta, FILE_ATTRIBUTE_DIRECTORY, file_info.as_mut());
            return Ok(Handle::Dir(rel));
        }
        if let Some(p) = dst.parent() {
            fs::create_dir_all(p)?;
        }
        // Dispositions that truncate (CREATE_ALWAYS) come through the
        // `overwrite` callback afterwards; here we only establish the file.
        let f = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false) // CREATE_ALWAYS truncation arrives via `overwrite`
            .open(&dst)?;
        let meta = f.metadata()?;
        CowFs::fill_file_info(&meta, FILE_ATTRIBUTE_NORMAL, file_info.as_mut());
        let _ = granted_access;
        Ok(Handle::File {
            f,
            rel: rel.clone(),
            writable: true,
        })
    }

    fn cleanup(&self, context: &Self::FileContext, file_name: Option<&U16CStr>, flags: u32) {
        if flags & FSP_CLEANUP_DELETE == 0 {
            return;
        }
        // The kernel may not pass a name for delete-on-close; the open handle
        // carries the path as a fallback.
        let rel = match file_name {
            Some(name) => rel_of(name),
            None => match context {
                Handle::File { rel, .. } => rel.clone(),
                Handle::Dir(rel) => rel.clone(),
            },
        };
        if let Err(e) = self.delete_merged(&rel) {
            eprintln!("cowt: warning: delete of {} failed: {e}", rel.display());
        }
        let _ = context;
    }

    fn flush(
        &self,
        context: Option<&Self::FileContext>,
        _file_info: &mut FileInfo,
    ) -> FspResult<()> {
        if let Some(Handle::File { f, writable, .. }) = context {
            if *writable {
                // FlushFileBuffers on a read-only handle fails with
                // ACCESS_DENIED; only fsync write-opened handles.
                f.sync_all()?;
            }
        }
        Ok(())
    }

    fn get_file_info(
        &self,
        context: &Self::FileContext,
        file_info: &mut FileInfo,
    ) -> FspResult<()> {
        match context {
            Handle::File { f, .. } => {
                let meta = f.metadata()?;
                CowFs::fill_file_info(&meta, FILE_ATTRIBUTE_NORMAL, file_info);
            }
            Handle::Dir(rel) => {
                let path = self.resolve(rel).ok_or(FspError::IO(ErrorKind::NotFound))?;
                let meta = fs::symlink_metadata(&path)?;
                CowFs::fill_file_info(&meta, FILE_ATTRIBUTE_DIRECTORY, file_info);
            }
        }
        Ok(())
    }

    fn overwrite(
        &self,
        context: &Self::FileContext,
        _file_attributes: u32,
        _replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FileInfo,
    ) -> FspResult<()> {
        if let Handle::File { f, .. } = context {
            f.set_len(0)?;
            let meta = f.metadata()?;
            CowFs::fill_file_info(&meta, FILE_ATTRIBUTE_NORMAL, file_info);
        }
        Ok(())
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        _pattern: Option<&U16CStr>,
        marker: winfsp::filesystem::DirMarker<'_>,
        buffer: &mut [u8],
    ) -> FspResult<u32> {
        let Handle::Dir(rel) = context else {
            return Err(FspError::IO(ErrorKind::InvalidInput));
        };
        // Resume after the marker name (names are returned sorted).
        let resume_after = marker
            .inner()
            .map(|m| PathBuf::from(std::ffi::OsString::from_wide(m)));
        let mut cursor = 0u32;
        let mut push = |name: &std::ffi::OsString, is_dir: bool| -> FspResult<bool> {
            let mut di = DirInfo::<255>::new();
            let attrs = if is_dir {
                FILE_ATTRIBUTE_DIRECTORY
            } else {
                FILE_ATTRIBUTE_NORMAL
            };
            di.file_info_mut().file_attributes = attrs;
            di.set_name(name)?;
            Ok(di.append_to_buffer(buffer, &mut cursor))
        };
        // Real sizes/times are filled lazily by `get_file_info` on demand;
        // the listing itself only needs names and attributes.
        for (name, is_dir) in self.merged_dir_entries(rel) {
            if let Some(after) = &resume_after {
                if name <= *after {
                    continue;
                }
            }
            if !push(&name, is_dir)? {
                break;
            }
        }
        DirInfo::<255>::finalize_buffer(buffer, &mut cursor);
        Ok(cursor)
    }

    fn rename(
        &self,
        _context: &Self::FileContext,
        file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_exists: bool,
    ) -> FspResult<()> {
        let src = rel_of(file_name);
        let dst = rel_of(new_file_name);
        let lower_has = fs::symlink_metadata(self.lower_of(&src)).is_ok();
        let src_up = self.upper_of(&src);
        if lower_has && fs::symlink_metadata(&src_up).is_err() {
            // Lower-only source: copy it up so the rename happens entirely
            // in the upper layer, then whiteout the old name.
            self.copy_up_tree(&src)?;
        }
        if fs::symlink_metadata(&src_up).is_err() {
            return Err(FspError::IO(ErrorKind::NotFound));
        }
        let dst_up = self.upper_of(&dst);
        if let Some(p) = dst_up.parent() {
            fs::create_dir_all(p)?;
        }
        if replace_if_exists {
            let _ = self.delete_merged(&dst);
        }
        fs::rename(&src_up, &dst_up)?;
        if lower_has {
            let name = src
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "no file name"))?;
            let wh = src_up.with_file_name(format!("{WHITEOUT_PREFIX}{name}"));
            fs::write(&wh, b"")?;
        }
        Ok(())
    }

    fn set_basic_info(
        &self,
        _context: &Self::FileContext,
        _file_attributes: u32,
        _creation_time: u64,
        _last_access_time: u64,
        _last_write_time: u64,
        _last_change_time: u64,
        _file_info: &mut FileInfo,
    ) -> FspResult<()> {
        // Attribute/timestamp updates are cosmetic for cowt's purpose.
        Ok(())
    }

    fn set_delete(
        &self,
        _context: &Self::FileContext,
        file_name: &U16CStr,
        delete_file: bool,
    ) -> FspResult<()> {
        let _ = (file_name, delete_file);
        // The actual deletion happens in `cleanup` (WinFsp contract).
        Ok(())
    }

    fn set_file_size(
        &self,
        context: &Self::FileContext,
        new_size: u64,
        _set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> FspResult<()> {
        if let Handle::File { f, .. } = context {
            f.set_len(new_size)?;
            let meta = f.metadata()?;
            CowFs::fill_file_info(&meta, FILE_ATTRIBUTE_NORMAL, file_info);
        }
        Ok(())
    }

    fn read(&self, context: &Self::FileContext, buffer: &mut [u8], offset: u64) -> FspResult<u32> {
        let Handle::File { f, .. } = context else {
            return Err(FspError::IO(ErrorKind::InvalidInput));
        };
        let n = f.seek_read(buffer, offset)?;
        Ok(n as u32)
    }

    fn write(
        &self,
        context: &Self::FileContext,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        _constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> FspResult<u32> {
        let Handle::File { f, .. } = context else {
            return Err(FspError::IO(ErrorKind::InvalidInput));
        };
        let off = if write_to_eof {
            f.metadata()?.len().saturating_add(offset)
        } else {
            offset
        };
        let n = f.seek_write(buffer, off)?;
        let meta = f.metadata()?;
        CowFs::fill_file_info(&meta, FILE_ATTRIBUTE_NORMAL, file_info);
        Ok(n as u32)
    }

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> FspResult<()> {
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let mut free = 0u64;
        let mut total = 0u64;
        let mut _free_total = 0u64;
        let wide: Vec<u16> = self
            .upper
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                windows::core::PCWSTR(wide.as_ptr()),
                Some(&mut free),
                Some(&mut total),
                Some(&mut _free_total),
            )
        };
        let ok = ok.is_ok();
        out_volume_info.total_size = if ok { total } else { 0 };
        out_volume_info.free_size = if ok { free } else { 0 };
        out_volume_info.set_volume_label("cowt");
        Ok(())
    }
}
