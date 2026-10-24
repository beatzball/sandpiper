#!/usr/bin/env bash
# M0: boot an arm64 Linux microVM with libkrun and run one command in it over
# vsock. Development only (DESIGN.md §12).
#
# Usage: scripts/m0.sh [CMD [ARGS...]]     default: echo hello
set -euo pipefail
cd "$(dirname "$0")/.."

ALPINE_VERSION=3.24.3
ALPINE_SHA256=4d3f80afaebd447c5591a63a73bde380a149fe8b2d85488cafabb2ff97551f50
ARCH=aarch64
TARGET="${ARCH}-unknown-linux-musl"
OUT=target/m0
ROOTFS="$OUT/rootfs"

mkdir -p "$OUT"
tarball="$OUT/alpine-minirootfs-${ALPINE_VERSION}-${ARCH}.tar.gz"
if [ ! -f "$tarball" ]; then
  curl -fsSL -o "$tarball.part" \
    "https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION%.*}/releases/${ARCH}/alpine-minirootfs-${ALPINE_VERSION}-${ARCH}.tar.gz"
  mv "$tarball.part" "$tarball"
fi
echo "${ALPINE_SHA256}  ${tarball}" | shasum -a 256 -c --quiet

if [ ! -d "$ROOTFS" ]; then
  mkdir -p "$ROOTFS.part"
  tar -xzf "$tarball" -C "$ROOTFS.part"
  mv "$ROOTFS.part" "$ROOTFS"
fi

cargo build -q --release -p sandpiper-guest --target "$TARGET"
mkdir -p "$ROOTFS/sandpiper"
install -m 0755 "target/$TARGET/release/sandpiper-guest" "$ROOTFS/sandpiper/guest"

cargo build -q --release -p sandpiper-vmm
VMM=target/release/sandpiper-vmm
if [ "$(uname -s)" = Darwin ]; then
  codesign -f -s - --entitlements scripts/hypervisor.entitlements "$VMM" 2>/dev/null
  # libkrun loads its kernel (libkrunfw) by bare name at runtime, and dyld's
  # default fallback path does not include the Homebrew prefix.
  export DYLD_FALLBACK_LIBRARY_PATH="${DYLD_FALLBACK_LIBRARY_PATH:-$(brew --prefix)/lib:/usr/local/lib:/usr/lib}"
fi

[ $# -eq 0 ] && set -- echo hello
exec "$VMM" run --root "$ROOTFS" --time -- "$@"
