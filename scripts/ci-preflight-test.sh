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
grep -Fxq 'ci-preflight: Node tests require setsid (install util-linux)' "$fixture/frontend.log" \
  || fail 'missing setsid: frontend preflight must explain the prerequisite'
echo 'ci-preflight test: missing setsid rejected before frontend setup'

# Run the real preflight and session runner. The node stub runs the checked-in
# workflow contract script from the repository root, plus the flake checker body
# in a minimal ESLint fixture without npm package installation.
frontend_root="$fixture/frontend"
mkdir -p "$frontend_root/scripts" "$frontend_root/tools/eslint/node_modules/eslint"
git init -q "$frontend_root"
git -C "$frontend_root" -c user.name=Fixture -c user.email=fixture@example.invalid -c commit.gpgsign=false \
  commit --allow-empty -qm 'Fixture base'
fixture_base="$(git -C "$frontend_root" rev-parse HEAD)"
cp "$root/scripts/check-test-flakes.mjs" "$frontend_root/scripts/"
: >"$frontend_root/tools/test-flake-allowlist.tsv"
for gate in select-checks-test.sh public-install-acceptance-test.sh dependabot-config-test.sh; do
  printf 'exit 0\n' >"$frontend_root/scripts/$gate"
done
printf '{"type":"module"}\n' >"$frontend_root/tools/eslint/package.json"
printf 'export const frontendTests = [];\n' >"$frontend_root/tools/eslint/eslint.config.js"
cat >"$frontend_root/tools/eslint/node_modules/eslint/index.js" <<'JS'
exports.ESLint = class {
  async lintFiles() { return []; }
};
JS
# Record and stop test launches that bypass the PATH shim before their suites run.
cat >"$fixture/node-test-probe.cjs" <<'JS'
if (process.execArgv.includes("--test")) {
  const args = [...process.execArgv, ...process.argv.slice(1)];
  require("node:fs").appendFileSync(
    process.env.CI_PREFLIGHT_TEST_NODE_LOG, "Unexpected real Node test: " + JSON.stringify(args) + "\n");
  process.exit(1);
}
JS
cat >"$fixture/node-noisy-preload.cjs" <<'JS'
console.log("ci-preflight-test: noisy preload must not affect node major probe");
JS
export CI_PREFLIGHT_TEST_REAL_NODE CI_PREFLIGHT_TEST_ROOT="$root" CI_PREFLIGHT_TEST_REAL_PATH="$PATH"
export CI_PREFLIGHT_TEST_NODE_PROBE="$fixture/node-test-probe.cjs"
CI_PREFLIGHT_TEST_REAL_NODE="$(command -v node)"
for helper in ci-ensure-cargo-tool.sh ci-run-timed-cargo-test.sh ci-cargo-log.sh ci-frontend-node-timeout.sh require-host-eslint-node.sh; do
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
  -p)
    # Simulate the major value while preserving Node startup and preload output.
    exec "$CI_PREFLIGHT_TEST_REAL_NODE" -p 'process.env.CI_PREFLIGHT_TEST_NODE_MAJOR'
    ;;
  scripts/ci-workflow-test.mjs)
    exec env -u JMODGUARD_DIFF_BASE -u JMODGUARD_DIFF_HEAD -u JMODGUARD_PREFLIGHT_BASE \
      PATH="$CI_PREFLIGHT_TEST_REAL_PATH" NODE_OPTIONS="--require=\"$CI_PREFLIGHT_TEST_NODE_PROBE\"" \
      "$CI_PREFLIGHT_TEST_REAL_NODE" "$CI_PREFLIGHT_TEST_ROOT/scripts/ci-workflow-test.mjs"
    ;;
  --test)
    printf -v quoted_args ' %q' "$@"
    printf 'ci-preflight: node%s\n' "$quoted_args" >>"$CI_PREFLIGHT_TEST_NODE_LOG"
    read -r session < <(ps -o sid= -p "$$")
    if [[ "$session" != "$$" || "${JMODGUARD_TEST_TIMEOUT_SEC:-}" != 600 ||
      " ${NODE_OPTIONS:-} " != *" --max-old-space-size=2048 "* ]]; then
      printf 'Node test lacks its own 600s session or 2 GiB heap cap: %s\n' "$*" >&2
      exit 1
    fi
    ;;
  *) exec "$CI_PREFLIGHT_TEST_REAL_NODE" "$@" ;;
esac
SH
chmod +x "$fixture/bin/npm" "$fixture/bin/node"

# Compare all observed invocations with the files routed through the helper.
# A direct node --test call will still be recorded and fail the comparison.
required_node_test_manifest="$root/scripts/ci-preflight-node-test-files.txt"
[ -f "$required_node_test_manifest" ] || fail "missing Node test manifest: $required_node_test_manifest"
required_node_test_files="$(
  grep -Ev '^[[:space:]]*(#|$)' "$required_node_test_manifest"
)"
[ -n "$required_node_test_files" ] || fail 'Node test manifest has no entries'
node_test_files="$(sed -nE 's/^[[:blank:]]*run_node_test[[:blank:]]+([^[:blank:]]+).*/\1/p' "$root/scripts/ci-preflight.sh")"
[ -n "$node_test_files" ] || fail 'no Node test files found in preflight'
if ! diff -u <(printf '%s\n' "$required_node_test_files" | LC_ALL=C sort) \
  <(printf '%s\n' "$node_test_files" | LC_ALL=C sort); then
  fail 'run_node_test call sites must match scripts/ci-preflight-node-test-files.txt'
fi

# Add a fixture-only call through the real helper so that an unquoted log
# differs from the captured argv even on Node versions without a timeout flag.
spaced_file='scripts/argv fixture/frontend-state-test.mjs'
sed -E '/^[[:blank:]]*run_node_test[[:blank:]]+scripts\/frontend-state-test\.mjs([[:blank:]]|$)/a\
  run_node_test "$CI_PREFLIGHT_TEST_SPACED_FILE" || exit $?
' "$root/scripts/ci-preflight.sh" >"$frontend_root/scripts/ci-preflight.sh"

contract_test_files=(scripts/check-test-flakes.test.mjs scripts/pullfrog-command-check.test.mjs)

run_fixture_preflight() {
  local major="$1" label="$2"
  shift 2
  local node_log="$fixture/node-argv"
  : >"$node_log"
  if ! (
    cd "$frontend_root"
    # The hosted event's revisions are for the checkout, not this repository.
    PATH="$fixture/bin:$PATH" CI_PREFLIGHT_TEST_NODE_MAJOR="$major" CI_PREFLIGHT_TEST_NODE_LOG="$node_log" \
      NODE_OPTIONS="--require=\"$fixture/node-noisy-preload.cjs\" --require=\"$CI_PREFLIGHT_TEST_NODE_PROBE\"" \
      JMODGUARD_DIFF_BASE="$fixture_base" JMODGUARD_DIFF_HEAD="$fixture_base" \
      CI_PREFLIGHT_TEST_SPACED_FILE="$spaced_file" /bin/bash scripts/ci-preflight.sh "$@"
  ) >"$fixture/preflight.log" 2>&1; then
    cat "$fixture/preflight.log" >&2
    fail "$label preflight failed"
  fi
}

check_node_invocations() {
  local label="$1" expected_timeout="$2"
  shift 2
  local node_log="$fixture/node-argv"
  local -a test_args=(--test) expected=()
  if [ -n "$expected_timeout" ]; then
    test_args+=("$expected_timeout")
  fi
  local file quoted_args
  for file in "$@"; do
    printf -v quoted_args ' %q' "${test_args[@]}" "$file"
    expected+=("ci-preflight: node$quoted_args")
  done
  # Expect one invocation per file and keep duplicates in the observed output.
  LC_ALL=C sort -o "$node_log" "$node_log"
  if ! diff -u <(printf '%s\n' "${expected[@]}" | LC_ALL=C sort -u) "$node_log"; then
    fail "$label: unexpected Node test invocations"
  fi

  if ! diff -u "$node_log" \
    <(grep -E '^ci-preflight: node --test( |$)' "$fixture/preflight.log" | LC_ALL=C sort); then
    fail "$label: command log differs from actual Node test arguments"
  fi
}

run_fixture_preflight 24 'contracts/Pullfrog' --only contracts --only pullfrog
check_node_invocations 'contracts/Pullfrog' --test-timeout=60000 "${contract_test_files[@]}"
grep -Fq 'ci-workflow-test: ok' "$fixture/preflight.log" \
  || fail 'checked-in workflow contract script must complete'
echo 'ci-preflight test: contracts, pullfrog, and workflow Node paths verified'

# Reverse the manifest order so a positional comparison fails.
mapfile -t frontend_test_files < <(
  grep -Fvx -f <(printf '%s\n' "${contract_test_files[@]}") <<<"$required_node_test_files" | tac
)

check_node_timeout_flag() {
  local major="$1" expected_timeout="$2" label="frontend Node major '$1'"
  run_fixture_preflight "$major" "$label" --only frontend
  check_node_invocations "$label" "$expected_timeout" "${frontend_test_files[@]}" "$spaced_file"
}

# shellcheck source=scripts/ci-frontend-node-timeout.sh
source "$root/scripts/ci-frontend-node-timeout.sh"
[ "$frontend_node_test_timeout_min_major" = 24 ] || fail 'frontend Node timeout must start at Node 24 (see docs/CI.md)'

check_node_timeout_flag 20 ''
check_node_timeout_flag 22 ''
check_node_timeout_flag 23 ''
check_node_timeout_flag 24 --test-timeout=60000
check_node_timeout_flag 26 --test-timeout=60000
check_node_timeout_flag '24junk' ''
check_node_timeout_flag '' ''
echo 'ci-preflight test: Node timeout flag, session and heap gating verified'

# Drive the real API and integration lanes with tool stubs in the fixture repo.
# Keep the Cargo stub separate from the tool-installation cases above.
runner_bin="$fixture/runner-bin"
mkdir -p "$runner_bin"
printf '#!/bin/bash\nexit 0\n' >"$runner_bin/cargo-nextest"
cat >"$runner_bin/cargo" <<'SH'
#!/bin/bash
set -euo pipefail
printf -v argv ' %q' "$@"
printf 'cargo%s\n' "$argv" >>"$CI_PREFLIGHT_TEST_CARGO_LOG"
case "$*" in
  'nextest run --help')
    printf '%s\n' '--config-file' '--run-ignored' '--no-capture'
    ;;
  'nextest run '*)
    if [[ " $* " == *' --no-default-features '* ]]; then
      exit "$CI_PREFLIGHT_TEST_NEXTEST_RC"
    fi
    ;;
  'metadata --no-deps --format-version 1')
    printf '%s\n' '{"packages":[{"name":"jmodguard-api","targets":[{"kind":["lib"]}]},{"name":"jmodguard-integration-tests","targets":[{"kind":["lib"]}]}]}'
    ;;
  'build --locked -p jmodguard-'*' --message-format=json-render-diagnostics')
    printf '{"reason":"compiler-artifact","target":{"name":"%s"},"executable":"/fixture/%s"}\n' "$4" "$4"
    ;;
  'test --locked -p jmodguard-integration-tests --test dual_process_smoke -- --list --ignored --format terse --nocapture')
    printf '%s\n' 'smoke_fixture: test'
    ;;
  'test --locked -p jmodguard-integration-tests --test dual_process_smoke -- --exact smoke_fixture --ignored --test-threads=1 --nocapture')
    # The per-test launcher creates a session for the smoke process and its children.
    read -r session < <(ps -o sid= -p "$$")
    if [ "$session" != "$$" ]; then
      echo 'ci-preflight test cargo: ignored smoke must run in its own timed test session' >&2
      exit 1
    fi
    ;;
  'update --workspace --locked'|'clippy --locked '*|'build --locked '*|\
  'test --locked '*' --doc -- --list --format terse'|\
  'test --locked '*' --doc -- --list --ignored --format terse'|\
  'test --locked -p jmodguard-integration-tests --test dual_process_smoke --no-run')
    ;;
  *)
    echo "ci-preflight test cargo: unexpected:$argv" >&2
    exit 1
    ;;
esac
SH
chmod +x "$runner_bin/cargo" "$runner_bin/cargo-nextest"

cargo_log="$fixture/cargo-argv"
run_fixture_cargo_preflight() {
  local nextest_rc="$1"
  shift
  : >"$cargo_log"
  (
    cd "$frontend_root"
    PATH="$runner_bin:$fixture/bin:$PATH" NEXTEST_TEST_THREADS=2 \
      CI_PREFLIGHT_TEST_CARGO_LOG="$cargo_log" CI_PREFLIGHT_TEST_NEXTEST_RC="$nextest_rc" \
      /bin/bash "$root/scripts/ci-preflight.sh" "$@"
  ) >"$fixture/cargo-preflight.log" 2>&1
}

if ! run_fixture_cargo_preflight 0 --only api --only integration; then
  cat "$fixture/cargo-preflight.log" >&2
  fail 'nextest and ignored-smoke preflight must pass with successful tools'
fi
grep -Eq '^cargo nextest run .* -p jmodguard-api --no-default-features --lib$' "$cargo_log" \
  || fail 'nextest must forward the API package, no-default-features and lib selections'
grep -Fxq 'cargo test --locked -p jmodguard-integration-tests --test dual_process_smoke -- --exact smoke_fixture --ignored --test-threads=1 --nocapture' "$cargo_log" \
  || fail 'the ignored dual-process smoke must use the GNU per-test runner'

preflight_rc=0
run_fixture_cargo_preflight 101 --only api || preflight_rc=$?
[ "$preflight_rc" -eq 101 ] || fail "nextest exit 101 must reach the caller, got $preflight_rc"
echo 'ci-preflight test: nextest argv, ignored-test fallback and failure propagation verified'

bash "$root/scripts/ci-preflight.sh" --audit-expiry-self-check
