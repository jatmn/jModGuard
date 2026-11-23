#!/usr/bin/env bash
# Exercise the fixed shell handoff/fallback without a live Bunny container.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
scratch="$(mktemp -d "${TMPDIR:-/tmp}/jmodguard-bootstrap-XXXXXX")"
bootstrap_pid=''
trap 'if [ -n "$bootstrap_pid" ]; then kill -TERM "$bootstrap_pid" 2>/dev/null || true; wait "$bootstrap_pid" 2>/dev/null || true; fi; rm -rf "$scratch"' EXIT
mkdir -p "$scratch/updates"
release="sha256:$(printf 'a%.0s' {1..64})"
sed -e "s#/usr/local/bin/jmodguard-launcher#$scratch/baked#g" \
    -e "s#/tmp/jmodguard-launcher-next#$scratch/control#g" \
    -e "s#/tmp/jmodguard-updates/#$scratch/updates/#g" \
    "$root/scripts/bootstrap-container.sh" >"$scratch/bootstrap"
cat >"$scratch/baked" <<'SH'
#!/bin/sh
if [ ! -f "$JMODGUARD_TEST_ROOT/started" ]; then
  : >"$JMODGUARD_TEST_ROOT/started"
else
  # Rejection fixtures submit just one handoff.
  [ -z "${JMODGUARD_TEST_CANDIDATE:-}" ] || exit 2
  [ -n "${JMODGUARD_LAUNCHER_FAILED:-}" ] || exit 2
  : >"$JMODGUARD_TEST_ROOT/fallback"
  # The owner requests the same digest after the first replacement failed.
  : >"$JMODGUARD_TEST_ROOT/retry"
fi
candidate="${JMODGUARD_TEST_CANDIDATE:-$JMODGUARD_TEST_ROOT/updates/next}"
hash="${JMODGUARD_TEST_HASH:-$(sha256sum "$candidate" | cut -d' ' -f1)}"
printf '%s\n%s\n%s\n' "$candidate" "$hash" "$JMODGUARD_TEST_RELEASE" >"$JMODGUARD_TEST_ROOT/control"
exit 77
SH
cat >"$scratch/updates/next" <<'SH'
#!/bin/sh
: >"$JMODGUARD_TEST_ROOT/candidate-started"
if [ ! -f "$JMODGUARD_TEST_ROOT/retry" ]; then exit 42; fi
[ -z "${JMODGUARD_LAUNCHER_FAILED:-}" ] || exit 3
printf '%s\n' "${JMODGUARD_LAUNCHER_RELEASE:-}" >"$JMODGUARD_TEST_ROOT/launcher-release"
: >"$JMODGUARD_TEST_ROOT/retry-succeeded"
while :; do sleep 1; done
SH
chmod 700 "$scratch/baked" "$scratch/updates/next"
cp -p "$scratch/updates/next" "$scratch/outside"

assert_rejected_launcher() {
  local reason="$1" candidate="$2" hash="$3" status=0
  JMODGUARD_TEST_ROOT="$scratch" JMODGUARD_TEST_RELEASE="$release" \
    JMODGUARD_TEST_CANDIDATE="$candidate" JMODGUARD_TEST_HASH="$hash" \
    timeout --foreground --kill-after=2s 10s /bin/sh "$scratch/bootstrap" || status=$?
  [ ! -e "$scratch/candidate-started" ] || { echo "bootstrap executed launcher with $reason" >&2; exit 1; }
  [ "$status" -eq 77 ] || { echo "bootstrap did not return the baked launcher's status for $reason: $status" >&2; exit 1; }
  rm "$scratch/started"
}

assert_rejected_launcher 'a mismatched hash' "$scratch/updates/next" "$(printf '%064d' 0)"
assert_rejected_launcher 'a path outside the updates directory' "$scratch/outside" "$(sha256sum "$scratch/outside" | cut -d' ' -f1)"

mkdir -p "$scratch/alternate-tmp"
TMPDIR="$scratch/alternate-tmp" JMODGUARD_TEST_ROOT="$scratch" JMODGUARD_TEST_RELEASE="$release" /bin/sh "$scratch/bootstrap" &
bootstrap_pid=$!
attempt=0
while [ ! -f "$scratch/retry-succeeded" ] && [ "$attempt" -lt 100 ]; do
  sleep 0.1
  attempt=$((attempt + 1))
done
[ -f "$scratch/fallback" ] || { echo 'bootstrap fallback was not reached' >&2; exit 1; }
[ -f "$scratch/retry-succeeded" ] || { echo 'owner retry inherited a stale failure marker' >&2; exit 1; }
[ "$(cat "$scratch/launcher-release")" = "$release" ] || { echo 'launcher did not receive the exact release ID from the control record' >&2; exit 1; }
kill -TERM "$bootstrap_pid"
wait "$bootstrap_pid"
bootstrap_pid=''
echo 'bootstrap-container-test: ok'
