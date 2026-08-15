#!/usr/bin/env bash
# Install FUSE-T (kext-less FUSE for macOS) and make it visible to the
# Rust build: fuser's build.rs probes `pkg-config fuse` on macOS, and the
# linker needs a `libfuse.dylib`.
#
# Usage:  bash scripts/macos/install-fuse-t.sh
# Run as the normal user (Homebrew refuses to run as root); privileged
# filesystem writes inside use sudo.

set -euo pipefail

if [ "$(id -u)" -eq 0 ]; then
    echo "error: do not run as root (Homebrew refuses); run as the normal user" >&2
    exit 1
fi

echo "==> installing FUSE-T via Homebrew"
brew install macos-fuse-t/homebrew-cask/fuse-t

echo "==> locating the FUSE-T libfuse-compatible dylib"
FUSE_T_DYLIB=""
for d in \
    /usr/local/lib/libfuse.2.dylib \
    /usr/local/lib/libfuse.dylib \
    /usr/local/lib/libfuse-t.dylib \
    /Library/Filesystems/fuse-t.fs/Contents/Resources/libfuse-t.dylib
do
    if [ -f "$d" ]; then
        FUSE_T_DYLIB="$d"
        break
    fi
done
if [ -z "$FUSE_T_DYLIB" ]; then
    # FUSE-T may install under an unexpected name; look around.
    FUSE_T_DYLIB=$(find /usr/local/lib /Library/Filesystems/fuse-t.fs/Contents/Resources \
        -name 'libfuse*' -type f 2>/dev/null | head -1 || true)
fi
if [ -z "$FUSE_T_DYLIB" ]; then
    echo "error: could not find the FUSE-T libfuse dylib" >&2
    exit 1
fi
echo "    dylib: $FUSE_T_DYLIB"

if [ ! -e /usr/local/lib/libfuse.dylib ]; then
    sudo ln -s "$FUSE_T_DYLIB" /usr/local/lib/libfuse.dylib
    echo "    linked /usr/local/lib/libfuse.dylib"
fi

echo "==> ensuring pkg-config metadata (fuser build.rs probes 'fuse')"
sudo mkdir -p /usr/local/lib/pkgconfig
if [ ! -f /usr/local/lib/pkgconfig/fuse.pc ]; then
    sudo tee /usr/local/lib/pkgconfig/fuse.pc > /dev/null <<'EOF'
prefix=/usr/local
libdir=${prefix}/lib
includedir=${prefix}/include
Name: fuse
Description: FUSE-T (kext-less FUSE for macOS)
Version: 2.9.9
Libs: -L${libdir} -Wl,-rpath,${libdir} -lfuse-t
Cflags: -I${includedir}/fuse -D_FILE_OFFSET_BITS=64
EOF
    echo "    wrote /usr/local/lib/pkgconfig/fuse.pc"
fi

if pkg-config --exists fuse; then
    echo "pkg-config fuse: OK ($(pkg-config --modversion fuse))"
else
    echo "warning: pkg-config still cannot find fuse; set PKG_CONFIG_PATH=/usr/local/lib/pkgconfig" >&2
fi
echo "done."
