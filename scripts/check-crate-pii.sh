#!/usr/bin/env bash
# Package a crate and scan what would be published for personal details.
# Usage: scripts/check-crate-pii.sh [crate]   (default: sandpiper)
set -euo pipefail
crate="${1:-sandpiper}"
blocklist="${HOME}/.config/git/identity-blocklist"

cargo generate-lockfile --quiet
cargo package -p "$crate" --allow-dirty --no-verify --quiet
version=$(cargo pkgid -p "$crate" | sed 's/.*[#@]//')
archive="target/package/${crate}-${version}.crate"

dir=$(mktemp -d); patterns=$(mktemp)
trap 'rm -rf "$dir" "$patterns"' EXIT
tar -xzf "$archive" -C "$dir"

# Blank and comment lines in the blocklist would match everything.
[ -f "$blocklist" ] && grep -vE '^\s*(#|$)' "$blocklist" > "$patterns"
printf '%s\n' "/Users/" "/home/" "$(id -un)" >> "$patterns"

if grep -rniE -f "$patterns" "$dir"; then
  echo "FAIL: ${archive} contains personal details (lines above)." >&2
  exit 1
fi
echo "ok: ${archive} is clean"
