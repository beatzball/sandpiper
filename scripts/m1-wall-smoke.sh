#!/usr/bin/env bash
# Wall check, firewall table and template checks for the host agent. Linux
# with KVM only. Also builds test binaries (in target/wall-tests) that open
# a wall on purpose, and shows the wall check fails those machines.
#
# Usage: scripts/m1-wall-smoke.sh
set -uo pipefail
cd "$(dirname "$0")/.."
scripts/m1-image.sh >/dev/null || exit 1
TEMPLATE=node scripts/m1-image.sh >/dev/null || exit 1
cargo build -q --release -p sandpiper-vmm -p sandpiperd || exit 1
cargo build -q --release --target-dir target/wall-tests \
  -p sandpiper-vmm -p sandpiperd --features sandpiper-vmm/wall-tests,sandpiperd/wall-tests || exit 1

STATE=$(mktemp -d /var/tmp/sandpiperd-walls.XXXXXX)
PORT=17181
URL=http://127.0.0.1:$PORT
fails=0
daemon=

check() { # name, expected, actual
  if [ "$2" = "$3" ]; then echo "ok   $1"; else echo "FAIL $1: want [$2] got [$3]"; fails=$((fails + 1)); fi
}
start() { # [env...] BIN_DIR
  sudo env "$@" "$BIN/sandpiperd" --state "$STATE" --listen "127.0.0.1:$PORT" --allow-weak-host --vmm "$BIN/sandpiper-vmm" 2>>"$STATE.log" &
  daemon=$!
  for _ in $(seq 100); do curl -fs "$URL/v1/health" >/dev/null && break; sleep 0.1; done
  TOKEN=$(sudo cat "$STATE/token")
}
stop() { sudo kill -TERM "$(pgrep -P "$daemon" -x sandpiperd || echo "$daemon")" 2>/dev/null; wait "$daemon" 2>/dev/null; daemon=; }
cleanup() { [ -n "$daemon" ] && stop; sudo nft delete table inet sandpiper 2>/dev/null; sudo rm -rf "$STATE" "$STATE.log"; }
trap cleanup EXIT

api() {
  curl -s -X "$1" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    ${3:+--data "$3"} -w '\n%{http_code}' "$URL$2"
}
status() { tail -n1; }
field() { python3 -c "import json,sys; print(json.loads(sys.stdin.read().splitlines()[0])$1)"; }
events() { curl -s --max-time 3 -H "Authorization: Bearer $TOKEN" "$URL/v1/events?machine=$1"; }

sudo install -D -m 0644 target/m1/base.img "$STATE/templates/base.img"
sudo install -D -m 0644 target/m1/node.img "$STATE/templates/node.img"

BIN=target/release
# This dev VM is not hardened (nested, shared CPU), so the checks pass
# --allow-weak-host. Show that the check is there.
if grep -qs '^Vulnerable' /sys/devices/system/cpu/vulnerabilities/*; then
  check "a weak host is refused without --allow-weak-host" "1" \
    "$(sudo "$BIN/sandpiperd" --state "$STATE" --listen 127.0.0.1:$PORT 2>&1 | grep -c 'not hardened')"
fi
start
check "health is ok" "True" "$(curl -s "$URL/v1/health" | field '["ok"]')"
check "firewall table installed" "1" "$(sudo nft list table inet sandpiper >/dev/null 2>&1 && echo 1 || echo 0)"
M=$(api POST /v1/machines '{"template":"base"}' | field '["id"]')
walls=$(events "$M" | python3 -c '
import json, sys
for l in sys.stdin:
    e = json.loads(l)
    if e["type"] == "machine.checked": print(" ".join(e["walls"]))')
check "every machine is wall-checked" \
  "netns netns-devices cgroup uid guest:vsock guest:net-devices canary:host-loopback canary:host-lan canary:vmm-netns" "$walls"
api DELETE "/v1/machines/$M" >/dev/null

N=$(api POST /v1/machines '{"template":"node"}' | field '["id"]')
check "node template runs node" "2" "$(api POST "/v1/machines/$N/exec" '{"cmd":["node","-e","console.log(1+1)"]}' |
  python3 -c '
import base64, json, sys
print("".join(base64.b64decode(json.loads(l)["data"]).decode() for l in sys.stdin if l.strip() and l.startswith("{") and json.loads(l)["type"] == "exec.stdout").strip())')"
api DELETE "/v1/machines/$N" >/dev/null

sudo nft add rule inet sandpiper input counter accept
for _ in $(seq 30); do [ "$(curl -s "$URL/v1/health" | field '["ok"]')" = False ] && break; sleep 1; done
check "a changed firewall table is noticed" "False" "$(curl -s "$URL/v1/health" | field '["ok"]')"
check "  ...and no machine starts" "503" "$(api POST /v1/machines '{"template":"base"}' | status)"
stop

# Test builds with a wall open on purpose.
BIN=target/wall-tests/release
for wall in tsi net; do
  start SANDPIPER_BREAK_WALL=$wall
  out=$(api POST /v1/machines '{"template":"base"}')
  check "a test build says so" "wall-tests" "$(curl -s "$URL/v1/health" | field '["build"]')"
  check "a machine with $wall open is refused" "500" "$(echo "$out" | status)"
  case $wall in
    tsi) want="wall check failed: a probe from inside reached the vmm-netns canary" ;;
    net) want="wall check failed: net-devices is open inside the guest" ;;
  esac
  check "  ...by the wall check" "$want" "$(echo "$out" | field '["error"]' | sed 's/^machine failed to start: //')"
  stop
done

if [ "$fails" -ne 0 ]; then
  echo "--- sandpiperd log"; sudo cat "$STATE.log"
  echo "$fails check(s) failed"; exit 1
fi
echo "all checks passed"
