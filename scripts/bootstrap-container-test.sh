#!/usr/bin/env bash
# Exercise the fixed shell handoff/fallback without a live Bunny container.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
scratch="$(mktemp -d "${TMPDIR:-/tmp}/jmodguard-bootstrap-XXXXXX")"
bootstrap_pid=''
trap 'if [ -n "$bootstrap_pid" ]; then kill -TERM "$bootstrap_pid" 2>/dev/null || true; wait "$bootstrap_pid" 2>/dev/null || true; fi; rm -rf "$scratch"' EXIT
mkdir -p "$scratch/updates"
sed -e "s#/usr/local/bin/jmodguard-launcher#$scratch/baked#g" \
    -e "s#/tmp/jmodguard-launcher-next#$scratch/control#g" \
    -e "s#/tmp/jmodguard-updates/#$scratch/updates/#g" \
    "$root/scripts/bootstrap-container.sh" >"$scratch/bootstrap"
cat >"$scratch/baked" <<'SH'
#!/bin/sh
if [ ! -f "$JMODGUARD_TEST_ROOT/started" ]; then
  : >"$JMODGUARD_TEST_ROOT/started"
else
  [ -n "${JMODGUARD_LAUNCHER_FAILED:-}" ] || exit 2
  : >"$JMODGUARD_TEST_ROOT/fallback"
  # The owner requests the same digest after the first replacement failed.
  : >"$JMODGUARD_TEST_ROOT/retry"
fi
hash=$(sha256sum "$JMODGUARD_TEST_ROOT/updates/next" | cut -d' ' -f1)
printf '%s\n%s\n%s\n' "$JMODGUARD_TEST_ROOT/updates/next" "$hash" "sha256:$(printf '%064d' 0)" >"$JMODGUARD_TEST_ROOT/control"
exit 77
SH
cat >"$scratch/updates/next" <<'SH'
#!/bin/sh
if [ ! -f "$JMODGUARD_TEST_ROOT/retry" ]; then exit 42; fi
[ -z "${JMODGUARD_LAUNCHER_FAILED:-}" ] || exit 3
[ -n "${JMODGUARD_LAUNCHER_RELEASE:-}" ] || exit 4
: >"$JMODGUARD_TEST_ROOT/retry-succeeded"
while :; do sleep 1; done
SH
chmod 700 "$scratch/baked" "$scratch/updates/next"
mkdir -p "$scratch/alternate-tmp"
TMPDIR="$scratch/alternate-tmp" JMODGUARD_TEST_ROOT="$scratch" /bin/sh "$scratch/bootstrap" &
bootstrap_pid=$!
attempt=0
while [ ! -f "$scratch/retry-succeeded" ] && [ "$attempt" -lt 100 ]; do
  sleep 0.1
  attempt=$((attempt + 1))
done
[ -f "$scratch/fallback" ] || { echo 'bootstrap fallback was not reached' >&2; exit 1; }
[ -f "$scratch/retry-succeeded" ] || { echo 'owner retry inherited a stale failure marker' >&2; exit 1; }
kill -TERM "$bootstrap_pid"
wait "$bootstrap_pid"
bootstrap_pid=''
echo 'bootstrap-container-test: ok'
