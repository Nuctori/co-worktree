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

### Added — round 21 (CRLF / line-ending / empty-file boundaries)

- A non-empty `.wh.*` file in the upper layer is now treated as a plain user
  file: it stays visible in `diff` and is written back by `apply` (previously
  every `.wh.`-prefixed name was skipped, silently dropping real changes).
- WinFsp / FUSE-T `create`+`mkdir` refuse `.wh.`-prefixed names (the reserved
  deletion-marker namespace): a user-created 0-byte `.wh.notes.txt` can no
  longer be mistaken for a deletion marker and delete an untouched host file
  on `apply` (new E2E test on Windows).
- Old-Mac lone-`\r` line endings no longer glue unified-diff lines together
  (a deleted line hidden by the carriage-return overwrite); `\r` is treated
  as a line terminator, CRLF untouched.
- Corrupted manifests with an empty or truncated file hash are now rejected
  loudly (`CorruptManifest`) instead of producing phantom diffs/conflicts.
- Regression locks: CRLF minimal hunks, LF↔CRLF conversion, trailing-newline
  marker, BOM, mixed endings, and 0-byte-file boundaries across
  scan/diff/merge/apply.

### Added — round 22 (CLI arg boundaries / exit codes / usage)

- `fork --name` now validates names with the same rule the resolver uses:
  empty names, separators and `..` components are rejected at creation, so
  the tool can no longer create a worktree it cannot resolve by name.
- A worktree name can no longer shadow an existing worktree *id* (`fork
  --name <existing-id>` is refused) — previously `drop <that name>` would
  have hit the wrong worktree.
- `resolve(".")` is rejected exactly like `".."` (it would otherwise resolve
  to the state root, dangerous under a misconfigured `COWT_HOME`).
- `cowt run` exits non-zero when it detects the host directory was NOT
  restored (state/`<id>`/`real` residue) even if the child exited 0 — the
  damaged state is now observable to scripts.
- A child killed by a signal (unix) is reported as "killed by signal N" with
  the conventional 128+N exit code, instead of a misleading "exited with
  code 1".
- Regression locks: clap parse boundaries exit 2 (no subcommand, unknown
  subcommand/flag, extra positional, missing cmd, duplicate flag) and
  `--help`/`--version` exit 0.
- Note: `cowt doctor` intentionally exits 0 even when no backend is
  available (it is a report command; CI and scripts parse its stdout) —
  that contract is pinned by the E2E suite.

### Added — round 23 (manifest corruption / damaged-state recovery)

- `apply` refuses a semantically-corrupted base manifest: a deletion marker
  (whiteout) whose victim exists on the host but not in the base would
  previously hit the "keep host" branch — 0 operations, rc=0, then upper
  cleared and baseline advanced, silently destroying the only record of the
  deletion intent. It now fails loudly and leaves upper intact.
- `drop --force` can discard a worktree with unreadable/missing `meta.json`
  (half-created fork, disk damage): without `--force` it refuses with an
  actionable message; `list` warns instead of silently hiding the directory;
  `resolve` finds worktree-shaped dirs even without `meta.json`.
- Corrupt manifests with duplicate path keys (serde last-wins) or invalid
  path keys (absolute, `.`/`..` components, empty) are now rejected loudly —
  they previously produced misleading `both_added (base=-)` conflicts or
  silent no-ops.
- Regression locks: 22-variant manifest corruption matrix, and recovery
  paths (missing manifest/upper, garbage run.pid never block drop).

### Added — round 24 (apply failure atomicity / partial-apply rollback)

- File→empty-dir kind migration no longer deletes the freshly created
  directory (the migration Delete ran after Mkdir and removed it, losing
  the "create dir" intent while reporting written=1 deleted=1).
- Deleting a directory whose host content includes files unknown to base
  now conflicts instead of silently skipping the delete, reporting success
  and advancing the baseline (deletion intent was lost forever).
- TOCTOU guard: every destructive apply operation re-verifies the host
  path still matches the plan-time snapshot (size/mtime/kind) before
  touching it — a host edit landing between planning and execution aborts
  the apply instead of being silently overwritten.
- A read-only worktree file (mode 444 / read-only attribute) no longer
  makes the whole apply fail: the staged copy gets temporary write access
  for fsync, then its permissions are restored before the rename.
- Windows rename is now backup-and-restore: a failed MoveFile no longer
  leaves the destination missing (old file is moved aside, restored on
  failure) — previously a failed remove-then-rename lost both old and new
  content and retry dead-locked in a DeleteVsModify conflict.
- A missing `upper/` layer (kill -9 between apply's reset steps) is
  self-healed: diff/apply treat it as empty, run recreates it.
- `apply` re-checks the run/mount gates after the commit phase — a `cowt
  run` started mid-apply no longer has its upper-layer writes destroyed by
  the baseline advance + layer reset.
- Error paths now reference the worktree source path instead of the
  internal `.cowt-apply-*` staging path.
- Regression locks: partial-failure retry convergence, staging residue
  isolation (staging is outside the target and never scanned).

### Added — round 25 (merge conflict corners: rename collisions, dir↔file swap residuals)

- dir→file migration with host-only content directly under the migrated
  directory now conflicts at plan time (the R24 host_only check only
  covered pure deletes; the migration branch silently planned clean, then
  deleted base children and failed forever on the non-empty dir).
- dir→symlink migration now works on unix: `write_symlink` removes an
  empty directory left at the destination (it previously only removed
  files, so `symlink()` hit EEXIST every time).
- Regression locks: rename-collision matrix (7 combinations), conflict
  classification boundaries (BothAdded kind mismatches, converged dir
  children), plan re-execution idempotency, and work-source-missing error
  paths.

### Added — round 26 (run process semantics: env / cwd / signals / exit codes)

- userns mode no longer leaks `COWT_LOWER` (the host directory path) and
  the other internal mount variables into the child's environment — the
  isolation bypass is no longer handed out via `$COWT_LOWER`.
- `SIGTERM`/`SIGINT` sent to `cowt run` are now forwarded to the isolated
  child (escalating to SIGKILL if the child traps them), and the child runs
  in its own process group so stray grandchildren holding the view are
  reaped before unmount — killing cowt no longer orphans a process that
  keeps the mount (and deadlocks `drop --force` on an EBUSY unmount).
- The pidfile is only cleared once the mount is actually down; a surviving
  mount keeps the stale marker so `drop --force` recognizes it as our own
  leftover instead of a foreign mount.
- Missing commands exit 127 (shell convention) instead of 1, so scripts can
  distinguish "tool missing" from "tool failed".
- Child process semantics documented in the README (cwd, env, streams,
  signal forwarding) and locked by regression tests.
- Note: `cowt run` has no timeout by design — the child runs until it
  exits.

### Added — round 27 (overlay symlink semantics / nested dirs)

- A non-directory entry (symlink/file) replacing a base directory in upper
  now shadows the whole subtree in the merged manifest — `rm -rf x && ln -s
  t x` previously kept x/f.txt visible, so diff missed the deletion and
  apply deadlocked forever on the non-empty dir (ENOTEMPTY).
- macOS backend: `readlink` / `symlink` / `mknod` (regular files) are now
  implemented — host symlinks were completely unusable in the FUSE-T view
  (fuser defaults returned ENOSYS/EPERM, and readdir misreported symlinks
  as regular files).
- TOCTOU guard now also verifies symlink targets (not just "is still a
  symlink") before overwriting — a host retarget between planning and
  apply aborts instead of being silently replaced.
- `diff --content` on a changed symlink reports the link-target change
  instead of reading through to the (unrelated) target file contents.
- Windows: creating a file under a lower symlink/junction parent is refused
  (materializing the parent as a real directory in upper would flip its
  kind and break apply).
- Regression locks: symlink manifest round-trip (dangling/absolute/..-target)
  and whiteout-vs-symlink folding.

### Added — round 28 (pidfile races / locking / concurrency)

- `write_pidfile` stale replacement is now atomic (remove + retry O_EXCL) —
  two concurrent `cowt run` can no longer both win the stale-replace race
  and run against the same upper.
- `write_pidfile` uses the same starttime-aware liveness check as
  `running_pid`: a recycled-pid residual is replaced instead of causing a
  permanent, falsely-reported "another run in progress" deadlock.
- An empty or unparseable pidfile (kill -9 between create and write) is no
  longer treated as our stale run — stale-mount cleanup refuses on that
  evidence, so a foreign mount (rclone/sshfs) can never be torn down by a
  crash residual.
- `cowt run` only clears the pidfile when no live process is recorded — a
  successor run's marker is never deleted during our teardown window.
- userns mode: the wrapper now waits for the pidfile before mounting, so a
  kill in the spawn→pidfile window leaves the child waiting instead of
  holding an unrecorded mount (and `drop` no longer deletes state under a
  live child).
- `atomic_write` tmp names are process-unique (concurrent applies can no
  longer truncate each other's temp file).
- Windows Ctrl-C handler closes its process handle (no leak per Ctrl-C).
- Regression locks: stale_run refuses unparseable pidfiles;
  clear_running ownership; write_pidfile replaces recycled/rejects live
  markers; cli-level garbage-pidfile and recycled-pid run tests.

### Added — round 29 (unicode: NFC/NFD normalization, non-UTF-8 names)

- A non-UTF-8 filename (or symlink target) is now SKIPPED with a warning
  instead of hard-failing the whole `fork` — one bad name in a 10k-file
  tree no longer refuses everything, and `apply` no longer wedges forever
  on an unserializable baseline entry.
- macOS: manifest keys are canonicalized to NFC at scan time — APFS is
  normalization-insensitive, and a deletion spelled with a different
  normalization (NFC vs NFD) previously wrote a whiteout that never
  matched the base key, silently dropping the deletion intent.
- `sanitize_display` also strips Unicode format characters (bidi
  overrides, zero-width spaces, BOM) that `is_control()` misses, and the
  raw `meta.target` prints in `run`/`drop` now go through it — an ESC byte
  in a directory name can no longer inject terminal sequences.
- Regression locks: non-UTF-8 skip-with-warning; NFC/NFD distinct keys on
  normalization-sensitive filesystems.

### Added — round 30 (mode/mtime change detection & apply restore)

- Directory permission bits now count as content on unix (like files):
  a chmod-only change on a directory is visible to `diff`, and `apply`
  restores the worktree's mode — previously a `mkdir -m 0700` was widened
  to the umask default (0755) on the host, and dir chmods were silently
  dropped.
- macOS: `setattr` now writes the mode back into the isolated layer (copy
  up first, never touching the host) and `mkdir` preserves the requested
  permission bits — `chmod +x` inside a worktree actually works now (the
  chmod-only detection chain was dead at its source on macOS).
- TOCTOU guard: a host chmod between planning and execution aborts the
  apply (mode counts as content, matching `content_eq`).
- Regression locks: file/dir chmod-only → Modified → apply restores mode;
  touch-only is NOT a change (mtime deliberately excluded from content
  equality, otherwise apply would never converge); host chmod in the
  plan→execute window aborts.

### Added — round 31 (drop --force boundaries / missing worktree / running drop)

- macOS `drop --force` now verifies the mountpoint symlink is a cowt view
  under the state root (basename "view" + parent in COWT_HOME + worktree
  fingerprint) before removing it — a foreign symlink pointing at user
  data was previously deleted wholesale.
- Linux `drop --force` only unmounts mounts whose `/proc/self/mounts`
  device is overlay/fuse-overlayfs — a stale pidfile no longer authorizes
  tearing down a foreign filesystem mounted later (D-005).
- The `real`-dir data-loss check moved after the trash sweep (immediately
  before the rename) with post-rename rollback, so the slow sweep cannot
  widen a check→rename window during which a concurrent run moves the host
  dir aside; the sweep itself skips trashes holding a `real` dir.
- Half-created fork dirs (kill between create_dir and the manifest write:
  only upper/work present) are now resolvable and cleanable by
  `drop --force` — no more permanent ghosts.
- Trash names use the state dir name, not raw `meta.id` (a corrupted id
  could escape the state root).
- `terminate` verifies the process actually died (SIGKILL/taskkill
  follow-up with verification) and bails with the pid otherwise.
- Regression locks: half-created-dir cleanup; trash sweep preserves
  real-holding trashes.

### Added — round 32 (resource limits: huge trees / deep nesting / fd exhaustion / large files)

- `dir_size` (status/list) no longer follows symlinks — a self-ring or a
  link to an external tree previously caused a stack overflow or counted
  foreign bytes.
- `effective_manifest` whiteout/opaque folding is now O(n·log m) instead
  of O(n·m) — batch folding with ancestor-chain membership checks; bulk
  deletions of tens of thousands of files no longer take minutes in
  diff/apply.
- `collect_whiteouts` is iterative (explicit stack) instead of recursive —
  very deep trees no longer hit the fd limit and silently miss whiteouts.
- `apply`'s post-commit baseline scan reuses the plan-time hashes
  (`rescan(current)`) instead of hashing the whole tree twice.
- `merge::plan` precomputes the base-key set for host-only checks —
  deleting thousands of directories is no longer quadratic.
- Regression locks: ~1900-level deep tree scan (fd bounded); 96MB file
  streaming hash; 5k-entry × 800-whiteout fold stays fast.

### Added — round 33 (list/status/doctor output contracts)

- `list` ordering is deterministic: created_epoch first, id as tie-break —
  same-second worktrees no longer fall back to filesystem read_dir order.
- `status` distinguishes an unreadable upper layer from an empty one:
  "unknown (unreadable)" in human output and `upper_bytes: null` in JSON,
  instead of a fabricated 0.
- `.trash-*` names are refused by `resolve` (direct id lookup could
  previously resurrect a drop leftover as a ghost worktree in `status`).
- `status` human output now includes the `created:` line, matching the
  JSON schema.
- `fork --name` rejects control characters (newline/tab/ESC) — a hostile
  label can no longer break the one-worktree-per-line output contract or
  inject terminal escapes.
- `list`/`status` sanitize `id` and `backend` fields too — a forged
  meta.json backend could previously leak raw ANSI to the terminal.
- Removed duplicated filter blocks and a dead binding in `state.rs`
  (list/resolve).
- Regression locks: deterministic ordering, control-char name rejection,
  trash-name refusal, created line parity, sanitization, upper
  unreadability.

### Added — round 34 (manifest versioning & forward-compat contracts)

- `Manifest` carries a format `version` (default 1, omitted from output):
  a file written by a NEWER cowt now fails with a distinct
  "unsupported manifest format ... written by a newer cowt" error instead
  of being misreported as corrupt (which would have steered users to
  `drop --force` and destroyed a healthy worktree).
- All `Entry`/`WorktreeMeta` fields now carry `#[serde(default)]` — adding
  a field can no longer silently make every pre-existing manifest/meta
  unreadable (the `Option`-without-default serde trap is closed for
  `name` too). Locked by "v1 minimal contract" tests that fail the moment
  someone adds a field without a default.
- macOS: `from_json` NFC-normalizes path keys like the scanner does —
  pre-round-29 manifests (or hand-edited NFD keys) no longer produce
  phantom Deleted+Added pairs against a fresh rescan.
- Regression locks: newer-version rejection, v1 minimal manifest and meta
  contracts, full-entry round-trip (previously only one symlink target
  was round-tripped).

### Added — round 35 (fork: id conflicts / source validation / running fork)

- `fork` refuses a target that contains (or is inside) the cowt state root
  — the baseline previously snapshotted cowt's own state (other worktrees'
  uppers), and on winfsp/macOS the worktree could never run (moving the
  host dir into its own subdirectory fails).
- `fork` refuses a currently-mounted target (a `cowt run` in progress or a
  foreign mount) — the baseline previously captured the uncommitted merged
  view, including possibly torn upper writes.
- `fork` refuses when `$HOME` is unset and `--force-path` was not given
  (the boundary check was silently skipped), and opens the state root
  before scanning (error ordering).
- `fork` fails loudly when the scan found 0 entries with warnings — a
  fully unreadable directory no longer produces an "empty" worktree whose
  diff later reports every host file as Added.
- `fork --name .trash-*` is refused (such a worktree would be
  unaddressable by name).
- Concurrent same-name forks are serialized by a post-write name re-check:
  both could previously pass the pre-check (the other's meta.json wasn't
  written yet) and produce an ambiguous duplicate.
- Regression locks: state-root containment, mounted-target refusal,
  trash-prefixed name rejection.

### Added — round 36 (crash-window recovery matrix)

- Linux: a kill -9 between mount success and pidfile write left a live
  overlay with NO ownership proof — run/apply/diff and `drop --force` all
  refused it forever (manual umount only). Ownership now also checks the
  mount options (`upperdir=<state>/<id>/upper`) in `/proc/self/mounts`,
  so such leftovers are recognized and torn down.
- winfsp/macOS: `copy_up`'s temp file moved into a reserved
  `.cowt-copy-tmp.` namespace that `effective_manifest` filters — a crash
  between copy and rename previously surfaced as a ghost `A foo.tmp` diff
  and applied the torn file into the host directory.
- macOS: a kill -9 between `rename(target→real)` and the mountpoint
  symlink stranded the host dir with no way out (run failed, `drop
  --force` hit the real guard). `is_mounted` now also recognizes the
  stranded state via meta.target, and `unmount` restores it.
- `apply` sweeps stale `.cowt-apply-<pid>-*` staging dirs (dead pid only)
  — crashed applies no longer accumulate unbounded staged copies in the
  target's parent.
- `atomic_write` sweeps stale `*.json.tmp-<pid>-*` residue (dead pid
  only) — crashed fork/apply no longer accumulate unbounded manifest
  copies in the state dir.
- Regression locks: copy-tmp invisibility (effective_manifest + plan),
  stale tmp sweep, staging sweep.

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
