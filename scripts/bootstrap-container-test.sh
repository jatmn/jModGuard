#!/usr/bin/env bash
# Exercise the fixed shell handoff/fallback without a live Bunny container.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
scratch="$(mktemp -d "${TMPDIR:-/tmp}/jmodguard-bootstrap-XXXXXX")"
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/updates"
sed -e "s#/usr/local/bin/jmodguard-launcher#$scratch/baked#g" \
    -e "s#/tmp/jmodguard-launcher-next#$scratch/control#g" \
    -e "s#/tmp/jmodguard-updates/#$scratch/updates/#g" \
    "$root/scripts/bootstrap-container.sh" >"$scratch/bootstrap"
cat >"$scratch/baked" <<'SH'
#!/bin/sh
if [ ! -f "$JMODGUARD_TEST_ROOT/started" ]; then
  : >"$JMODGUARD_TEST_ROOT/started"
  hash=$(sha256sum "$JMODGUARD_TEST_ROOT/updates/next" | cut -d' ' -f1)
  printf '%s\n%s\n%s\n' "$JMODGUARD_TEST_ROOT/updates/next" "$hash" "sha256:$(printf '%064d' 0)" >"$JMODGUARD_TEST_ROOT/control"
  exit 77
fi
[ -n "${JMODGUARD_LAUNCHER_FAILED:-}" ] || exit 2
: >"$JMODGUARD_TEST_ROOT/fallback"
exit 0
SH
cat >"$scratch/updates/next" <<'SH'
#!/bin/sh
exit 42
SH
chmod 700 "$scratch/baked" "$scratch/updates/next"
JMODGUARD_TEST_ROOT="$scratch" /bin/sh "$scratch/bootstrap"
[ -f "$scratch/fallback" ] || { echo 'bootstrap fallback was not reached' >&2; exit 1; }
echo 'bootstrap-container-test: ok'
