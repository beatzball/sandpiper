#!/usr/bin/env bash
# Host agent checks: the API, the event log, time limits, cgroups and
# cleanup. Linux with KVM only; runs sandpiperd as root on a private state
# directory and port.
#
# Usage: scripts/m1-api-smoke.sh
set -uo pipefail
cd "$(dirname "$0")/.."
scripts/m1-image.sh >/dev/null || exit 1
cargo build -q --release -p sandpiper-vmm -p sandpiperd || exit 1

STATE=$(mktemp -d /var/tmp/sandpiperd-smoke.XXXXXX)
PORT=17171
URL=http://127.0.0.1:$PORT
D=target/release/sandpiperd
fails=0
daemon=

check() { # name, expected, actual
  if [ "$2" = "$3" ]; then echo "ok   $1"; else echo "FAIL $1: want [$2] got [$3]"; fails=$((fails + 1)); fi
}
start() {
  sudo "$D" --state "$STATE" --listen "127.0.0.1:$PORT" --allow-weak-host 2>>"$STATE.log" &
  daemon=$!
  for _ in $(seq 100); do curl -fs "$URL/v1/health" >/dev/null && break; sleep 0.1; done
  TOKEN=$(sudo cat "$STATE/token")
}
stop() { sudo kill -"${1:-TERM}" "$(pgrep -P "$daemon" -x sandpiperd || echo "$daemon")" 2>/dev/null; wait "$daemon" 2>/dev/null; }
cleanup() { [ -n "$daemon" ] && stop; sudo rm -rf "$STATE" "$STATE.log" "$STATE.big" "$STATE.stream"; }
trap cleanup EXIT

# api METHOD PATH [JSON] — prints the body, then the status on its own line.
api() {
  curl -s -X "$1" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    ${3:+--data "$3"} -w '\n%{http_code}' "$URL$2"
}
status() { tail -n1; }
field() { python3 -c "import json,sys; print(json.loads(sys.stdin.read().splitlines()[0])$1)"; }
# The stdout of an exec stream, decoded, then the exit code or end event.
result() {
  python3 -c '
import base64, json, sys
out, end = b"", ""
for line in sys.stdin.read().splitlines()[:-1]:
    if not line: continue
    e = json.loads(line)
    if e["type"] == "exec.stdout": out += base64.b64decode(e["data"])
    if e["type"] == "exec.exited": end = str(e["code"])
    if e["type"] in ("exec.timedout", "exec.failed"): end = e["type"]
print(out.decode(errors="replace").strip() + "|" + end)'
}
BASE='{"template":"base"}'
create() { api POST /v1/machines "${1:-$BASE}"; }
run_in() { api POST "/v1/machines/$1/exec" "$2" | result; }

sudo install -D -m 0644 target/m1/base.img "$STATE/templates/base.img"
start

check "health needs no token"   "200" "$(curl -s -o /dev/null -w '%{http_code}' "$URL/v1/health")"
check "no token is refused"     "401" "$(curl -s -o /dev/null -w '%{http_code}' -X POST "$URL/v1/machines")"
check "wrong token is refused"  "401" "$(TOKEN=x api POST /v1/machines '{"template":"base"}' | status)"
check "unknown template"        "404" "$(create '{"template":"nope"}' | status)"
check "template name is a name" "404" "$(create '{"template":"../token"}' | status)"
check "unknown field refused"   "422" "$(create '{"template":"base","privileged":true}' | status)"
check "ttl above the ceiling"   "400" "$(create '{"template":"base","ttl":{"maxMs":999999999}}' | status)"

out=$(create)
check "create"                  "201" "$(echo "$out" | status)"
M=$(echo "$out" | field '["id"]')
check "created machine runs"    "running" "$(echo "$out" | field '["state"]')"
check "exec stdout and code"    "hello|0" "$(run_in "$M" '{"cmd":["echo","hello"]}')"
check "exec exit code"          "|7"      "$(run_in "$M" '{"cmd":["sh","-c","exit 7"]}')"
check "exec stdin"              "3|0"     "$(run_in "$M" '{"cmd":["wc","-l"],"stdin":"a\nb\nc\n"}')"
check "exec env and cwd"        "v /tmp|0" "$(run_in "$M" '{"cmd":["sh","-c","echo $K $(pwd)"],"env":{"K":"v"},"cwd":"/tmp"}')"
check "workload is uid 1000"    "1000|0"  "$(run_in "$M" '{"cmd":["id","-u"]}')"
check "a steady trickle of output streams in pieces" "1" "$(api POST "/v1/machines/$M/exec" \
  '{"cmd":["sh","-c","i=0; while [ $i -lt 30 ]; do printf x; sleep 0.05; i=$((i+1)); done"]}' |
  grep -c '"type":"exec.stdout"' | awk '{print ($1 >= 5)}')"
check "files persist between execs" "kept|0" \
  "$(run_in "$M" '{"cmd":["sh","-c","echo kept > /tmp/f"]}' >/dev/null; run_in "$M" '{"cmd":["cat","/tmp/f"]}')"
check "exec timeout"            "|exec.timedout" "$(run_in "$M" '{"cmd":["sleep","30"],"timeoutMs":1000}')"
check "machine runs on after a timeout" "still|0" "$(run_in "$M" '{"cmd":["echo","still"]}')"
# A command that never reads its stdin, with more input than any buffer
# holds: the timeout must still stop it.
python3 -c 'import base64,json; print(json.dumps({"cmd":["sleep","30"],"timeoutMs":1500,"stdinBase64":base64.b64encode(bytes(4<<20)).decode()}))' >"$STATE.big"
check "timeout with unread stdin" "|exec.timedout" "$(run_in "$M" "@$STATE.big")"
check "  ...and the next exec runs" "next|0" "$(run_in "$M" '{"cmd":["echo","next"]}')"
api POST "/v1/machines/$M/exec" '{"cmd":["sleep","3"]}' >/dev/null &
long=$!
sleep 1
check "one exec at a time"      "409" "$(api POST "/v1/machines/$M/exec" '{"cmd":["true"]}' | status)"
wait "$long"

# Outside the machine: its own uid and cgroup.
# libkrun renames the VMM's process, so match its command line.
vmm_uid=$(ps -eo uid=,args= | grep "sandpiper-vmm boot.*$M" | grep -v grep | awk '{print $1}' | sort -n | tail -1)
check "VMM runs as a machine uid" "1" "$([ "${vmm_uid:-0}" -ge 1900000000 ] && echo 1 || echo 0)"
cg=$(find /sys/fs/cgroup -maxdepth 3 -type d -name "$M" | head -1)
check "machine has a cgroup"    "1" "$([ -n "$cg" ] && echo 1 || echo 0)"
check "cgroup memory limit"     "805306368" "$(cat "$cg/memory.max" 2>/dev/null)"
check "cgroup io limit set"     "1" "$(grep -q wbps "$cg/io.max" 2>/dev/null && echo 1 || echo 0)"

replay=$(curl -s --max-time 2 -H "Authorization: Bearer $TOKEN" "$URL/v1/events?machine=$M&from=1")
check "events replay from 1"    "machine.created" "$(echo "$replay" | head -1 | field '["type"]')"
check "events replay from n"    "3" "$(curl -s --max-time 2 -H "Authorization: Bearer $TOKEN" "$URL/v1/events?machine=$M&from=3" | head -1 | field '["seq"]')"

api POST "/v1/machines/$M/exec" '{"cmd":["sleep","30"]}' >"$STATE.stream" &
long=$!
sleep 1
check "destroy"                 "204" "$(api DELETE "/v1/machines/$M" | status)"
wait "$long"
check "an exec in flight ends with the machine" "exec.failed machine stopped" \
  "$(head -n -1 "$STATE.stream" | grep -v '^$' | tail -1 | python3 -c 'import json,sys; e=json.load(sys.stdin); print(e["type"], e["reason"])')"
check "  ...and its end comes before the machine's" "exec.failed machine.destroyed" \
  "$(curl -s --max-time 5 -H "Authorization: Bearer $TOKEN" "$URL/v1/events?machine=$M" | tail -2 | python3 -c 'import json,sys; print(" ".join(json.loads(l)["type"] for l in sys.stdin))')"
check "destroyed machine state" "destroyed" "$(api GET "/v1/machines/$M" | field '["state"]')"
check "cgroup removed"          "0" "$([ -d "$cg" ] && echo 1 || echo 0)"
check "no VMM left"             "" "$(pgrep -f 'sandpiper-vmm boot')"
check "events end at destroyed" "machine.destroyed" \
  "$(curl -s --max-time 5 -H "Authorization: Bearer $TOKEN" "$URL/v1/events?machine=$M" | tail -1 | field '["type"]')"
check "replay past the end returns at once" "0" \
  "$(curl -s -o /dev/null -w '%{exitcode}' --max-time 3 -H "Authorization: Bearer $TOKEN" "$URL/v1/events?machine=$M&from=99999")"

# The final stop needs nothing from the VMM side: freeze the supervisor and
# the machine still goes.
M=$(create | field '["id"]')
sup=$(pgrep -f "sandpiper-vmm boot.*$M" | head -1)
sudo kill -STOP "$sup"
check "destroy with a stopped supervisor" "204" "$(api DELETE "/v1/machines/$M" | status)"
check "  ...leaves nothing running" "" "$(pgrep -f "sandpiper-vmm boot.*$M")"

M=$(create '{"template":"base","ttl":{"idleMs":1500}}' | field '["id"]')
sleep 3
check "idle TTL destroys"       "idle TTL reached" "$(api GET "/v1/machines/$M" | field '["reason"]')"
M=$(create '{"template":"base","ttl":{"maxMs":2500}}' | field '["id"]')
check "max TTL ends a running exec" "|exec.failed" "$(run_in "$M" '{"cmd":["sleep","10"]}')"
curl -s --max-time 10 -H "Authorization: Bearer $TOKEN" "$URL/v1/events?machine=$M" >/dev/null
check "  ...and the machine" "max TTL reached" "$(api GET "/v1/machines/$M" | field '["reason"]')"

M=$(create | field '["id"]')
stop KILL
sleep 1
check "VMMs die with the host agent" "" "$(pgrep -f 'sandpiper-vmm boot')"
start
check "restart marks lost machines failed" "failed" "$(api GET "/v1/machines/$M" | field '["state"]')"

M=$(create | field '["id"]')
curl -s -H "Authorization: Bearer $TOKEN" "$URL/v1/events?machine=$M" >/dev/null &
follower=$!
sleep 0.5
began=$(date +%s)
stop TERM
check "a clean stop does not wait for open streams" "1" "$([ $(( $(date +%s) - began )) -lt 10 ] && echo 1 || echo 0)"
wait "$follower"
start
check "clean stop destroys machines" "host agent stopping" "$(api GET "/v1/machines/$M" | field '["reason"]')"

if [ "$fails" -ne 0 ]; then
  echo "--- sandpiperd log"; sudo cat "$STATE.log"
  echo "$fails check(s) failed"; exit 1
fi
echo "all checks passed"
