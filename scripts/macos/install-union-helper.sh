#!/usr/bin/env bash
# Install the cowt-provided union mount helper for macOS.
#
# Apple removed /Library/Filesystems/union.fs from current macOS images
# (macOS 15+; also missing on macos-14 GH runners), so `mount -t union`
# fails with "exec ... mount_union: No such file or directory". The kernel
# union vfs is still present; only the userspace helper was dropped.
#
# Usage:  sudo bash scripts/macos/install-union-helper.sh
# Afterwards `cowt doctor` on macOS reports the union backend available.

set -euo pipefail

SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/mount_union.c"
DEST_DIR="/Library/Filesystems/union.fs/Contents/Resources"
DEST="$DEST_DIR/mount_union"

if [ "$(id -u)" -ne 0 ]; then
    echo "error: run with sudo (installs to $DEST)" >&2
    exit 1
fi

mkdir -p "$DEST_DIR"
cc -O2 -Wall -o "$DEST" "$SRC"
chmod 755 "$DEST"
echo "installed $DEST"

# Sanity check: the probe `mount -t union` must now succeed.
if mount -t union -o nobrowse "$(mktemp -d)" "$(mktemp -d)" 2>/dev/null; then
    echo "union mount probe: OK"
else
    echo "warning: mount probe failed — the kernel may have dropped union vfs" >&2
fi
