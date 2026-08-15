# Changelog

All notable changes to co-worktree are documented here.

## [Unreleased]

### Added — cross-platform support

- **macOS backend**: FUSE-T userspace CoW filesystem (`fuser` bindings, NFS-
  based, kext-less, no root). The host directory is moved to `state/<id>/real`
  while running and the original path becomes a symlink to the mounted view.
  Deletions land as `.wh.` whiteouts, same encoding as Linux/Windows.
- **Windows backend**: WinFsp userspace CoW filesystem. While a worktree
  runs, the host directory is moved to `state/<id>/real` and WinFsp mounts
  directly onto the original path; no admin needed. Copy-up on write-open,
  `.wh.` whiteouts on deletion, cross-layer rename, case-insensitive
  whiteout matching. Stale mounts from hard-killed runs are auto-recovered
  by the next `run` / `diff` / `apply` / `drop --force`.
- **GitHub Actions E2E on all three platforms**: real-backend acceptance
  suite (`crates/cowt/tests/e2e.rs`, run as root on Linux/macOS, with WinFsp
  on Windows) covering fork/run/diff/apply/drop, three-way conflicts, crash
  survival, performance budgets, concurrent-run refusal, foreign-mount
  refusal and crash-recovery-on-next-run. `scripts/e2e.sh` was replaced by
  the portable Rust suite.
- **Docs (English-first)**: `README.md` (English) is now the primary
  document; the Chinese version moved to `README.zh-CN.md`.

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
- macOS/Windows backends: unified whiteout semantics — resolve now checks
  upper → whiteout → lower (an upper entry wins over its own whiteout),
  merged listings hide whiteout victims, `create`/`mkdir` clear stale
  whiteouts (delete-then-recreate round-trip, covered by a new E2E test).
- `flush` no longer calls `sync_all()` on read-only WinFsp handles
  (FlushFileBuffers denies on read-only handles).
- Windows `pid_alive` distinguishes `ERROR_ACCESS_DENIED` (protected
  process, alive) from `ERROR_INVALID_PARAMETER` (no such pid).
- `cowt drop --force` only unmounts mounts proven to be our own stale
  leftovers; foreign mounts are refused even with `--force`.
- macOS: the recycled-pid guard (`drop --force` must not kill an innocent
  process whose pid was reused after a crash) now works — the pidfile
  starttime comes from `proc_pidinfo(PROC_PIDT_SHORTBSDINFO)`, with the FFI
  layout pinned by a size/offset unit test.

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
