#!/usr/bin/env bash
set -e
TMP=$(mktemp -d)
HOME_DIR="$TMP/home" STATE="$TMP/state"
TARGET="$HOME_DIR/.config/demoapp"
mkdir -p "$TARGET"
printf 'alpha\nbeta\n' > "$TARGET/config.txt"
printf '{"a": 1, "b": 2}' > "$TARGET/prefs.json"
printf 'stale' > "$TARGET/stale.cache"
export HOME="$HOME_DIR" COWT_HOME="$STATE"
unset XDG_STATE_HOME || true
COWT=./target/debug/cowt.exe

echo "== fork"
$COWT fork "$TARGET" --name demo
ID=$($COWT list --json | python -c "import sys,json;print(json.load(sys.stdin)[0]['id'])")
echo "id=$ID"
echo "== apply 1"
mkdir -p "$STATE/$ID/upper"
printf 'BETA\n' > "$STATE/$ID/upper/config.txt"
$COWT apply demo
echo "host config after apply1: [$(cat "$TARGET/config.txt")]"
echo "== apply 2"
printf 'BETA2\n' > "$STATE/$ID/upper/config.txt"
printf 'added\n' > "$STATE/$ID/upper/new.txt"
$COWT apply demo || echo "APPLY2 FAILED rc=$?"
echo "host config after apply2: [$(cat "$TARGET/config.txt")]"
rm -rf "$TMP"
