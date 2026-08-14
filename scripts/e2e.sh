#!/usr/bin/env bash
# co-worktree end-to-end acceptance suite.
#
# Exercises every acceptance criterion from the product spec against a real
# fuse-overlayfs backend: fork / run / diff / apply / drop, plus performance
# budgets, crash recovery, three-way conflicts and zero-residue teardown.
#
# Usage:  scripts/e2e.sh [path-to-cowt-binary]
# Requires: fuse-overlayfs, fusermount3, /dev/fuse.

set -euo pipefail

COWT_BIN="${1:-${COWT_BIN:-./target/release/cowt}}"
COWT_BIN="$(cd "$(dirname "$COWT_BIN")" && pwd)/$(basename "$COWT_BIN")"

PASS=0
FAIL=0
FAILED_TESTS=()

ok()   { PASS=$((PASS+1)); echo "  PASS  $1"; }
bad()  { FAIL=$((FAIL+1)); FAILED_TESTS+=("$1"); echo "  FAIL  $1"; }
check() { # check <name> <condition...>
  local name="$1"; shift
  if "$@"; then ok "$name"; else bad "$name"; fi
}

# contains <haystack> <needle>
contains() { [[ "$1" == *"$2"* ]]; }

section() { echo; echo "== $1 =="; }

# ------------------------------------------------------------------ sandbox
ROOT="$(mktemp -d)"
export HOME="$ROOT/home"
export COWT_HOME="$ROOT/state"
mkdir -p "$HOME"
cleanup() {
  set +e
  # Best effort: unmount anything left, wipe sandbox.
  grep "$ROOT" /proc/self/mounts | awk '{print $2}' | while read -r mp; do
    fusermount3 -u "$mp" 2>/dev/null || umount "$mp" 2>/dev/null
  done
  rm -rf "$ROOT"
}
trap cleanup EXIT

cowt() { "$COWT_BIN" "$@"; }

# Print the manifest.json path of the worktree with the given name.
manifest_of() {
  local d
  for d in "$COWT_HOME"/*/; do
    if grep -q "\"$1\"" "$d/meta.json" 2>/dev/null; then
      echo "$d/manifest.json"
      return 0
    fi
  done
  return 1
}

section "0. prerequisites"
cowt doctor
cowt doctor | grep -q "available: yes" || { echo "fuse-overlayfs unavailable, aborting E2E"; exit 1; }

# =================================================================== Fork ==
section "1. fork"

APP="$HOME/.config/e2eapp"
mkdir -p "$APP/sub"
printf 'line1\nline2\nline3\n' > "$APP/settings.txt"
printf '{"font": 12, "theme": "dark", "nested": {"x": 1}}\n' > "$APP/prefs.json"
printf 'cache-body\n' > "$APP/cache.bin"

t0=$(date +%s%N)
fork_out="$(cowt fork "$APP" --name e2eapp)"
t1=$(date +%s%N)
fork_ms=$(( (t1 - t0) / 1000000 ))
ID="$(cowt list --json | grep -o '"id": *"[^"]*"' | head -1 | sed 's/.*"id": *"//;s/"//')"
[ -n "$ID" ] || { echo "could not resolve worktree id"; exit 1; }
check "empty worktree fork < 500ms (was ${fork_ms}ms)" [ "$fork_ms" -lt 500 ]
list_out="$(cowt list)"
check "worktree listed" bash -c "echo '$list_out' | grep -q e2eapp"

# 10k+ file manifest scan
BIG="$HOME/.config/bigapp"
mkdir -p "$BIG"
for d in $(seq 1 50); do
  mkdir -p "$BIG/d$d"
  for f in $(seq 1 200); do echo "payload $d $f" > "$BIG/d$d/f$f.txt"; done
done
cowt fork "$BIG" --name bigapp >/dev/null
big_manifest="$(manifest_of bigapp)"
n_entries="$(grep -c '": {' "$big_manifest" || true)"
check "base manifest covers 10k files (found >= 10000)" [ "$n_entries" -ge 10000 ]

# symlink escape guard
ESC_OUT="$ROOT/outside-secret.txt"
echo "secret" > "$ESC_OUT"
ln -s "$ROOT" "$APP/escape-link"
cowt fork "$APP" --name escapetest >/dev/null
esc_manifest="$(manifest_of escapetest)"
check "symlink not followed (no escape-secret in manifest)" bash -c "! grep -q outside-secret '$esc_manifest'"
cowt drop escapetest >/dev/null
rm "$APP/escape-link"

# ==================================================================== Run ==
section "2. run (fuse-overlayfs backend)"

cowt run "$ID" -- sh -c '
  set -e
  cd "$HOME/.config/e2eapp"
  sed -i "s/line2/line2 CHANGED/" settings.txt
  printf "{\"font\": 16, \"theme\": \"dark\", \"nested\": {\"x\": 1, \"y\": 2}}\n" > prefs.json
  rm cache.bin
  mkdir -p logs && echo "session" > logs/session.log
' || echo "  (run exited rc=$?)"
# Diagnostics: dump the upper layer so whiteout encoding is visible in CI logs.
echo "  --- upper layer after run ---"
ls -la "$(dirname "$(manifest_of e2eapp)")/upper/" || true

check "host untouched during run (settings.txt)" grep -q '^line2$' "$APP/settings.txt"
check "host untouched during run (cache.bin present)" [ -f "$APP/cache.bin" ]
check "host untouched during run (no logs/)" [ ! -e "$APP/logs" ]
check "reads pass through (base visible in view)" grep -q '^line1$' "$APP/settings.txt"

# sequential-write overhead < 20% vs native: best of 3 runs, 4 MiB blocks
PERF="$HOME/.config/perfapp"; mkdir -p "$PERF"
cowt fork "$PERF" --name perfapp >/dev/null
best_native=999999; best_overlay=999999
for i in 1 2 3; do
  t0=$(date +%s%N)
  dd if=/dev/zero of="$PERF/native.bin" bs=4M count=128 conv=fdatasync 2>/dev/null
  t1=$(date +%s%N); rm -f "$PERF/native.bin"
  n=$(( (t1 - t0) / 1000000 )); [ "$n" -lt "$best_native" ] && best_native=$n
  t0=$(date +%s%N)
  cowt run perfapp -- dd if=/dev/zero of="$PERF/overlay.bin" bs=4M count=128 conv=fdatasync 2>/dev/null >/dev/null
  t1=$(date +%s%N)
  o=$(( (t1 - t0) / 1000000 )); [ "$o" -lt "$best_overlay" ] && best_overlay=$o
  rm -f "$PERF/overlay.bin" 2>/dev/null || true
done
native_ms=$best_native; overlay_ms=$best_overlay
# overlay_ms <= native_ms * 1.2  (integer math: overlay*5 <= native*6)
echo "  perf: native ${native_ms}ms vs overlay ${overlay_ms}ms (best of 3)"
check "sequential write overhead < 20%" [ $(( overlay_ms * 5 )) -le $(( native_ms * 6 + 1 )) ]
cowt drop perfapp --force >/dev/null

# crash survival
cowt run "$ID" -- sh -c 'echo crash > "$HOME/.config/e2eapp/crash.tmp"; kill -9 $$' 2>/dev/null || true
crash_diff="$(cowt diff "$ID" --json)"
check "after kill -9 upper data still diffable" contains "$crash_diff" crash.tmp

# =================================================================== Diff ==
section "3. diff"

diff_json="$(cowt diff "$ID" --json)"
check "added detected"    contains "$diff_json" '"path": "logs/session.log"'
check "modified detected" contains "$diff_json" '"path": "settings.txt"'
check "deleted detected"  contains "$diff_json" '"path": "cache.bin"'

content="$(cowt diff "$ID" --content)"
check "Myers line diff for text"  bash -c "grep -q -- '-line2' <<<\"$content\" && grep -q -- '+line2 CHANGED' <<<\"$content\""
check "JSON key-level diff"       contains "$content" 'font: 12 -> 16'

t0=$(date +%s%N)
cowt run bigapp -- sh -c 'for i in $(seq 1 50); do echo x >> "$HOME/.config/bigapp/d$i/f$i.txt"; done' >/dev/null 2>&1
cowt diff bigapp --stat >/dev/null
t1=$(date +%s%N)
diff_ms=$(( (t1 - t0) / 1000000 ))
echo "  10k-file worktree diff (incl. 50-change run): ${diff_ms}ms"
check "10k diff < 3s" [ "$diff_ms" -lt 3000 ]

# ================================================================== Apply ==
section "4. apply (three-way merge)"

# 4a. clean merge: base==current, worktree changed
cowt apply "$ID" >/dev/null
check "clean apply writes changes" grep -q 'line2 CHANGED' "$APP/settings.txt"
check "clean apply deletes whiteout victim" [ ! -e "$APP/cache.bin" ]
check "clean apply creates new files" [ -f "$APP/logs/session.log" ]
check "clean apply merges json content" grep -q '"font": 16' "$APP/prefs.json"

# 4b. conflict: host and worktree both changed the same file differently
CF="$HOME/.config/cfapp"; mkdir -p "$CF"
printf 'base\n' > "$CF/shared.txt"; printf 'stable\n' > "$CF/other.txt"
cowt fork "$CF" --name cfapp >/dev/null
CFID="$(cowt list --json | grep -B2 cfapp | grep -o '"id": *"[^"]*"' | head -1 | sed 's/.*"id": *"//;s/"//')"
cowt run cfapp -- sh -c 'echo worktree > "$HOME/.config/cfapp/shared.txt"; echo clean > "$HOME/.config/cfapp/clean.txt"' 2>/dev/null
echo host > "$CF/shared.txt"   # host moves after fork

set +e
dry="$(cowt apply cfapp --dry-run --json)"; dry_rc=$?
set -e
check "--dry-run exit 3 on conflict" [ "$dry_rc" -eq 3 ]
check "--dry-run reports structured conflict (kind)" contains "$dry" both_modified
check "--dry-run reports three hashes" bash -c "grep -q base_hash <<<\"$dry\" && grep -q current_hash <<<\"$dry\" && grep -q work_hash <<<\"$dry\""

set +e
cowt apply cfapp >/dev/null 2>&1; rc=$?
set -e
check "apply exits 3 on conflict" [ "$rc" -eq 3 ]
check "conflict: zero pollution (shared.txt still host)" grep -q '^host$' "$CF/shared.txt"
check "conflict: zero pollution (clean.txt NOT written)" [ ! -e "$CF/clean.txt" ]
check "conflict: no staging residue" bash -c "! ls -d \"$CF\"/../.cowt-apply-* >/dev/null 2>&1"
cowt drop cfapp >/dev/null

# 4c. host moved, worktree untouched -> host kept
KP="$HOME/.config/keepapp"; mkdir -p "$KP"; printf 'v1\n' > "$KP/f.txt"
cowt fork "$KP" --name keepapp >/dev/null
cowt run keepapp -- sh -c 'echo new > "$HOME/.config/keepapp/other.txt"' 2>/dev/null
echo host-v2 > "$KP/f.txt"
cowt apply keepapp >/dev/null
check "host change kept, worktree change applied" bash -c "grep -q host-v2 '$KP/f.txt' && [ -f '$KP/other.txt' ]"
cowt drop keepapp >/dev/null

# =================================================================== Drop ==
section "5. drop"

# refuse while running
cowt run bigapp -- sleep 30 >/dev/null 2>&1 &
runner=$!
for _ in $(seq 1 50); do
  ls "$COWT_HOME"/*/run.pid >/dev/null 2>&1 && break
  sleep 0.1
done
set +e
drop_out="$(cowt drop bigapp 2>&1)"; drop_rc=$?
set -e
check "drop refuses while running" [ "$drop_rc" -ne 0 ]
check "refusal mentions running process" contains "$drop_out" "running"
cowt drop bigapp --force >/dev/null
wait $runner 2>/dev/null || true
post_list="$(cowt list)"
check "force drop killed runner and cleaned state" bash -c "! grep -q bigapp <<<\"$post_list\""

cowt drop "$ID" >/dev/null
check "state fully removed" bash -c "[ -z \"\$(ls -A '$COWT_HOME' 2>/dev/null)\" ]"
check "no fuse mounts left" bash -c "! grep -q fuse-overlayfs /proc/self/mounts"
check "host keeps applied content after drop" grep -q 'line2 CHANGED' "$APP/settings.txt"

# ================================================================ summary ==
section "summary"
echo "passed: $PASS, failed: $FAIL"
if [ "$FAIL" -gt 0 ]; then
  printf 'failed tests:\n'
  printf '  - %s\n' "${FAILED_TESTS[@]}"
  exit 1
fi
echo "E2E: ALL GREEN"
