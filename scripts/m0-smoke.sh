#!/usr/bin/env bash
# M0 behaviour checks (directory root). Run scripts/m0.sh once first.
exec "$(dirname "$0")/smoke.sh" m0 "$@"
