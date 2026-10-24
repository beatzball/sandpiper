#!/usr/bin/env bash
# Behaviour checks for M0. Boots one microVM per check, so it needs libkrun
# and cannot run in CI. Run scripts/m0.sh once first to build everything.
set -uo pipefail
cd "$(dirname "$0")/.."
[ "$(uname -s)" = Darwin ] && export DYLD_FALLBACK_LIBRARY_PATH="${DYLD_FALLBACK_LIBRARY_PATH:-$(brew --prefix)/lib:/usr/local/lib:/usr/lib}"
V=target/release/sandpiper-vmm
R=target/m0/rootfs
fails=0

check() { # name, expected, actual
  if [ "$2" = "$3" ]; then echo "ok   $1"; else echo "FAIL $1: want [$2] got [$3]"; fails=$((fails + 1)); fi
}
run() { "$V" run --root "$R" "$@"; }

check "stdout"            "hello"   "$(run -- echo hello 2>/dev/null)"
check "stderr separate"   "err"     "$(run -- sh -c 'echo out; echo err >&2' 2>&1 >/dev/null)"
run -- sh -c 'exit 7' 2>/dev/null; check "exit code" "7" "$?"
run -- /nope 2>/dev/null;          check "missing command" "127" "$?"
check "stdin"             "3"       "$(printf 'a\nb\nc\n' | run --stdin -- wc -l 2>/dev/null | tr -d ' ')"
check "big stdin + output (no deadlock)" "1048576" \
  "$(head -c 5000000 /dev/zero | run --stdin --timeout 20 -- cat 2>/dev/null | wc -c | tr -d ' ')"
check "output cap reported" "sandpiper: output was truncated at the per-stream cap" \
  "$(run -- head -c 3000000 /dev/zero 2>&1 >/dev/null)"
run --timeout 2 -- sleep 10 2>/dev/null; check "timeout exit" "124" "$?"
check "no network"        "no-network" \
  "$(run -- sh -c 'wget -q -T 3 -O- http://1.1.1.1 >/dev/null 2>&1 && echo reachable || echo no-network' 2>/dev/null)"
check "clean environment" "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" "$(run -- env 2>/dev/null)"
start=$(date +%s)
check "background child does not hold the exec open" "hi" "$(run --timeout 20 -- sh -c 'sleep 30 & echo hi' 2>/dev/null)"
check "  ...and returns promptly" "1" "$([ $(( $(date +%s) - start )) -lt 5 ] && echo 1 || echo 0)"
"$V" run --root "$R" --timeout 60 -- sleep 60 >/dev/null 2>&1 &
driver=$!
sleep 2
kill -9 "$driver" 2>/dev/null; wait "$driver" 2>/dev/null
sleep 1
check "VMM dies when its driver is SIGKILLed" "" "$(pgrep -f 'sandpiper-vmm boot')"
check "no leftover VMMs"  "" "$(pgrep -f 'sandpiper-vmm boot')"

[ "$fails" -eq 0 ] && echo "all checks passed" || { echo "$fails check(s) failed"; exit 1; }
