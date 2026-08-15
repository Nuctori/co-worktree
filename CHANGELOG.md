# Changelog

All notable changes to co-worktree are documented here.

## [Unreleased]

### Added — cross-platform support

- **macOS backend**: kernel union mount (`mount -t union`, BSD-style). No
  third-party driver (macFUSE kexts cannot be approved headlessly on CI
  runners). Requires root; `cowt doctor` probes availability with a real
  mount. Deletions land as `.wh.` whiteouts, same encoding as Linux.
- **Windows backend**: WinFsp userspace filesystem with copy-on-write. While
  a worktree runs, the host directory is moved to `state/<id>/real` and the
  original path becomes a junction to the WinFsp view (`state/<id>/view`);
  junctions need no privileges. Copy-up on write-open, `.wh.` whiteouts on
  deletion, cross-layer rename. Stale junctions from hard-killed runs are
  auto-recovered by the next `run` / `diff` / `apply` / `drop --force`.
- **GitHub Actions E2E on all three platforms**: real-backend acceptance
  suite (`crates/cowt/tests/e2e.rs`, run as root on Linux/macOS, with WinFsp
  on Windows) covering fork/run/diff/apply/drop, three-way conflicts, crash
  survival, performance budgets, concurrent-run refusal, foreign-mount
  refusal and crash-recovery-on-next-run. `scripts/e2e.sh` was replaced by
  the portable Rust suite.
- **English documentation**: `README.en.md` (Chinese original remains
  `README.md`).

### Changed

- `cowt run` / `cowt diff` / `cowt apply` now recover stale mounts
  (crashed-run leftovers) automatically — only when the worktree's own
  pidfile proves the previous run died. Foreign mounts are always refused.
- Portable pid liveness (`kill -0` on unix, `OpenProcess` on Windows) and
  per-platform default state dirs (`~/.local/state/cowt` / `%LOCALAPPDATA%\cowt`).
- Merge commit is no longer atomic on Windows (destination is removed before
  rename); the staging phase still guarantees zero pollution on failure.
- Rust MSRV raised to 1.87 (winfsp bindings require edition 2024).

### Fixed

- `File::sync_all()` on a read-only handle failed on Windows (access
  denied); the staged-file fsync now opens with write access.
- Windows: `\\?\` canonical path prefix broke the `$HOME` boundary check.
- Windows: `.wh.` whiteout detection (zero-size files, no char devices).

### Known limitations

- macOS: Apple's file APIs (Finder etc.) handle FUSE mounts poorly; POSIX
  programs work normally. FUSE-T's NFS mount does not come up on headless
  GitHub Actions runners (fuse_mount returns but the mount never appears in
  `mount(8)`); the CI E2E runs the core suite there and skips mount tests
  with an explicit note — verify mounting on a real Mac.
- Windows: userspace I/O write overhead (E2E budget 3× native vs 1.2× on
  kernel backends); state dir and target must be on the same volume; no
  symlink semantics; registry is not isolated.
- The Windows binary links GPL-3.0 `winfsp` bindings (WinFsp itself is GPLv3
  with a FLOSS exception; see README License section). The macOS binary
  links FUSE-T's libfuse-t.dylib (must be installed).
- `winfsp-sys` is vendored under `third_party/` with pre-generated bindings
  (bindgen cross-compiling to Windows on Linux hosts is unworkable); update
  it manually when bumping the WinFsp API version.
