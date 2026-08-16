//! macOS backend: a userspace FUSE filesystem with copy-on-write, hosted on
//! **FUSE-T** (a kext-less libfuse-compatible runtime that serves FUSE over
//! the built-in NFS client — no kernel extension, no approval prompts, which
//! is what makes it the only viable option on CI runners and Apple Silicon).
//!
//! Apple removed the kernel union mount (`/Library/Filesystems/union.fs`)
//! from current macOS images entirely — `mount -t union` fails with
//! ENOENT/ENOTSUP — so this backend implements the same semantics as the
//! Windows one, in userspace:
//!
//! ```text
//! target (symlink) ──▶ state/<id>/view   (FUSE-T mount)
//!                        ├── lower ─▶ state/<id>/real   (host dir, moved aside)
//!                        └── upper ─▶ state/<id>/upper  (isolated writes)
//! ```
//!
//! While a worktree runs, the host directory is moved aside to `real` and
//! the original path becomes a symlink to the mounted view. Reads pass
//! through to `real`; writes copy files up into `upper` first; deletions of
//! lower-only files leave `.wh.<name>` whiteouts. Mounting needs root
//! (`mount` is privileged), like the old union backend.
//!
//! FUSE-T install: `brew install macos-fuse-t/homebrew-cask/fuse-t` and make
//! the libfuse pkg-config file visible (see scripts/macos/install-fuse-t.sh).

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use fuser::{
    BackgroundSession, FileAttr, FileType, Filesystem, KernelConfig, MountOption, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs,
    ReplyWrite, Request, Session,
};

use super::{Backend, MountGuard};

pub struct FuseT;

/// The copy-on-write filesystem. `lower` = the moved-aside host dir,
/// `upper` = the isolated write layer. Inodes are assigned by a path table
/// (the FUSE protocol addresses files by inode, not by path).
pub struct CowFs {
    lower: PathBuf,
    upper: PathBuf,
    inos: Mutex<InoTable>,
    fhs: Mutex<HashMap<u64, Handle>>,
    next_fh: AtomicU64,
}

const ROOT_INO: u64 = 1;
const WHITEOUT_PREFIX: &str = ".wh.";
/// Reserved namespace for winfsp/macos copy_up temp files (round-36): a
/// crash between copy and rename leaves `.cowt-copy-tmp.<name>` in upper;
/// effective_manifest filters it. A USER file with this prefix through the
/// view would be silently invisible to diff and never applied (round-40
/// review) — refuse at create like the whiteout namespace.
const COPY_TMP_PREFIX: &str = ".cowt-copy-tmp.";

/// True when `name` falls into a reserved namespace (whiteouts, copy_up
/// temp files) — such user files would be misinterpreted or invisible at
/// apply time. Every create path (create/mkdir/mknod/symlink) must refuse
/// them (round-21 + round-40 review).
fn is_reserved_name(name: &OsStr) -> bool {
    let s = name.to_string_lossy();
    s.starts_with(WHITEOUT_PREFIX) || s.starts_with(COPY_TMP_PREFIX)
}

struct InoTable {
    next: u64,
    by_ino: HashMap<u64, PathBuf>,
    by_path: HashMap<PathBuf, u64>,
}

impl InoTable {
    fn new() -> Self {
        let mut by_ino = HashMap::new();
        let mut by_path = HashMap::new();
        by_ino.insert(ROOT_INO, PathBuf::new());
        by_path.insert(PathBuf::new(), ROOT_INO);
        InoTable {
            next: ROOT_INO + 1,
            by_ino,
            by_path,
        }
    }

    fn ino_of(&self, rel: &Path) -> Option<u64> {
        self.by_path.get(rel).copied()
    }

    fn path_of(&self, ino: u64) -> Option<PathBuf> {
        self.by_ino.get(&ino).cloned()
    }

    fn alloc(&mut self, rel: PathBuf) -> u64 {
        if let Some(ino) = self.by_path.get(&rel) {
            return *ino;
        }
        let ino = self.next;
        self.next += 1;
        self.by_ino.insert(ino, rel.clone());
        self.by_path.insert(rel, ino);
        ino
    }
}

enum Handle {
    File(std::fs::File),
    Dir,
}

fn ttl() -> Duration {
    Duration::from_secs(1)
}

impl CowFs {
    fn upper_of(&self, rel: &Path) -> PathBuf {
        self.upper.join(rel)
    }

    fn lower_of(&self, rel: &Path) -> PathBuf {
        self.lower.join(rel)
    }

    /// Where the merged entry lives; upper wins. The empty path is the root,
    /// served from the lower (host) dir. A whiteout in upper shadows the
    /// lower entry entirely.
    fn resolve(&self, rel: &Path) -> Option<PathBuf> {
        if rel.as_os_str().is_empty() {
            return Some(self.lower.clone());
        }
        let up = self.upper_of(rel);
        if fs::symlink_metadata(&up).is_ok() {
            return Some(up);
        }
        // Whiteout check: the entry itself or any ancestor may be whiteouted
        // (directory whiteouts shadow whole subtrees).
        if self.is_shadowed(rel) {
            return None; // deleted in the worktree
        }
        let low = self.lower_of(rel);
        if fs::symlink_metadata(&low).is_ok() {
            return Some(low);
        }
        None
    }

    /// True if `rel` or any of its ancestors is whiteouted in upper: a
    /// directory whiteout shadows the whole subtree beneath it.
    fn is_shadowed(&self, rel: &Path) -> bool {
        let mut cur = rel;
        loop {
            let (parent, name) = match (cur.parent(), cur.file_name()) {
                (Some(p), Some(n)) if !cur.as_os_str().is_empty() => (p, n),
                _ => return false,
            };
            let needle = name.to_string_lossy().to_lowercase();
            if let Ok(rd) = fs::read_dir(self.upper_of(parent)) {
                for e in rd.flatten() {
                    let s = e.file_name();
                    let s = s.to_string_lossy();
                    if let Some(victim) = s.strip_prefix(WHITEOUT_PREFIX) {
                        if victim.to_lowercase() == needle {
                            return true;
                        }
                    }
                }
            }
            cur = parent;
        }
    }

    /// Merged directory entries: upper wins; whiteouts and the lower entries
    /// they shadow are excluded.
    fn merged_dir_entries(&self, rel: &Path) -> Vec<std::ffi::OsString> {
        let mut names: Vec<std::ffi::OsString> = Vec::new();
        if let Ok(rd) = fs::read_dir(self.upper_of(rel)) {
            for e in rd.flatten() {
                let name = e.file_name();
                let s = name.to_string_lossy();
                if s.starts_with(WHITEOUT_PREFIX) {
                    continue; // whiteouts are never listed
                }
                names.push(name);
            }
        }
        if let Ok(rd) = fs::read_dir(self.lower_of(rel)) {
            for e in rd.flatten() {
                let name = e.file_name();
                // APFS (default) resolves names case-insensitively: an
                // upper `foo.txt` shadows a lower `Foo.txt`, or a reopen
                // could resolve to the wrong copy and copy_up would
                // silently overwrite the worktree's file (round-38-01,
                // mirrors the winfsp fix).
                let folded = name.to_string_lossy().to_lowercase();
                if names
                    .iter()
                    .any(|n| n.to_string_lossy().to_lowercase() == folded)
                {
                    continue; // shadowed by an upper entry
                }
                if self.is_shadowed(&rel.join(&name)) {
                    continue; // shadowed by a whiteout (entry or ancestor)
                }
                names.push(name);
            }
        }
        names.sort();
        names
    }

    /// Copy a lower-only file into upper (parents included). Atomic via a
    /// temp file + rename; an existing upper copy is only trusted when its
    /// size matches the lower file (a torn copy from a crashed run must not
    /// be silently reused as the base).
    fn copy_up(&self, rel: &Path) -> io::Result<PathBuf> {
        let src = self.lower_of(rel);
        let dst = self.upper_of(rel);
        if let Some(p) = dst.parent() {
            fs::create_dir_all(p)?;
        }
        match fs::symlink_metadata(&dst) {
            Err(_) => {}
            Ok(m) => {
                let src_meta = fs::symlink_metadata(&src)?;
                if m.len() == src_meta.len() {
                    return Ok(dst); // trusted existing copy
                }
                // Torn copy (wrong size): replace below.
            }
        }
        let tmp = dst.with_file_name(format!(
            ".cowt-copy-tmp.{}",
            dst.file_name().unwrap_or_default().to_string_lossy()
        ));
        fs::copy(&src, &tmp)?;
        fs::rename(&tmp, &dst)?;
        Ok(dst)
    }

    /// Recursively copy a lower-only file or directory tree into upper.
    fn copy_up_tree(&self, rel: &Path) -> io::Result<()> {
        let meta = fs::symlink_metadata(self.lower_of(rel))?;
        if !meta.is_dir() {
            self.copy_up(rel)?;
            return Ok(());
        }
        for name in self.merged_dir_entries(rel) {
            self.copy_up_tree(&rel.join(name))?;
        }
        Ok(())
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
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
        let wh = up.with_file_name(format!("{WHITEOUT_PREFIX}{name}"));
        if let Some(p) = wh.parent() {
            fs::create_dir_all(p)?;
        }
        fs::write(&wh, b"")?;
        Ok(())
    }

    /// Remove the whiteout for `rel` and every ancestor, if any: recreating
    /// a path clears the shadow so lower entries become visible again
    /// (overlayfs semantics without opaque markers). Exact-name matching:
    /// a differently-cased recreate keeps the old whiteout (D+A in diff).
    fn clear_whiteout(&self, rel: &Path) {
        let mut cur = rel;
        loop {
            let (parent, name) = match (cur.parent(), cur.file_name()) {
                (Some(p), Some(n)) if !cur.as_os_str().is_empty() => (p, n),
                _ => return,
            };
            if let Ok(rd) = fs::read_dir(self.upper_of(parent)) {
                for e in rd.flatten() {
                    let n = e.file_name();
                    let s = n.to_string_lossy();
                    if let Some(victim) = s.strip_prefix(WHITEOUT_PREFIX) {
                        if victim == name.to_string_lossy() {
                            let _ = fs::remove_file(e.path());
                        }
                    }
                }
            }
            cur = parent;
        }
    }
    /// FUSE attributes for a merged path.
    fn attr_of(&self, rel: &Path) -> io::Result<FileAttr> {
        let path = self
            .resolve(rel)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not found"))?;
        let meta = fs::symlink_metadata(&path)?;
        let ino = self.inos.lock().unwrap().alloc(rel.to_path_buf());
        let kind = if meta.is_dir() {
            FileType::Directory
        } else if meta.file_type().is_symlink() {
            FileType::Symlink
        } else {
            FileType::RegularFile
        };
        // Report the REAL permission bits: hard-coding 0644/0755 would let
        // the kernel enforce wrong modes — a host 0600 file would show as
        // world-readable while running, and a 0755 script would fail to
        // exec through the view.
        let perm = {
            use std::os::unix::fs::PermissionsExt;
            (meta.permissions().mode() & 0o7777) as u16
        };
        Ok(FileAttr {
            ino,
            size: meta.len(),
            blocks: meta.len().div_ceil(512),
            atime: meta.accessed().unwrap_or(SystemTime::UNIX_EPOCH),
            mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            ctime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            crtime: SystemTime::UNIX_EPOCH,
            kind,
            perm,
            nlink: 1,
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
            rdev: 0,
            blksize: 4096,
            flags: 0,
        })
    }
}

impl Filesystem for CowFs {
    fn init(&mut self, _req: &Request<'_>, _config: &mut KernelConfig) -> Result<(), i32> {
        Ok(())
    }

    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let rel = match self
            .inos
            .lock()
            .unwrap()
            .path_of(parent)
            .map(|p| p.join(name))
        {
            Some(r) => r,
            None => return reply.error(libc::ENOENT),
        };
        match self.attr_of(&rel) {
            Ok(attr) => reply.entry(&ttl(), &attr, 0),
            Err(_) => reply.error(libc::ENOENT),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let rel = match self.inos.lock().unwrap().path_of(ino) {
            Some(r) => r,
            None => return reply.error(libc::ENOENT),
        };
        match self.attr_of(&rel) {
            Ok(attr) => reply.attr(&ttl(), &attr),
            Err(_) => reply.error(libc::ENOENT),
        }
    }

    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        // chmod inside the worktree must persist into the isolated layer —
        // otherwise the whole chmod-only detection chain is dead on macOS
        // (round-30). Copy up first (never touch the host dir), then apply
        // the permission bits to the upper copy.
        if let Some(mode) = mode {
            let rel = self.inos.lock().unwrap().path_of(ino);
            if let Some(rel) = rel {
                let up = self.upper_of(&rel);
                if fs::symlink_metadata(&up).is_err() {
                    let _ = self.copy_up(&rel);
                }
                use std::os::unix::fs::PermissionsExt;
                if let Err(e) = fs::set_permissions(&up, fs::Permissions::from_mode(mode & 0o7777))
                {
                    return reply.error(e.raw_os_error().unwrap_or(libc::EIO));
                }
            }
        }
        // Truncation is the only other attribute cowt needs.
        if let Some(size) = size {
            let truncated = if let Some(fh) = fh {
                let lock = self.fhs.lock().unwrap();
                match lock.get(&fh) {
                    Some(Handle::File(f)) => Some(f.set_len(size)),
                    _ => None,
                }
            } else {
                // Inode-based truncate without a handle: ensure the file is
                // in upper first (never touch the host dir).
                let rel = self.inos.lock().unwrap().path_of(ino);
                match rel {
                    Some(rel) => {
                        let up = self.upper_of(&rel);
                        if fs::symlink_metadata(&up).is_err() {
                            let _ = self.copy_up(&rel);
                        }
                        Some(
                            fs::OpenOptions::new()
                                .write(true)
                                .open(&up)
                                .and_then(|f| f.set_len(size)),
                        )
                    }
                    None => None,
                }
            };
            if let Some(Err(e)) = truncated {
                return reply.error(e.raw_os_error().unwrap_or(libc::EIO));
            }
        }
        let rel = match self.inos.lock().unwrap().path_of(ino) {
            Some(r) => r,
            None => return reply.error(libc::ENOENT),
        };
        match self.attr_of(&rel) {
            Ok(attr) => reply.attr(&ttl(), &attr),
            Err(_) => reply.error(libc::ENOENT),
        }
    }

    fn mkdir(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let Some(rel) = self
            .inos
            .lock()
            .unwrap()
            .path_of(parent)
            .map(|p| p.join(name))
        else {
            return reply.error(libc::ENOENT);
        };
        // `.wh.`/`.cowt-copy-tmp.` are reserved namespaces; a user file
        // with those prefixes would be misread as a deletion marker or be
        // silently invisible at apply time — refuse, same as the winfsp
        // backend (round-21, round-40 review).
        if is_reserved_name(name) {
            return reply.error(libc::EPERM);
        }
        self.clear_whiteout(&rel);
        let dst = self.upper_of(&rel);
        if let Err(e) = fs::create_dir_all(&dst) {
            return reply.error(e.raw_os_error().unwrap_or(libc::EIO));
        }
        // Preserve the requested permission bits instead of the umask
        // default, so a private (0700) dir stays private (round-30).
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = fs::set_permissions(&dst, fs::Permissions::from_mode(mode & 0o7777)) {
            return reply.error(e.raw_os_error().unwrap_or(libc::EIO));
        }
        match self.attr_of(&rel) {
            Ok(attr) => reply.entry(&ttl(), &attr, 0),
            Err(_) => reply.error(libc::EIO),
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(rel) = self
            .inos
            .lock()
            .unwrap()
            .path_of(parent)
            .map(|p| p.join(name))
        else {
            return reply.error(libc::ENOENT);
        };
        match self.delete_merged(&rel) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    fn readlink(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyData) {
        // Round-27: symlinks were completely unusable in the view (fuser's
        // default readlink returns ENOSYS, so every path resolution through
        // a host symlink failed). Return the raw link target; the kernel
        // resolves relative targets against the mount root, which mirrors
        // the host tree at the same location, so semantics stay correct.
        let rel = match self.inos.lock().unwrap().path_of(ino) {
            Some(r) => r,
            None => return reply.error(libc::ENOENT),
        };
        let path = match self.resolve(&rel) {
            Some(p) => p,
            None => return reply.error(libc::ENOENT),
        };
        match fs::read_link(&path) {
            Ok(t) => {
                use std::os::unix::ffi::OsStrExt;
                reply.data(t.as_os_str().as_bytes());
            }
            Err(_) => reply.error(libc::EINVAL),
        }
    }

    fn symlink(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let Some(rel) = self
            .inos
            .lock()
            .unwrap()
            .path_of(parent)
            .map(|p| p.join(link_name))
        else {
            return reply.error(libc::ENOENT);
        };
        // `.wh.`/`.cowt-copy-tmp.` are reserved namespaces — refuse
        // (round-21, round-40 review).
        if is_reserved_name(link_name) {
            return reply.error(libc::EPERM);
        }
        self.clear_whiteout(&rel);
        let dst = self.upper_of(&rel);
        if let Some(p) = dst.parent() {
            if let Err(e) = fs::create_dir_all(p) {
                return reply.error(e.raw_os_error().unwrap_or(libc::EIO));
            }
        }
        if let Err(e) = std::os::unix::fs::symlink(target, &dst) {
            return reply.error(e.raw_os_error().unwrap_or(libc::EIO));
        }
        match self.attr_of(&rel) {
            Ok(attr) => reply.entry(&ttl(), &attr, 0),
            Err(_) => reply.error(libc::EIO),
        }
    }

    fn mknod(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        // Round-27: only plain files (S_IFREG) are created via mknod by
        // well-behaved tools (e.g. some POSIX wrappers). Devices/FIFOs are
        // not supported in the isolated layer — refuse loudly instead of
        // the fuser default ENOSYS.
        const S_IFMT: u32 = 0o170000;
        const S_IFREG: u32 = 0o100000;
        if mode & S_IFMT != S_IFREG {
            return reply.error(libc::EPERM);
        }
        let Some(rel) = self
            .inos
            .lock()
            .unwrap()
            .path_of(parent)
            .map(|p| p.join(name))
        else {
            return reply.error(libc::ENOENT);
        };
        // `.wh.`/`.cowt-copy-tmp.` are reserved namespaces; a user file
        // with those prefixes would be misread as a deletion marker or be
        // silently invisible at apply time — refuse (round-21, round-40
        // review: mknod was the one create path without the guard).
        if is_reserved_name(name) {
            return reply.error(libc::EPERM);
        }
        self.clear_whiteout(&rel);
        let dst = self.upper_of(&rel);
        if let Some(p) = dst.parent() {
            if let Err(e) = fs::create_dir_all(p) {
                return reply.error(e.raw_os_error().unwrap_or(libc::EIO));
            }
        }
        if let Err(e) = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&dst)
        {
            return reply.error(e.raw_os_error().unwrap_or(libc::EIO));
        }
        match self.attr_of(&rel) {
            Ok(attr) => reply.entry(&ttl(), &attr, 0),
            Err(_) => reply.error(libc::EIO),
        }
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        self.unlink(_req, parent, name, reply)
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        let (Some(src), Some(dst)) = (
            self.inos
                .lock()
                .unwrap()
                .path_of(parent)
                .map(|p| p.join(name)),
            self.inos
                .lock()
                .unwrap()
                .path_of(newparent)
                .map(|p| p.join(newname)),
        ) else {
            return reply.error(libc::ENOENT);
        };
        // Round-40 review: rename is a create path too — `mv x .wh.foo`
        // would seed user data into the whiteout namespace (deleted as a
        // marker at apply) or into the copy-tmp namespace (invisible).
        if is_reserved_name(newname) {
            return reply.error(libc::EPERM);
        }
        let lower_has = fs::symlink_metadata(self.lower_of(&src)).is_ok();
        let src_up = self.upper_of(&src);
        if lower_has && fs::symlink_metadata(&src_up).is_err() {
            if let Err(e) = self.copy_up_tree(&src) {
                return reply.error(e.raw_os_error().unwrap_or(libc::EIO));
            }
        }
        if fs::symlink_metadata(&src_up).is_err() {
            return reply.error(libc::ENOENT);
        }
        let dst_up = self.upper_of(&dst);
        if let Some(p) = dst_up.parent() {
            if let Err(e) = fs::create_dir_all(p) {
                return reply.error(e.raw_os_error().unwrap_or(libc::EIO));
            }
        }
        let _ = self.delete_merged(&dst); // replace-if-exists semantics
        if lower_has {
            // Whiteout BEFORE the rename: a crash between the steps leaves
            // either "rename done" or "not done" — never both trees.
            let name = match src.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => return reply.error(libc::EINVAL),
            };
            if let Err(e) = fs::write(
                src_up.with_file_name(format!("{WHITEOUT_PREFIX}{name}")),
                b"",
            ) {
                return reply.error(e.raw_os_error().unwrap_or(libc::EIO));
            }
        }
        if let Err(e) = fs::rename(&src_up, &dst_up) {
            return reply.error(e.raw_os_error().unwrap_or(libc::EIO));
        }
        reply.ok()
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        let Some(rel) = self.inos.lock().unwrap().path_of(ino) else {
            return reply.error(libc::ENOENT);
        };
        let wants_write =
            flags & (libc::O_WRONLY | libc::O_RDWR | libc::O_TRUNC | libc::O_APPEND) != 0;
        let path = match self.resolve(&rel) {
            Some(p) => p,
            None => return reply.error(libc::ENOENT),
        };
        let path = if wants_write && path.starts_with(&self.lower) {
            match self.copy_up(&rel) {
                Ok(p) => p,
                Err(e) => return reply.error(e.raw_os_error().unwrap_or(libc::EIO)),
            }
        } else {
            path
        };
        let mut opts = fs::OpenOptions::new();
        opts.read(true).write(wants_write);
        let f = match opts.open(&path) {
            Ok(f) => f,
            Err(e) => return reply.error(e.raw_os_error().unwrap_or(libc::EIO)),
        };
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed) + 2;
        self.fhs.lock().unwrap().insert(fh, Handle::File(f));
        reply.opened(fh, 0);
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let lock = self.fhs.lock().unwrap();
        let Some(Handle::File(f)) = lock.get(&fh) else {
            return reply.error(libc::EBADF);
        };
        let mut buf = vec![0u8; size as usize];
        match f.read_at(&mut buf, offset.max(0) as u64) {
            Ok(n) => {
                buf.truncate(n);
                reply.data(&buf);
            }
            Err(e) => reply.error(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let lock = self.fhs.lock().unwrap();
        let Some(Handle::File(f)) = lock.get(&fh) else {
            return reply.error(libc::EBADF);
        };
        match f.write_at(data, offset.max(0) as u64) {
            Ok(n) => reply.written(n as u32),
            Err(e) => reply.error(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    fn flush(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _lock_owner: u64,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.fhs.lock().unwrap().remove(&fh);
        reply.ok();
    }

    fn fsync(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        let synced = self.fhs.lock().unwrap().get(&fh).and_then(|h| match h {
            Handle::File(f) => Some(f.sync_all()),
            Handle::Dir => None,
        });
        match synced {
            Some(Ok(())) => reply.ok(),
            Some(Err(e)) => reply.error(e.raw_os_error().unwrap_or(libc::EIO)),
            None => reply.ok(),
        }
    }

    fn opendir(&mut self, _req: &Request<'_>, _ino: u64, _flags: i32, reply: ReplyOpen) {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed) + 2;
        self.fhs.lock().unwrap().insert(fh, Handle::Dir);
        reply.opened(fh, 0);
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let Some(rel) = self.inos.lock().unwrap().path_of(ino) else {
            return reply.error(libc::ENOENT);
        };
        let entries = self.merged_dir_entries(&rel);
        let mut table = self.inos.lock().unwrap();
        for (i, name) in entries.iter().enumerate() {
            let idx = i as i64 + 1;
            if idx <= offset {
                continue;
            }
            let child = rel.join(name);
            let kind = fs::symlink_metadata(self.resolve(&child).unwrap_or_default())
                .map(|m| {
                    if m.is_dir() {
                        FileType::Directory
                    } else {
                        FileType::RegularFile
                    }
                })
                .unwrap_or(FileType::RegularFile);
            let ino = table.alloc(child);
            if reply.add(ino, idx, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        reply: ReplyEmpty,
    ) {
        self.fhs.lock().unwrap().remove(&fh);
        reply.ok();
    }

    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        // Best effort: report a fixed 1 TiB volume. Real sizing would need
        // statvfs FFI (libc::statvfs takes a C path); not worth the surface
        // for the MVP — nothing in cowt's E2E depends on exact numbers.
        reply.statfs(1 << 30, 1 << 29, 1 << 29, 1 << 20, 1 << 20, 4096, 255, 4096);
    }

    fn access(&mut self, _req: &Request<'_>, _ino: u64, _mask: i32, reply: ReplyEmpty) {
        reply.ok();
    }

    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let Some(rel) = self
            .inos
            .lock()
            .unwrap()
            .path_of(parent)
            .map(|p| p.join(name))
        else {
            return reply.error(libc::ENOENT);
        };
        // `.wh.`/`.cowt-copy-tmp.` are reserved namespaces; a user file
        // with those prefixes would be misread as a deletion marker or be
        // silently invisible at apply time — refuse, same as the winfsp
        // backend (round-21, round-40 review).
        if is_reserved_name(name) {
            return reply.error(libc::EPERM);
        }
        self.clear_whiteout(&rel);
        let dst = self.upper_of(&rel);
        if let Some(p) = dst.parent() {
            if let Err(e) = fs::create_dir_all(p) {
                return reply.error(e.raw_os_error().unwrap_or(libc::EIO));
            }
        }
        let f = match fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(flags & libc::O_TRUNC != 0)
            .open(&dst)
        {
            Ok(f) => f,
            Err(e) => return reply.error(e.raw_os_error().unwrap_or(libc::EIO)),
        };
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed) + 2;
        self.fhs.lock().unwrap().insert(fh, Handle::File(f));
        match self.attr_of(&rel) {
            Ok(attr) => reply.created(&ttl(), &attr, 0, fh, 0),
            Err(_) => reply.error(libc::EIO),
        }
    }
}

// ------------------------------------------------------------ backend glue ==

/// Probe: mount a throwaway filesystem, then tear it down. The session is
/// dropped (unmounting the probe) *before* the temp dir is removed — the
/// mountpoint stays busy while the session is alive.
fn probe() -> anyhow::Result<()> {
    let probe = std::env::temp_dir().join(format!("cowt-fuse-probe-{}", std::process::id()));
    let (lower, upper, mountpoint) = (probe.join("l"), probe.join("u"), probe.join("m"));
    for d in [&lower, &upper, &mountpoint] {
        fs::create_dir_all(d).with_context(|| format!("create probe dir {}", d.display()))?;
    }
    let session = match mount_cow(&lower, &upper, &mountpoint, "cowt-probe") {
        Ok(s) => s,
        Err(e) => {
            let _ = fs::remove_dir_all(&probe);
            return Err(e);
        }
    };
    drop(session); // unmount the probe filesystem
                   // Retry briefly: unmount may take a moment to release the mountpoint.
    for _ in 0..50 {
        if fs::remove_dir_all(&probe).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = fs::remove_dir_all(&probe);
    Ok(())
}

/// Mount the CoW filesystem at `mountpoint` and return a background session.
fn mount_cow(
    lower: &Path,
    upper: &Path,
    mountpoint: &Path,
    fsname: &str,
) -> Result<BackgroundSession> {
    let fs = CowFs {
        lower: lower.to_path_buf(),
        upper: upper.to_path_buf(),
        inos: Mutex::new(InoTable::new()),
        fhs: Mutex::new(HashMap::new()),
        next_fh: AtomicU64::new(0),
    };
    let options = vec![
        MountOption::FSName(fsname.to_string()),
        MountOption::AutoUnmount,
        MountOption::DefaultPermissions,
    ];
    // FUSE-T mounts over NFS, which is asynchronous: Session::new returns
    // before the mount is actually reachable. Poll `mount` output until the
    // mountpoint shows up.
    let session = Session::new(fs, mountpoint, &options)
        .with_context(|| format!("mount FUSE-T filesystem at {}", mountpoint.display()))?;
    let background = BackgroundSession::new(session).context("start FUSE-T session")?;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if is_mounted_now(mountpoint) {
            break;
        }
        if std::time::Instant::now() > deadline {
            let out = std::process::Command::new("mount").output();
            let mounts = out
                .as_ref()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default();
            let err = out
                .as_ref()
                .map(|o| String::from_utf8_lossy(&o.stderr).into_owned())
                .unwrap_or_default();
            bail!(
                "FUSE-T mount at {} never became reachable; \
                 mount stdout: [{mounts}] stderr: [{err}]",
                mountpoint.display()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(background)
}

/// Is `mountpoint` listed by `mount` right now?
fn is_mounted_now(mountpoint: &Path) -> bool {
    std::process::Command::new("mount")
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout).contains(&format!(" on {} ", mountpoint.display()))
        })
        .unwrap_or(false)
}

impl Backend for FuseT {
    fn name(&self) -> &'static str {
        "fuse-t"
    }

    fn available(&self) -> Result<()> {
        probe().map_err(|e| {
            anyhow::anyhow!(
                "FUSE-T is not usable: {e:#} \
                 (install it with `brew install macos-fuse-t/homebrew-cask/fuse-t`; \
                 see scripts/macos/install-fuse-t.sh)"
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

        // Stale state from a hard-killed `cowt run`: restore it first.
        if fs::symlink_metadata(mountpoint)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            if layout.real.exists() {
                eprintln!(
                    "cowt: recovering stale mount state at {}",
                    mountpoint.display()
                );
                restore(mountpoint, &layout)?;
            } else {
                bail!(
                    "{} is a symlink but no moved-aside directory was found; \
                     refusing to touch a foreign symlink",
                    mountpoint.display()
                );
            }
        }
        let _ = fs::remove_dir_all(&layout.view);

        // Move the host dir aside, point the original path at the view.
        fs::rename(mountpoint, &layout.real).with_context(|| {
            format!(
                "move {} aside to {}",
                mountpoint.display(),
                layout.real.display()
            )
        })?;
        let result = (|| -> Result<MountGuard> {
            fs::create_dir_all(&layout.view)
                .with_context(|| format!("create view dir {}", layout.view.display()))?;
            std::os::unix::fs::symlink(&layout.view, mountpoint).with_context(|| {
                format!(
                    "create symlink {} -> {}",
                    mountpoint.display(),
                    layout.view.display()
                )
            })?;
            let session = mount_cow(&layout.real, upper, &layout.view, "cowt")?;
            eprintln!(
                "cowt: FUSE-T mounted at {} (upper: {}, host dir moved to {})",
                mountpoint.display(),
                upper.display(),
                layout.real.display()
            );
            Ok(MountGuard::with_session(mountpoint.to_path_buf(), session))
        })();
        if result.is_err() {
            // Roll back: drop the symlink and move the host dir back.
            // Surface restore failure — a stranded `real` is user data.
            if fs::symlink_metadata(mountpoint)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false)
            {
                let _ = fs::remove_file(mountpoint);
            }
            if let Err(e) = restore(mountpoint, &layout) {
                eprintln!(
                    "cowt: warning: restore of the host directory failed: {e:#} — \
                     it is still at {}",
                    layout.real.display()
                );
            }
        }
        result
    }

    fn unmount(&self, mountpoint: &Path) -> Result<()> {
        // Idempotent: works from a fresh process (`cowt drop --force`) or
        // from the owning `cowt run`. The FUSE session dies with its host
        // process; here we only undo the symlink.
        let state = match fs::read_link(mountpoint) {
            Ok(target) => target,
            Err(_) => {
                // Round-36: kill -9 between rename(target->real) and the
                // symlink creation strands `real` with no mountpoint
                // symlink. Find the state dir whose meta.target is this
                // mountpoint and restore the host dir.
                if let Some(dir) = find_state_for(mountpoint) {
                    let layout = Layout {
                        real: dir.join("real"),
                        view: dir.join("view"),
                    };
                    restore(mountpoint, &layout)?;
                }
                return Ok(());
            }
        };
        // The symlink target must be a cowt VIEW: basename "view" whose
        // parent is a worktree state dir INSIDE the cowt state root. A
        // foreign symlink (user data dir whose basename happens to be
        // "view") must never be deleted, even with a stale pidfile
        // authorizing the drop (round-31).
        let state_root = crate::state::State::open()?.root().to_path_buf();
        let parent = state.parent().unwrap_or_else(|| Path::new(""));
        let is_cowt_view = state.file_name().map(|n| n == "view").unwrap_or(false)
            && parent.starts_with(&state_root)
            && (parent.join("meta.json").is_file()
                || parent.join("manifest.json").is_file()
                || parent.join("upper").is_dir());
        if !is_cowt_view {
            bail!(
                "symlink at {} points at {} (not a cowt view under the state root); \
                 refusing to touch it",
                mountpoint.display(),
                state.display()
            );
        }
        let layout = Layout {
            real: parent.join("real"),
            view: state.clone(),
        };
        // The state dir was deleted from under the running worktree
        // (external `rm -rf state/<id>`): the moved-aside host dir is gone
        // with it. Never silent.
        if !layout.real.exists() {
            eprintln!(
                "cowt: ERROR: worktree state was deleted while running; \
                 the host directory at {} may have been lost",
                mountpoint.display()
            );
        }
        let _ = fs::remove_dir_all(&state);
        fs::remove_file(mountpoint)
            .with_context(|| format!("remove symlink at {}", mountpoint.display()))?;
        restore(mountpoint, &layout)
    }

    fn is_mounted(&self, mountpoint: &Path) -> bool {
        // Round-36: a kill -9 between rename(target->real) and the symlink
        // creation strands `real` with NO mountpoint symlink — the symlink
        // check alone misses it and drop --force hits the real guard. A
        // state dir with meta.target == mountpoint and a `real` present is
        // as much evidence of an in-flight mount as the symlink (WinFsp
        // already checks both).
        if fs::symlink_metadata(mountpoint)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return true;
        }
        find_state_for(mountpoint)
            .map(|dir| dir.join("real").exists())
            .unwrap_or(false)
    }
}

/// Paths derived from the worktree state dir (upper's parent): the moved-aside
/// host dir (`real`) and the FUSE mountpoint (`view`).
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

/// Find the state dir whose meta.json names `mountpoint` as its target.
/// Used when the mountpoint symlink is missing (kill -9 between the
/// rename(target->real) and the symlink creation, round-36) — the state
/// dir is the only remaining link between the stranded `real` and the host
/// path.
fn find_state_for(mountpoint: &Path) -> Option<PathBuf> {
    let root = crate::state::State::open().ok()?.root().to_path_buf();
    let rd = std::fs::read_dir(&root).ok()?;
    for e in rd.flatten() {
        let dir = e.path();
        let Ok(meta) = crate::state::State::load_meta(&dir) else {
            continue;
        };
        if meta.target == mountpoint {
            return Some(dir);
        }
    }
    None
}

/// Restore the host directory: move `state/real` back to `mountpoint`.
fn restore(mountpoint: &Path, layout: &Layout) -> Result<()> {
    if !layout.real.exists() {
        return Ok(()); // already restored
    }
    if let Ok(m) = fs::symlink_metadata(mountpoint) {
        if m.is_dir() {
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
