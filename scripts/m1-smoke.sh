#!/usr/bin/env bash
# M1 behaviour checks: disk image, sandboxed VMM, confined workloads. Linux
# with KVM only. Builds the image and the VMM, then runs the checks as root.
set -euo pipefail
cd "$(dirname "$0")/.."
scripts/m1-image.sh
cargo build -q --release -p sandpiper-vmm
exec sudo scripts/smoke.sh m1 "$@"
