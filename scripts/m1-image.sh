#!/usr/bin/env bash
# Build an M1 template: an ext4 image of Alpine with the guest agent in it.
# Linux only. Needs sudo, so that every file in the image is owned by root
# and not by whoever ran the build.
#
# Usage: scripts/m1-image.sh [OUT]                 base: target/m1/base.img
#        TEMPLATE=node scripts/m1-image.sh [OUT]   adds Node.js: target/m1/node.img
#
# The node template installs Alpine's nodejs package with apk, which checks
# each package against the signing keys in the pinned rootfs. The version
# that went in is written to /etc/sandpiper-template in the image.
set -euo pipefail
cd "$(dirname "$0")/.."

ALPINE_VERSION=3.24.3
ARCH=$(uname -m)
case "$ARCH" in
  aarch64) ALPINE_SHA256=4d3f80afaebd447c5591a63a73bde380a149fe8b2d85488cafabb2ff97551f50 ;;
  *) echo "m1-image: no pinned rootfs for $ARCH yet" >&2; exit 1 ;;
esac
TARGET="${ARCH}-unknown-linux-musl"
TEMPLATE=${TEMPLATE:-base}
case "$TEMPLATE" in
  base) SIZE=256M ;;
  node) SIZE=384M ;;
  *) echo "m1-image: unknown template $TEMPLATE (base or node)" >&2; exit 1 ;;
esac
OUT=${1:-target/m1/$TEMPLATE.img}

mkdir -p "$(dirname "$OUT")" target/m1
tarball="target/m1/alpine-minirootfs-${ALPINE_VERSION}-${ARCH}.tar.gz"
if [ ! -f "$tarball" ]; then
  curl -fsSL -o "$tarball.part" \
    "https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION%.*}/releases/${ARCH}/alpine-minirootfs-${ALPINE_VERSION}-${ARCH}.tar.gz"
  mv "$tarball.part" "$tarball"
fi
echo "${ALPINE_SHA256}  ${tarball}" | sha256sum -c --quiet

cargo build -q --release -p sandpiper-guest --target "$TARGET"

stage=$(mktemp -d)
trap 'sudo rm -rf "$stage"' EXIT
sudo tar -xzf "$tarball" -C "$stage"
sudo install -D -o root -g root -m 0755 "target/$TARGET/release/sandpiper-guest" "$stage/sandpiper/guest"
# The workload user (see sandpiper-guest's confine.rs) and its one writable
# directory besides /tmp.
echo 'sandpiper:x:1000:1000:workload:/work:/sbin/nologin' | sudo tee -a "$stage/etc/passwd" >/dev/null
echo 'sandpiper:x:1000:' | sudo tee -a "$stage/etc/group" >/dev/null
sudo install -d -o 1000 -g 1000 -m 0755 "$stage/work"
sudo chmod 1777 "$stage/tmp"
echo "template=$TEMPLATE alpine=$ALPINE_VERSION" | sudo tee "$stage/etc/sandpiper-template" >/dev/null
if [ "$TEMPLATE" = node ]; then
  # The image gets no resolver config of its own: machines have no network.
  sudo cp /etc/resolv.conf "$stage/etc/resolv.conf"
  sudo chroot "$stage" /sbin/apk add --no-cache --quiet nodejs >/dev/null
  sudo rm -f "$stage/etc/resolv.conf"
  sudo chroot "$stage" /sbin/apk list --installed nodejs 2>/dev/null | sudo tee -a "$stage/etc/sandpiper-template" >/dev/null
fi

rm -f "$OUT.part"
truncate -s "$SIZE" "$OUT.part"
sudo mkfs.ext4 -q -L sandpiper -d "$stage" "$OUT.part"
sudo chown "$(id -u):$(id -g)" "$OUT.part"
mv "$OUT.part" "$OUT"
echo "built $OUT"
