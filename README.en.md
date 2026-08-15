# co-worktree

**Git-worktree-style isolation, review and merge — for any application's
config and data directories.**

Run a program in its normal environment while every file/config side effect is
transparently redirected into an isolated layer; review the changes, merge them
back selectively, or discard them with one command. **Not a container, not a
VM, not a security sandbox.**

> 中文版文档见 [README.zh-CN 中文](README.md)。

## Install

```sh
# Linux: the only runtime dependency is fuse-overlayfs (userspace, no kernel module)
sudo apt-get install fuse-overlayfs   # Debian/Ubuntu
sudo dnf install fuse-overlayfs       # Fedora

cargo install --path crates/cowt --bin cowt   # or grab a binary from Releases
```

macOS and Windows:

```sh
# macOS: kernel union mount (BSD-style), no third-party driver; mounting needs root
cargo install --path crates/cowt --bin cowt   # or download from Releases
sudo cowt run vscode -- code        # run needs sudo (mount is privileged)

# Windows: requires WinFsp (signed kernel driver + userspace DLL)
choco install winfsp                  # or install from https://winfsp.dev
cowt run vscode -- code              # no admin needed (junctions are unprivileged)
```

`cowt doctor` reports backend availability on any platform.

## Quick start

```sh
# 1. Fork: create an isolated worktree over a config dir (metadata snapshot only)
cowt fork ~/.config/Code --name vscode

# 2. Run: execute the program in a virtual merged view; writes land in the isolated layer
cowt run vscode -- code

# 3. Diff: review changes of the isolated layer vs the fork snapshot
cowt diff vscode
cowt diff vscode --content        # Myers line diff + key-level diff for JSON/YAML
cowt diff vscode --json           # machine-readable

# 4a. Apply: three-way merge (base / current / worktree) back into the host
cowt apply vscode --dry-run       # preview operations and conflicts
cowt apply vscode                 # writes only when conflict-free; zero pollution on conflict

# 4b. Drop: discard the worktree; zero residue on the host
cowt drop vscode                  # refuses while a process is running; --force kills and cleans
```

## How it works

```
┌───────────────────── host dir ~/.config/Code ──────────────────────┐
│  fork:  base manifest (paths + BLAKE3 hashes, metadata only)        │
│  run:   merged view mounted over the original path                   │
│           read  → pass through to lower (host dir)                   │
│           write → redirected to upper (~/.local/state/cowt/<id>/upper)│
│  diff:  base manifest ⊗ upper → added / modified / deleted          │
│  apply: three-way merge base ⊗ current ⊗ worktree                   │
│          base==current and worktree changed → apply                  │
│          all three differ → conflict, nothing written, report        │
│  drop:  unmount + atomic deletion of the isolated layer              │
└────────────────────────────────────────────────────────────────────┘
```

## Architecture

| Layer | Contents |
| --- | --- |
| `cowt-core` | Pure Rust, cross-platform: Manifest (parallel BLAKE3 scan), Diff (Myers line-level / JSON·YAML key-level), three-way Merge (staging + atomic rename commit) |
| `cowt` CLI | Platform backend trait: Linux overlayfs (3 modes), macOS kernel union mount, Windows WinFsp userspace filesystem (below) |

### Platform backends

| Platform | Backend | Requirements | Notes |
| --- | --- | --- | --- |
| Linux | kernel overlayfs (root) / kernel overlayfs+userns (non-root) / fuse-overlayfs (fallback) | fuse-overlayfs package | Auto-detected at runtime; deletions become whiteouts (both encodings parsed: char dev 0:0 under the original name, and zero-size `.wh.`-prefixed files) |
| macOS | kernel union mount (`mount -t union`, BSD-style) | root | No third-party driver (macFUSE kexts cannot be approved headlessly on CI runners); deletions become `.wh.` whiteouts; Apple's file APIs (Finder etc.) handle unions poorly — POSIX programs work fine |
| Windows | WinFsp userspace CoW filesystem (`winfsp` bindings) | WinFsp installed (choco / winfsp.dev) | While running, the host dir is moved to `state/<id>/real` and the original path becomes a junction → the WinFsp view; junctions need no privileges; deletions become `.wh.` whiteouts; userspace I/O makes the write path slower than kernel backends |

Probing is automatic, no configuration needed; `cowt doctor` shows the active
backend. All three backends share the same whiteout encoding, so diff / merge /
apply behave identically everywhere.

Design decisions: synchronous I/O, no async runtime (the FUSE callback model is
synchronous); zero network services, zero container runtime, fully offline;
the MVP does not isolate the Windows registry (modern apps store config in
files); the Windows backend does not implement symlink semantics (junctions are
treated as ordinary directory entries).

## Boundary declarations

- **No runtime isolation**: CPU, memory, network and IPC are not limited
- **Not malware protection**: prevents side-effect pollution only, not
  privilege escalation / kernel exploits / driver injection
- **User-level directories only**: paths outside `$HOME` are refused by
  default (`--force-path` overrides)
- **Windows registry**: not isolated in the MVP — file-level config only
- **Crash recovery**: if `cowt run` is hard-killed (kill -9, power loss), the
  leftover mount and pidfile are automatically recognized and restored by the
  next `cowt run` / `cowt diff` / `cowt apply` / `cowt drop --force` — cleanup
  happens only when the worktree's own pidfile has gone stale; foreign mounts
  are always refused, never unmounted
- **Windows same-volume limit**: the state dir (`COWT_HOME`, default
  `%LOCALAPPDATA%\cowt`) must sit on the same volume as the target app dir
  (Windows cannot rename across volumes); `cowt run` reports a clear error
  otherwise

## Performance (acceptance criteria + measurements)

| Metric | Budget | Measured |
| --- | --- | --- |
| empty worktree fork | < 500ms | ~5ms |
| manifest scan | 10k+ files supported | ~215ms for 10k files |
| sequential write overhead | < 20% | kernel-overlay ~9% (CI); fuse-overlayfs ~4–7% on a regular SSD |
| 10k-file diff | < 3s | ~20–200ms |

## Development & verification

```sh
cargo test --workspace                # unit + CLI integration tests (real mounts when the backend is available)
cargo test --test e2e -- --ignored    # end-to-end acceptance (root on Linux/macOS, WinFsp on Windows)
```

CI (GitHub Actions): rustfmt, clippy `-D warnings`, full tests on all three
platforms, real-backend E2E on all three (Linux kernel-overlay / macOS union /
Windows WinFsp), Windows/macOS cross-compile checks, and release artifacts for
all three platforms.

## License

MIT. The Windows backend links WinFsp through the GPL-3.0 `winfsp`/`winfsp-sys`
bindings. WinFsp itself is GPLv3 with a FLOSS exception (explicitly permitting
FLOSS projects to link its DLL), so this project's MIT license is unaffected;
the binding layer ships with the Windows binary under GPL-3.0.
