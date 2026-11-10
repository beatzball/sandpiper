#!/usr/bin/env bash
# Behaviour checks. Boots one microVM per check, so it needs libkrun and
# cannot run in CI. Use the wrappers: scripts/m0-smoke.sh (Mac or Linux,
# directory root) and scripts/m1-smoke.sh (Linux, disk image, sandboxed).
#
# Usage: scripts/smoke.sh m0|m1
set -uo pipefail
cd "$(dirname "$0")/.."
[ "$(uname -s)" = Darwin ] && export DYLD_FALLBACK_LIBRARY_PATH="${DYLD_FALLBACK_LIBRARY_PATH:-$(brew --prefix)/lib:/usr/local/lib:/usr/lib}"
V=target/release/sandpiper-vmm
MODE=${1:?usage: smoke.sh m0|m1}
case "$MODE" in
  m0) ROOT=(--root target/m0/rootfs); export SANDPIPER_UNSANDBOXED=1 ;;
  m1) ROOT=(--disk target/m1/base.img)
      [ "$(id -u)" = 0 ] || { echo "m1 checks run as root (the VMM sandbox needs it)"; exit 2; } ;;
  *) echo "unknown mode $MODE"; exit 2 ;;
esac
fails=0

check() { # name, expected, actual
  if [ "$2" = "$3" ]; then echo "ok   $1"; else echo "FAIL $1: want [$2] got [$3]"; fails=$((fails + 1)); fi
}
run() { "$V" run "${ROOT[@]}" "$@"; }

check "stdout"            "hello"   "$(run -- echo hello 2>/dev/null)"
check "stderr separate"   "err"     "$(run -- sh -c 'echo out; echo err >&2' 2>&1 >/dev/null)"
run -- sh -c 'exit 7' 2>/dev/null; check "exit code" "7" "$?"
run -- /nope 2>/dev/null;          check "missing command" "127" "$?"
check "stdin"             "3"       "$(printf 'a\nb\nc\n' | run --stdin -- wc -l 2>/dev/null | tr -d ' ')"
check "big stdin + output (no deadlock)" "1048576" \
  "$(head -c 5000000 /dev/zero | run --stdin --timeout 20 -- cat 2>/dev/null | wc -c | tr -d ' ')"
check "slow reader gets every byte before Exit" "900000" \
  "$(run --timeout 20 -- head -c 900000 /dev/zero 2>/dev/null | (sleep 2; wc -c) | tr -d ' ')"
check "output cap reported" "sandpiper: output was truncated at the per-stream cap" \
  "$(run -- head -c 3000000 /dev/zero 2>&1 >/dev/null)"
check "cap reported when a background process holds stdout" "sandpiper: output was truncated at the per-stream cap" \
  "$(run --timeout 20 -- sh -c 'head -c 3000000 /dev/zero; sleep 30 &' 2>&1 >/dev/null)"
check "big stdin to a slow reader does not exhaust guest memory" "629145600" \
  "$(head -c 629145600 /dev/zero | run --stdin --timeout 120 -- sh -c 'sleep 3; wc -c' 2>/dev/null | tr -d ' ')"
run --timeout 2 -- sleep 10 2>/dev/null; check "timeout exit" "124" "$?"
check "no network"        "no-network" \
  "$(run -- sh -c 'wget -q -T 3 -O- http://1.1.1.1 >/dev/null 2>&1 && echo reachable || echo no-network' 2>/dev/null)"
check "clean environment" "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" "$(run -- env 2>/dev/null)"
start=$(date +%s)
check "background child does not hold the exec open" "hi" "$(run --timeout 20 -- sh -c 'sleep 30 & echo hi' 2>/dev/null)"
check "  ...and returns promptly" "1" "$([ $(( $(date +%s) - start )) -lt 5 ] && echo 1 || echo 0)"
"$V" run "${ROOT[@]}" --timeout 60 -- sleep 60 >/dev/null 2>&1 &
driver=$!
sleep 2
kill -9 "$driver" 2>/dev/null; wait "$driver" 2>/dev/null
sleep 1
check "VMM dies when its driver is SIGKILLed" "" "$(pgrep -f 'sandpiper-vmm boot')"
check "no leftover VMMs"  "" "$(pgrep -f 'sandpiper-vmm boot')"

if [ "$MODE" = m1 ]; then
  # Walls inside the guest (DESIGN.md §4.4).
  check "workload runs as uid 1000" "1000" "$(run -- id -u 2>/dev/null)"
  check "workload cannot open vsock" "blocked" "$(run -- /sandpiper/guest probe vsock 2>/dev/null)"
  check "workload cannot use io_uring" "blocked" "$(run -- /sandpiper/guest probe io_uring 2>/dev/null)"
  check "system files belong to root" "0" "$(run -- stat -c %u /bin/busybox 2>/dev/null)"
  check "workload cannot write system files" "denied" \
    "$(run -- sh -c 'echo x > /etc/passwd 2>/dev/null && echo written || echo denied' 2>/dev/null)"
  run -- sh -c 'echo left-behind > /work/mark' >/dev/null 2>&1
  check "each machine gets a fresh disk" "clean" \
    "$(run -- sh -c 'test -e /work/mark && echo dirty || echo clean' 2>/dev/null)"

  # The VMM sandbox, seen from the host (DESIGN.md §4.2).
  "$V" run "${ROOT[@]}" --timeout 30 -- sleep 20 >/dev/null 2>&1 &
  driver=$!
  vmm=""
  for _ in $(seq 1 100); do
    vmm=$(pgrep -f 'sandpiper-vmm boot' | while read -r p; do
      [ "$(stat -c %u /proc/$p)" != 0 ] && echo "$p"; done | head -1)
    [ -n "$vmm" ] && break
    sleep 0.1
  done
  field() { awk -v k="$1:" '$1 == k { print $2 }' "/proc/$vmm/status"; }
  same_ns() { [ "$(readlink /proc/$vmm/ns/$1)" = "$(readlink /proc/1/ns/$1)" ] && echo same || echo own; }
  check "VMM runs as its own uid, not root" "1" "$([ -n "$vmm" ] && [ "$(field Uid)" -ge 200000 ] && echo 1 || echo 0)"
  check "VMM has no capabilities" "0000000000000000" "$(field CapEff)"
  check "VMM has no supplementary groups" "" "$(field Groups)"
  check "VMM is under a seccomp filter" "2" "$(field Seccomp)"
  check "VMM cannot gain privileges" "1" "$(field NoNewPrivs)"
  for ns in net mnt pid ipc uts; do check "VMM has its own $ns namespace" "own" "$(same_ns $ns)"; done
  check "VMM sees only dev and machine" "dev machine" "$(ls /proc/$vmm/root | tr '\n' ' ' | sed 's/ $//')"
  check "VMM's network namespace has no devices but lo" "lo" \
    "$(nsenter -t "$vmm" -n ip -o link show | awk -F': ' '{print $2}' | tr '\n' ' ' | sed 's/ $//')"
  kill "$driver" 2>/dev/null; wait "$driver" 2>/dev/null
fi

[ "$fails" -eq 0 ] && echo "all checks passed" || { echo "$fails check(s) failed"; exit 1; }
