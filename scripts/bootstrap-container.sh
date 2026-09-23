#!/bin/sh
# Fixed PID 1 supervisor. The Rust launcher may be replaced independently;
# a failed replacement falls back to the launcher baked into this image.
set -u

baked=/usr/local/bin/jmodguard-launcher
active="$baked"
child=''
control=/tmp/jmodguard-launcher-next

shutdown() {
  if [ -n "$child" ]; then
    kill -TERM "$child" 2>/dev/null || true
    wait "$child" 2>/dev/null || true
  fi
  exit 0
}
trap shutdown TERM INT

while :; do
  rm -f "$control"
  "$active" &
  child=$!
  status=0
  wait "$child" || status=$?
  child=''
  if [ "$status" -eq 77 ] && [ -f "$control" ]; then
    candidate=$(sed -n '1p' "$control")
    expected=$(sed -n '2p' "$control")
    release=$(sed -n '3p' "$control")
    case "$candidate:$expected:$release" in
      /tmp/jmodguard-updates/*:????????????????????????????????????????????????????????????????:sha256:*)
        if [ -f "$candidate" ] && [ "$(sha256sum "$candidate" | cut -d' ' -f1)" = "$expected" ]; then
          active="$candidate"
          unset JMODGUARD_LAUNCHER_FAILED
          export JMODGUARD_LAUNCHER_RELEASE="$release"
          continue
        fi
        ;;
    esac
  fi
  if [ "$active" != "$baked" ]; then
    JMODGUARD_LAUNCHER_FAILED="${JMODGUARD_LAUNCHER_RELEASE:-}"
    export JMODGUARD_LAUNCHER_FAILED
    active="$baked"
    unset JMODGUARD_LAUNCHER_RELEASE
    continue
  fi
  exit "$status"
done
