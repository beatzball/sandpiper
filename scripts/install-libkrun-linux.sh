#!/usr/bin/env bash
# Install the pinned libkrunfw (prebuilt, checksum-verified) and libkrun
# (built from source, with block and network devices) into /usr/local.
# Linux only; needs sudo, a Rust toolchain, build-essential and libclang-dev
# (libkrun's build generates bindings with it).
set -euo pipefail

LIBKRUNFW_VERSION=v5.6.2
LIBKRUN_VERSION=v1.19.7
# Tags can move; the commit cannot.
LIBKRUN_COMMIT=43b2f5aadeebb90f42053c9df2ec8b16583e554f
ARCH=$(uname -m)
case "$ARCH" in
  aarch64) FW_SHA256=dea7905a167eee17d482200ea2fe15871aceb293b0674ac825ed7ae69759399f ;;
  x86_64)  FW_SHA256=016c32ddb2a28aa300382cab33352d63a41a901eb36ee8d1ca24b389791c8b91 ;;
  *) echo "install-libkrun: unsupported architecture $ARCH" >&2; exit 1 ;;
esac

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

curl -fsSL -o "$work/fw.tgz" \
  "https://github.com/libkrun/libkrunfw/releases/download/${LIBKRUNFW_VERSION}/libkrunfw-${ARCH}.tgz"
echo "${FW_SHA256}  $work/fw.tgz" | sha256sum -c --quiet
tar -xzf "$work/fw.tgz" -C "$work"
sudo install -m 0755 "$work"/lib*/*/libkrunfw.so.* /usr/local/lib/
sudo ln -sf "libkrunfw.so.${LIBKRUNFW_VERSION#v}" /usr/local/lib/libkrunfw.so.5
sudo ln -sf libkrunfw.so.5 /usr/local/lib/libkrunfw.so

git clone -q --depth 1 -b "$LIBKRUN_VERSION" https://github.com/containers/libkrun.git "$work/libkrun"
[ "$(git -C "$work/libkrun" rev-parse HEAD)" = "$LIBKRUN_COMMIT" ] || {
  echo "install-libkrun: $LIBKRUN_VERSION is not commit $LIBKRUN_COMMIT" >&2; exit 1; }
make -C "$work/libkrun" BLK=1 NET=1 -j"$(nproc)"
sudo make -C "$work/libkrun" BLK=1 NET=1 install PREFIX=/usr/local

# libkrun installs into lib64, which Debian and Ubuntu do not search.
echo /usr/local/lib64 | sudo tee /etc/ld.so.conf.d/sandpiper-libkrun.conf >/dev/null
sudo ldconfig
ldconfig -p | grep -E 'libkrun(fw)?\.so\.' 
