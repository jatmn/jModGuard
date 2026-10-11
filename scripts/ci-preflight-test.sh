#!/usr/bin/env bash
# Exercise ensure_cargo_tool without running supply-chain gates.
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
fixture="$(mktemp -d "${TMPDIR:-/tmp}/jmodguard-ci-preflight-test.XXXXXX")"
trap 'rm -rf "$fixture"' EXIT

# shellcheck source=scripts/ci-ensure-cargo-tool.sh
source "$root/scripts/ci-ensure-cargo-tool.sh"

fail() {
  echo "ci-preflight test: $*" >&2
  exit 1
}

write_cargo_deny() {
  local path="$1" version="$2" marker="${3:-}"
  cat >"$path" <<SH
#!/bin/bash
set -euo pipefail
if [ "\${1:-}" = --version ]; then
  echo "cargo-deny ${version}"
  exit 0
fi
if [ -n "${marker}" ]; then
  echo "ci-preflight test: wrong deny binary executed (${marker})" >&2
  exit 9
fi
exit 0
SH
  chmod +x "$path"
}

mkdir -p "$fixture/bin" "$fixture/opt" "$fixture/cargo-home/bin"
install_log="$fixture/install.log"

cat >"$fixture/bin/cargo" <<'SH'
#!/bin/bash
set -euo pipefail
if [ "${1:-}" = install ]; then
  printf 'argv=%s\n' "$*" >>"$CI_PREFLIGHT_TEST_INSTALL_LOG"
  mkdir -p "$CARGO_HOME/bin"
  cat >"$CARGO_HOME/bin/cargo-deny" <<'INNER'
#!/bin/bash
if [ "${1:-}" = --version ]; then
  echo "cargo-deny 0.20.2"
  exit 0
fi
exit 0
INNER
  chmod +x "$CARGO_HOME/bin/cargo-deny"
  exit 0
fi
echo "ci-preflight test cargo: unexpected: $*" >&2
exit 1
SH
chmod +x "$fixture/bin/cargo"

run_ensure() {
  local stdout="$fixture/stdout"
  (
    export JMODGUARD_CI_PREFIX="$fixture/opt"
    export CARGO_HOME="$fixture/cargo-home"
    export CI_PREFLIGHT_TEST_INSTALL_LOG="$install_log"
    export PATH="$1:/usr/bin:/bin"
    rm -f "$install_log"
    ensure_cargo_tool cargo-deny cargo-deny 0.20.2
  ) >"$stdout" 2>&1
  printf '%s' "$(cat "$stdout")"
}

pinned_root="$fixture/opt/cargo-deny-0.20.2/bin"
mkdir -p "$pinned_root"

# Pinned present and correct.
write_cargo_deny "$pinned_root/cargo-deny" 0.20.2
write_cargo_deny "$fixture/bin/cargo-deny" 9.9.9 wrong-path
out="$(run_ensure "$fixture/bin")"
grep -Fq 'using pinned cargo-deny at' "$fixture/stdout" || fail 'pinned correct: expected pinned message'
grep -Fq "$pinned_root/cargo-deny" "$fixture/stdout" || fail 'pinned correct: expected pinned path'
if [ -f "$install_log" ]; then
  fail 'pinned correct: must not install'
fi

# Pinned missing, PATH correct.
rm -rf "$fixture/opt/cargo-deny-0.20.2"
write_cargo_deny "$fixture/bin/cargo-deny" 0.20.2
out="$(run_ensure "$fixture/bin")"
grep -Fq 'using cargo-deny at' "$fixture/stdout" || fail 'path correct: expected PATH message'
grep -Fq 'using pinned' "$fixture/stdout" && fail 'path correct: must not use pinned'
if [ -f "$install_log" ]; then
  fail 'path correct: must not install'
fi

# Pinned missing, PATH wrong version -> install pin, never run wrong binary.
write_cargo_deny "$fixture/bin/cargo-deny" 9.9.9 wrong-path
out="$(run_ensure "$fixture/bin")"
grep -Fq 'installing cargo-deny 0.20.2' "$fixture/stdout" || fail 'path wrong: expected install message'
[ -f "$install_log" ] || fail 'path wrong: expected cargo install'
grep -Fq 'using cargo-deny at' "$fixture/stdout" || fail 'path wrong: expected resolved binary message'
resolved="$(PATH="$fixture/cargo-home/bin:$fixture/bin" command -v cargo-deny)"
case "$resolved" in
  "$fixture/cargo-home/bin/cargo-deny") ;;
  *) fail "path wrong: expected install in cargo home, got $resolved" ;;
esac
if PATH="$fixture/cargo-home/bin:$fixture/bin" cargo-deny check 2>/dev/null; then
  :
else
  rc=$?
  [ "$rc" -eq 9 ] && fail 'path wrong: wrong deny binary was executed'
fi

# Pinned present but wrong version -> PATH correct copy used.
mkdir -p "$pinned_root"
write_cargo_deny "$pinned_root/cargo-deny" 9.9.9 wrong-pinned
write_cargo_deny "$fixture/bin/cargo-deny" 0.20.2
out="$(run_ensure "$fixture/bin")"
grep -Fq 'using cargo-deny at' "$fixture/stdout" || fail 'pinned wrong: expected PATH message'
grep -Fq "$fixture/bin/cargo-deny" "$fixture/stdout" || fail 'pinned wrong: expected PATH binary path'
grep -Fq 'using pinned cargo-deny' "$fixture/stdout" && fail 'pinned wrong: must not use pinned'
if [ -f "$install_log" ]; then
  fail 'pinned wrong: must not install when PATH is correct'
fi

echo 'ci-preflight test: ensure_cargo_tool selection verified'
