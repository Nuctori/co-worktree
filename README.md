# co-worktree

**Git-worktree-style isolation, review and merge for any application's config and data directory.**

Run a program in its normal environment while every file/config side effect is
transparently redirected into an isolated layer; review the changes, merge them
back selectively, or discard them with one command. **Not a container, not a
VM, not a security sandbox.**

![CI](https://github.com/Nuctori/co-worktree/actions/workflows/ci.yml/badge.svg)

> 中文版文档见 [README.zh-CN 中文](README.zh-CN.md).

## Features

- **Isolation without copies**: `fork` takes a metadata snapshot (paths +
  BLAKE3 hashes) — no files are copied. `run` mounts a copy-on-write view over
  the original path; the program reads through to the host and writes only
  into the isolated layer.
- **Reviewable changes**: `diff` reports file-level, Myers line-level, and
  JSON/YAML key-level changes against the fork snapshot.
- **Safe merge-back**: `apply` performs a three-way merge (base ⊗ current ⊗
  worktree) and writes nothing on conflict. `drop` unmounts and deletes the
  layer, leaving zero residue on the host.
- **Cross-platform**: Linux (kernel overlayfs, root or userns), macOS (FUSE-T,
  kext-less, no root), Windows (WinFsp, no admin) — one binary, backend
  auto-detected at runtime, shared whiteout encoding on all three.
- **Crash recovery**: a hard-killed `cowt run` (kill -9, power loss) is
  recognized by the next `run` / `diff` / `apply` / `drop --force` via the
  stale pidfile — and only when the mount is provably our own leftover;
  foreign mounts are never unmounted.
- **Zero daemons, zero network, fully offline.**

## Install

```sh
# Linux: only runtime dependency is fuse-overlayfs (userspace, no kernel module)
sudo apt-get install fuse-overlayfs   # Debian/Ubuntu
sudo dnf install fuse-overlayfs       # Fedora

cargo install --path crates/cowt --bin cowt   # or grab a binary from Releases
```

macOS and Windows:

```sh
# macOS: FUSE-T (kext-less FUSE via NFS) — no kernel extension, no root
cargo install --path crates/cowt --bin cowt
bash scripts/macos/install-fuse-t.sh          # one-time: install FUSE-T + link libfuse
cowt run vscode -- code

# Windows: WinFsp (signed kernel driver + userspace DLL)
winget install --id WinFsp.WinFsp   # or: choco install winfsp / winfsp.dev
cowt run vscode -- code             # no admin needed
```

`cowt doctor` reports backend availability on any platform.

## Quick start

```sh
# 1. Fork: isolate VS Code's config directory (metadata snapshot only)
cowt fork ~/.config/Code --name vscode

# 2. Run: VS Code sees its normal path, writes land in the isolated layer
cowt run vscode -- code

# 3. Diff: review what the layer changed
cowt diff vscode              # file-level
cowt diff vscode --content    # + Myers line diff and JSON/YAML key diff
cowt diff vscode --json       # machine-readable

# 4a. Apply: three-way merge back; conflict-free or nothing is written
cowt apply vscode --dry-run   # preview operations and conflicts
cowt apply vscode

# 4b. Drop: discard the layer, host untouched
cowt drop vscode              # refuses while a process is running; --force kills first
```

## How it works

```
┌───────────────── host dir ~/.config/Code ─────────────────┐
│ fork  → base manifest (paths + BLAKE3 hashes, metadata)    │
│ run   → merged view mounted over the original path         │
│          reads  → pass through to lower (host dir)         │
│          writes → redirected to upper (isolated layer)     │
│ diff  → base manifest ⊗ upper → added / modified / deleted │
│ apply → three-way merge base ⊗ current ⊗ worktree          │
│          base==current and worktree changed → apply        │
│          all three differ → conflict, zero writes          │
│ drop  → unmount + atomic deletion of the layer             │
└────────────────────────────────────────────────────────────┘
```

Deletions become **whiteouts** in the layer (kernel-style char device 0:0
carrying the victim's name, or zero-size `.wh.`-prefixed files), so renames,
delete-then-recreate, and case-insensitive lookups behave identically on every
backend.

## Platform support

| Platform | Backend | Requirements | Verified |
| --- | --- | --- | --- |
| **Linux** | kernel overlayfs (root) / overlayfs+userns (non-root) / fuse-overlayfs (fallback) | fuse-overlayfs package | ✅ full E2E in CI + real machine |
| **Windows** | WinFsp userspace CoW filesystem (`winfsp` bindings) | WinFsp (winget / choco / winfsp.dev) | ✅ full E2E in CI + real machine |
| **macOS** | FUSE-T userspace CoW filesystem (`fuser` bindings, NFS-based, kext-less, no root) | FUSE-T (`scripts/macos/install-fuse-t.sh`) | ✅ core-logic E2E in CI — mount cases auto-skip on headless runners (FUSE-T NFS mounts do not activate there; environment limit, not a code issue). Mount path awaits a real-machine run |

While running, the macOS/Windows backends move the host directory to
`state/<id>/real` and mount the view over the original path (symlink on macOS,
WinFsp direct mount on Windows).

CI baseline: **12/12 green** — rustfmt, clippy `-D warnings`, unit +
integration tests on all three platforms, real-backend E2E on all three,
Windows cross-compile check, release builds for all three.

## Safety boundaries

- **Not a sandbox**: no CPU, memory, network, or IPC limits; not malware
  protection — side-effect isolation only
- **User-level directories only**: paths outside `$HOME` are refused by
  default (`--force-path` overrides)
- **Windows registry**: not isolated (MVP: file-level config only)
- **Symlinks are not isolated**: a link inside the forked directory is
  followed by the merged view — writes through it reach the host target
  directly and are invisible to `cowt diff`. `fork` prints a warning when
  it detects symlinks. (The Windows backend contains the write via
  copy-up — junction targets are copied, not followed; on Unix and macOS
  the kernel resolves the link at the VFS boundary, same as Linux.)
- **`apply` is conflict-gated, not transactional**: per-file renames are
  atomic, but a multi-file apply interrupted mid-way (crash, power loss)
  leaves the already-written files in place — each file is individually
  consistent, no partial file bodies. Apply refuses while the worktree is
  running (checked both before and after planning).
- **Windows same-volume limit**: the state dir (`COWT_HOME`, default
  `%LOCALAPPDATA%\cowt`) must be on the same volume as the target (Windows
  cannot rename across volumes); `cowt run` reports a clear error otherwise
- **Windows: `std::fs::remove_dir_all` fails on the view** with
  `ERROR_INVALID_NAME` (std opens dirs with `FILE_OPEN_REPARSE_POINT`, which
  WinFsp rejects on non-reparse paths). Real-world deletion via
  cmd/explorer/PowerShell works normally; Rust programs should delete
  directory trees by enumerating and removing entries individually.
- **Windows 8.3 short names**: operations through a 8.3 alias
  (`REALLYL~1.TXT`) are not tracked (the manifest keys the long name);
  whiteouts written under the short spelling are ignored. Modern APIs use
  long names, so this only affects paths typed by hand.
- **`..` past the mount boundary leaves the layer** (all platforms): the
  kernel resolves `mount/..` to the host parent dir, so a traversal write
  bypasses isolation — the same class as a program writing any absolute
  path outside. Not a sandbox; `upper` is never polluted, so diff/apply
  stay truthful.
- **Windows: apply to a read-only host file fails** with a generic error
  (delete-then-rename cannot replace a READONLY-attribute file; clear the
  attribute first). Unix `rename(2)` replaces it atomically.
- **Linux userns mode**: a whole-directory rename through the view is not
  materialized into upper before teardown (the mount lives in a private
  namespace) — diff/apply then see the renamed dir without its children.
  Root/fuse modes materialize it; run as root or use `--force-path`-free
  setups where kernel-direct is available.
- **macOS case-sensitive APFS**: whiteout shadowing matches
  case-insensitively (safe default for the common case-insensitive APFS),
  so deleting `Foo.txt` on a case-sensitive volume also hides a distinct
  lower `foo.txt`.
- **JSON paths use native separators** (`\` on Windows, `/` elsewhere);
  scripts parsing `diff --json` / `apply --dry-run --json` should normalize.

## Performance

| Metric | Budget | Measured |
| --- | --- | --- |
| empty worktree fork | < 500ms | ~5ms |
| 10k-file manifest scan | 10k+ files | ~215ms |
| sequential write overhead | < 20% | kernel-overlay ~9% (CI); fuse-overlayfs ~4–7% |
| 10k-file diff | < 3s | ~20–200ms |

## Development

```sh
cargo test --workspace                                  # unit + CLI integration tests
cargo test --test e2e -- --ignored --test-threads=1     # real-backend E2E (mount cases auto-skip on headless environments)
cargo clippy --workspace --all-targets -- -D warnings
```

Structure: `cowt-core` (pure, cross-platform manifest/diff/merge) + `cowt`
(CLI + platform backends). The full test matrix runs on GitHub Actions on
every push.

## License

MIT. The Windows backend links WinFsp through the GPL-3.0
`winfsp`/`winfsp-sys` bindings. WinFsp itself is GPLv3 with a FLOSS exception
(explicitly permitting FLOSS projects to link its DLL), so this project's MIT
license is unaffected; the binding layer ships with the Windows binary under
GPL-3.0.
