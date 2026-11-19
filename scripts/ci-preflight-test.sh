#!/usr/bin/env bash
# Exercise preflight setup and ensure_cargo_tool without running the full gates.
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

# Hosted CI restores the job-local tool root (the actions/cache path) without
# putting it on PATH: the cached binary must be reused, not reinstalled.
empty_bin="$fixture/empty-bin"
cargo_only_bin="$fixture/cargo-only-bin"
mkdir -p "$empty_bin" "$cargo_only_bin"
cp "$fixture/bin/cargo" "$cargo_only_bin/cargo"
cached_stdout="$fixture/cached-stdout"
resolved_file="$fixture/cached-resolved"
(
  export JMODGUARD_CI_PREFIX="$fixture/opt"
  export CARGO_HOME="$fixture/cargo-home"
  export JMODGUARD_PREFLIGHT_CARGO_TOOL_ROOT="$tool_root"
  export CI_PREFLIGHT_TEST_INSTALL_LOG="$install_log"
  export PATH="$cargo_only_bin:/usr/bin:/bin"
  rm -f "$install_log"
  ensure_cargo_tool cargo-deny cargo-deny 0.20.2
  command -v cargo-deny >"$resolved_file"
) >"$cached_stdout" 2>&1
resolved="$(cat "$resolved_file")"
grep -Fq "using cached cargo-deny at $tool_root/bin/cargo-deny" "$cached_stdout" \
  || fail 'cached tool root: expected cached message with tool root path'
if [ -f "$install_log" ]; then
  fail 'cached tool root: must not install when the tool root has the pinned version'
fi
case "$resolved" in
  "$tool_root/bin/cargo-deny") ;;
  *) fail "cached tool root: expected the tool root binary on PATH, got $resolved" ;;
esac

# A wrong cached version must not be reused; the install path replaces it.
write_cargo_deny "$tool_root/bin/cargo-deny" 9.9.9 wrong-cached
rm -rf "$fixture/opt/cargo-deny-0.20.2"
out="$(run_ensure "$cargo_only_bin")"
grep -Fq "cached $tool_root/bin/cargo-deny is version 9.9.9, need 0.20.2" "$fixture/stdout" \
  || fail 'cached wrong: expected cached version mismatch message'
[ -f "$install_log" ] || fail 'cached wrong: expected cargo install'
grep -Fq "$tool_root" "$install_log" || fail 'cached wrong: expected job-local root in install log'

echo 'ci-preflight test: ensure_cargo_tool selection verified'

# Expose only the tools preflight needs to locate the checkout and load its helpers.
no_setsid_bin="$fixture/no-setsid-bin"
mkdir -p "$no_setsid_bin"
ln -s "$(command -v git)" "$no_setsid_bin/git"
ln -s "$(command -v dirname)" "$no_setsid_bin/dirname"
frontend_rc=0
PATH="$no_setsid_bin" /bin/bash "$root/scripts/ci-preflight.sh" --only frontend >"$fixture/frontend.log" 2>&1 \
  || frontend_rc=$?
[ "$frontend_rc" -eq 1 ] || fail "missing setsid: expected preflight exit 1, got $frontend_rc"
grep -Fxq 'ci-preflight: frontend Node tests require setsid (install util-linux)' "$fixture/frontend.log" \
  || fail 'missing setsid: frontend preflight must explain the prerequisite'
echo 'ci-preflight test: missing setsid rejected before frontend setup'

# Run the real frontend preflight and session runner with stubbed Node and
# dependency setup. Keep the fixture independent of installed npm packages.
frontend_root="$fixture/frontend"
mkdir -p "$frontend_root/scripts" "$frontend_root/tools"
git init -q "$frontend_root"
for helper in ci-ensure-cargo-tool.sh ci-run-timed-cargo-test.sh ci-cargo-log.sh require-host-eslint-node.sh; do
  ln -s "$root/scripts/$helper" "$frontend_root/scripts/$helper"
done
printf 'exit 0\n' >"$frontend_root/tools/eslint-web-selftest.sh"
cat >"$fixture/bin/npm" <<'SH'
#!/bin/bash
exit 0
SH
cat >"$fixture/bin/node" <<'SH'
#!/bin/bash
set -euo pipefail
case "$1" in
  -p) printf '%s\n' "$CI_PREFLIGHT_TEST_NODE_MAJOR" ;;
  -e|scripts/check-test-flakes.mjs) ;;
  --test) printf '%s\n' "$@" >>"$CI_PREFLIGHT_TEST_NODE_LOG" ;;
  *) exit 1 ;;
esac
SH
chmod +x "$fixture/bin/npm" "$fixture/bin/node"

# Compare all observed invocations with the files routed through the helper.
# A direct node --test call will still be recorded and fail the comparison.
frontend_test_files="$(sed -nE 's/^[[:blank:]]*run_frontend_node_test[[:blank:]]+([^[:blank:]]+).*/\1/p' "$root/scripts/ci-preflight.sh")"
[ -n "$frontend_test_files" ] || fail 'no frontend test files found in preflight'

check_node_timeout_flag() {
  local major="$1" expected_timeout="$2"
  local node_log="$fixture/node-argv"
  : >"$node_log"
  if ! (
    cd "$frontend_root"
    PATH="$fixture/bin:$PATH" CI_PREFLIGHT_TEST_NODE_MAJOR="$major" CI_PREFLIGHT_TEST_NODE_LOG="$node_log" \
      /bin/bash "$root/scripts/ci-preflight.sh" --only frontend
  ) >"$fixture/frontend.log" 2>&1; then
    cat "$fixture/frontend.log" >&2
    fail "Node major '${major}': frontend preflight failed"
  fi
  local -a test_args=(--test) expected=()
  if [ -n "$expected_timeout" ]; then
    test_args+=("$expected_timeout")
  fi
  local file
  while IFS= read -r file; do
    expected+=("${test_args[@]}" "$file")
  done <<<"$frontend_test_files"
  if ! diff -u <(printf '%s\n' "${expected[@]}") "$node_log"; then
    fail "Node major '${major}': unexpected frontend test arguments"
  fi

  # Read just the frontend-state invocation from the argument capture.
  local arg
  local -a actual_args=()
  while IFS= read -r arg; do
    if [ "$arg" = --test ]; then
      actual_args=()
    fi
    actual_args+=("$arg")
    [ "$arg" != scripts/frontend-state-test.mjs ] || break
  done <"$node_log"
  if ! diff -u <(printf 'ci-preflight: node%s\n' "$(printf ' %q' "${actual_args[@]}")") \
    <(grep -E '^ci-preflight: node .* scripts/frontend-state-test\.mjs$' "$fixture/frontend.log"); then
    fail "Node major '${major}': frontend command log differs from actual arguments"
  fi
}

check_node_timeout_flag 20 ''
check_node_timeout_flag 22 ''
check_node_timeout_flag 23 ''
check_node_timeout_flag 24 --test-timeout=60000
check_node_timeout_flag 26 --test-timeout=60000
check_node_timeout_flag '24junk' ''
check_node_timeout_flag '' ''
echo 'ci-preflight test: frontend Node timeout flag gating verified'

bash "$root/scripts/ci-preflight.sh" --audit-expiry-self-check
