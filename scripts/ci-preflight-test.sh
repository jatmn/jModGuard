#!/usr/bin/env bash
# Exercise ensure_cargo_tool without running supply-chain gates.
# shellcheck disable=SC2030,SC2031
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
tool_root="$fixture/job-local-tools"

cat >"$fixture/bin/cargo" <<'SH'
#!/bin/bash
set -euo pipefail
if [ "${1:-}" = install ]; then
  install_root=""
  install_target=""
  shift
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --root)
        install_root="$2"
        shift 2
        ;;
      --target-dir)
        install_target="$2"
        shift 2
        ;;
      --locked)
        shift
        ;;
      --version)
        shift 2
        ;;
      *)
        shift
        ;;
    esac
  done
  [ -n "$install_root" ] || { echo 'ci-preflight test cargo: missing --root' >&2; exit 1; }
  printf 'argv=%s\n' "install --root $install_root --target-dir $install_target" >>"$CI_PREFLIGHT_TEST_INSTALL_LOG"
  mkdir -p "$install_root/bin"
  cat >"$install_root/bin/cargo-deny" <<'INNER'
#!/bin/bash
if [ "${1:-}" = --version ]; then
  echo "cargo-deny 0.20.2"
  exit 0
fi
exit 0
INNER
  chmod +x "$install_root/bin/cargo-deny"
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
    export JMODGUARD_PREFLIGHT_CARGO_TOOL_ROOT="$tool_root"
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

# Pinned missing, PATH wrong version -> job-local --root install; shared ~/.cargo untouched.
fake_home="$fixture/fake-home"
mkdir -p "$fake_home/.cargo/bin"
printf 'marker\n' >"$fake_home/.cargo/bin/shared-marker"
write_cargo_deny "$fixture/bin/cargo-deny" 9.9.9 wrong-path
rm -rf "$tool_root"
set +e
(
  export JMODGUARD_CI_PREFIX="$fixture/opt"
  export HOME="$fake_home"
  export CARGO_HOME="$fake_home/.cargo"
  export JMODGUARD_PREFLIGHT_CARGO_TOOL_ROOT="$tool_root"
  export CI_PREFLIGHT_TEST_INSTALL_LOG="$install_log"
  export PATH="$fixture/bin:/usr/bin:/bin"
  rm -f "$install_log"
  ensure_cargo_tool cargo-deny cargo-deny 0.20.2
) >"$fixture/stdout" 2>&1
install_rc=$?
set -e
if [ "$install_rc" -ne 0 ]; then
  cat "$fixture/stdout" >&2
  fail "path wrong: ensure_cargo_tool install failed with $install_rc"
fi
grep -Fq 'installing cargo-deny 0.20.2' "$fixture/stdout" || fail 'path wrong: expected install message'
[ -f "$install_log" ] || fail 'path wrong: expected cargo install'
grep -Fq -- '--root' "$install_log" || fail 'path wrong: expected --root install'
grep -Fq -- '--target-dir' "$install_log" || fail 'path wrong: expected --target-dir install'
grep -Fq "$tool_root" "$install_log" || fail 'path wrong: expected job-local root in install log'
[ -x "$tool_root/bin/cargo-deny" ] || fail 'path wrong: expected binary under job-local root'
cmp -s "$fake_home/.cargo/bin/shared-marker" - <<<marker || fail 'path wrong: shared cargo home marker was modified'
resolved="$(PATH="$tool_root/bin:$fixture/bin" command -v cargo-deny)"
case "$resolved" in
  "$tool_root/bin/cargo-deny") ;;
  *) fail "path wrong: expected job-local binary, got $resolved" ;;
esac

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

bash "$root/scripts/ci-preflight.sh" --audit-expiry-self-check
