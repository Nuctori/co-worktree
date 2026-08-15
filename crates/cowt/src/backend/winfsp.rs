//! Windows backend: a WinFsp user-mode filesystem with copy-on-write.
//!
//! WinFsp (a signed kernel driver, installed via the official installer or
//! `choco install winfsp`) lets a user-mode process host a real filesystem.
//! This backend implements a passthrough-with-copy-up FS:
//!
//! ```text
//! target (junction) ──▶ state/<id>/view   (WinFsp mount)
//!                        ├── lower ─▶ state/<id>/real   (host dir, moved aside)
//!                        └── upper ─▶ state/<id>/upper  (isolated writes)
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
/// handle, or a directory path (re-listed on demand).
pub enum Handle {
    File(std::fs::File),
    Dir(PathBuf),
}

/// Paths derived from the worktree state dir (upper's parent): the moved-aside
/// host dir (`real`) and the mountpoint for the WinFsp volume (`view`).
struct Layout {
    real: PathBuf,
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
        winfsp::winfsp_init().map(|_| ()).map_err(|e| {
            anyhow::anyhow!(
                "WinFsp is not installed or its DLL failed to load: {e} \
                 (install WinFsp from https://winfsp.dev or `choco install winfsp`)"
            )
        })
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
        // `real` and the target is a dangling junction. Restore it first.
        if junction_exists(mountpoint) {
            if layout.real.exists() {
                eprintln!(
                    "cowt: recovering stale mount state at {}",
                    mountpoint.display()
                );
                restore(mountpoint, &layout)?;
            } else {
                bail!(
                    "{} is a junction but no moved-aside directory was found; \
                     refusing to touch a foreign junction",
                    mountpoint.display()
                );
            }
        }
        // Drop any leftover view dir (a dead WinFsp mount may leave a reparse
        // point behind); harmless if the mount is still alive.
        let _ = fs::remove_dir_all(&layout.view);

        // Move the host dir aside, put a junction to the (not yet mounted)
        // view in its place. On any later failure the dance is rolled back so
        // the host dir never stays stranded in `real`.
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
            fs::create_dir_all(&layout.view)
                .with_context(|| format!("create view dir {}", layout.view.display()))?;
            junction::create(mountpoint, &layout.view).with_context(|| {
                format!(
                    "create junction {} -> {}",
                    mountpoint.display(),
                    layout.view.display()
                )
            })?;

            let mut vp = VolumeParams::new();
            vp.filesystem_name("cowt")
                .post_cleanup_when_modified_only(true)
                .unicode_on_disk(true);
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
                    sd: allow_all_sd(),
                },
            )
            .context("create WinFsp filesystem")?;
            host.mount(layout.view.clone())
                .with_context(|| format!("mount WinFsp volume at {}", layout.view.display()))?;
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
            // Roll back: drop the junction (if any) and move the host dir
            // back into place.
            if junction_exists(mountpoint) {
                let _ = fs::remove_dir(mountpoint);
            }
            let _ = restore(mountpoint, &layout);
        }
        result
    }

    fn unmount(&self, mountpoint: &Path) -> Result<()> {
        // Idempotent: works from a fresh process (`cowt drop --force`) or from
        // the owning `cowt run` after its child exited. The WinFsp volume
        // itself dies with its host process; here we only undo the junction.
        let state = match fs::read_link(mountpoint) {
            Ok(target) => target,
            Err(_) => return Ok(()), // not a junction: nothing to restore
        };
        if state.file_name().map(|n| n != "view").unwrap_or(true) {
            bail!(
                "junction at {} points at {} (not a cowt view); refusing to touch it",
                mountpoint.display(),
                state.display()
            );
        }
        let layout = Layout {
            real: state.parent().unwrap_or(Path::new("")).join("real"),
            view: state.clone(),
        };
        let _ = fs::remove_dir_all(&state);
        fs::remove_dir(mountpoint)
            .with_context(|| format!("remove junction at {}", mountpoint.display()))?;
        restore(mountpoint, &layout)
    }

    fn is_mounted(&self, mountpoint: &Path) -> bool {
        junction_exists(mountpoint)
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

fn junction_exists(path: &Path) -> bool {
    fs::read_link(path).is_ok()
}

// ====================================================================== FS ==

/// The copy-on-write filesystem: lower = moved-aside host dir, upper = the
/// isolated write layer. `FileContext` is an open `std::fs::File` (or a dir
/// path), matching WinFsp's "file descriptor" mode.
pub struct CowFs {
    lower: PathBuf,
    upper: PathBuf,
    sd: Vec<u8>,
}

impl CowFs {
    fn upper_of(&self, rel: &Path) -> PathBuf {
        self.upper.join(rel)
    }

    fn lower_of(&self, rel: &Path) -> PathBuf {
        self.lower.join(rel)
    }

    /// Where the merged entry lives; upper wins. The empty path is the
    /// volume root, served from the lower (host) dir.
    fn resolve(&self, rel: &Path) -> Option<PathBuf> {
        if rel.as_os_str().is_empty() {
            return Some(self.lower.clone());
        }
        let up = self.upper_of(rel);
        if fs::symlink_metadata(&up).is_ok() {
            return Some(up);
        }
        let low = self.lower_of(rel);
        if fs::symlink_metadata(&low).is_ok() {
            return Some(low);
        }
        // Case-insensitive fallback: NTFS-like lookup by scanning the parent.
        let (parent, name) = (rel.parent().unwrap_or(Path::new("")), rel.file_name()?);
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
    /// ones; whiteouts and other shadowed names are excluded.
    fn merged_dir_entries(&self, rel: &Path) -> Vec<(std::ffi::OsString, bool)> {
        let mut names: Vec<(std::ffi::OsString, bool, bool)> = Vec::new(); // (name, is_dir, from_upper)
        if let Ok(rd) = fs::read_dir(self.upper_of(rel)) {
            for e in rd.flatten() {
                let name = e.file_name();
                if name.to_string_lossy().starts_with(WHITEOUT_PREFIX) {
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
        let name = rel
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "no file name"))?;
        let wh = up.with_file_name(format!("{WHITEOUT_PREFIX}{name}"));
        if let Some(p) = wh.parent() {
            fs::create_dir_all(p)?;
        }
        fs::write(&wh, b"")?;
        Ok(())
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
        security_descriptor: Option<&mut [c_void]>,
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
        if let Some(buf) = security_descriptor {
            let n = self.sd.len().min(buf.len());
            unsafe {
                std::ptr::copy_nonoverlapping(self.sd.as_ptr(), buf.as_mut_ptr().cast::<u8>(), n);
            }
        }
        Ok(FileSecurity {
            reparse: false,
            sz_security_descriptor: self.sd.len() as u64,
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
        Ok(Handle::File(f))
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
        Ok(Handle::File(f))
    }

    fn cleanup(&self, context: &Self::FileContext, file_name: Option<&U16CStr>, flags: u32) {
        if flags & FSP_CLEANUP_DELETE == 0 {
            return;
        }
        let Some(name) = file_name else { return };
        let rel = rel_of(name);
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
        if let Some(Handle::File(f)) = context {
            f.sync_all()?;
        }
        Ok(())
    }

    fn get_file_info(
        &self,
        context: &Self::FileContext,
        file_info: &mut FileInfo,
    ) -> FspResult<()> {
        match context {
            Handle::File(f) => {
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
        if let Handle::File(f) = context {
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
        _file_name: &U16CStr,
        _delete_file: bool,
    ) -> FspResult<()> {
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
        if let Handle::File(f) = context {
            f.set_len(new_size)?;
            let meta = f.metadata()?;
            CowFs::fill_file_info(&meta, FILE_ATTRIBUTE_NORMAL, file_info);
        }
        Ok(())
    }

    fn read(&self, context: &Self::FileContext, buffer: &mut [u8], offset: u64) -> FspResult<u32> {
        let Handle::File(f) = context else {
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
        let Handle::File(f) = context else {
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

/// A security descriptor granting Everyone full access — the WinFsp
/// passthrough convention. Built once per process.
fn allow_all_sd() -> Vec<u8> {
    use std::mem::size_of;
    use windows::Win32::Security::{
        AddAccessAllowedAceEx, CreateWellKnownSid, GetSecurityDescriptorLength, InitializeAcl,
        InitializeSecurityDescriptor, SetSecurityDescriptorDacl, WinWorldSid, ACL, ACL_REVISION,
        CONTAINER_INHERIT_ACE, OBJECT_INHERIT_ACE, PSECURITY_DESCRIPTOR, PSID, SECURITY_DESCRIPTOR,
        SID,
    };

    // GENERIC_ALL (windows-rs does not export it in Win32::Security).
    const GENERIC_ALL: u32 = 0x1000_0000;
    // SECURITY_DESCRIPTOR_REVISION.
    const SD_REVISION: u32 = 1;

    #[repr(C)]
    struct AclWithBuf {
        acl: ACL,
        buf: [u8; 1024],
    }

    unsafe {
        let mut sd = SECURITY_DESCRIPTOR::default();
        InitializeSecurityDescriptor(
            PSECURITY_DESCRIPTOR((&mut sd as *mut SECURITY_DESCRIPTOR).cast()),
            SD_REVISION,
        )
        .expect("InitializeSecurityDescriptor");
        let mut sid = SID::default();
        let mut sid_len = size_of::<SID>() as u32;
        CreateWellKnownSid(
            WinWorldSid,
            None,
            Some(PSID((&mut sid as *mut SID).cast())),
            &mut sid_len,
        )
        .expect("CreateWellKnownSid");
        let mut acl = AclWithBuf {
            acl: ACL::default(),
            buf: [0; 1024],
        };
        InitializeAcl(&mut acl.acl, size_of::<AclWithBuf>() as u32, ACL_REVISION)
            .expect("InitializeAcl");
        AddAccessAllowedAceEx(
            &mut acl.acl,
            ACL_REVISION,
            OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
            GENERIC_ALL,
            PSID((&mut sid as *mut SID).cast()),
        )
        .expect("AddAccessAllowedAceEx");
        SetSecurityDescriptorDacl(
            PSECURITY_DESCRIPTOR((&mut sd as *mut SECURITY_DESCRIPTOR).cast()),
            true,
            Some(&acl.acl as *const ACL),
            false,
        )
        .expect("SetSecurityDescriptorDacl");
        let len = GetSecurityDescriptorLength(PSECURITY_DESCRIPTOR(
            (&sd as *const SECURITY_DESCRIPTOR).cast_mut().cast(),
        )) as usize;
        let mut out = vec![0u8; len];
        std::ptr::copy_nonoverlapping(
            (&sd as *const SECURITY_DESCRIPTOR).cast::<u8>(),
            out.as_mut_ptr(),
            len,
        );
        out
    }
}
