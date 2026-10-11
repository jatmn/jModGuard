#!/usr/bin/env bash
# Regression tests for the GNU per-test cargo runner (real Cargo, real rustc).
# shellcheck disable=SC2030,SC2031
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
fixture="$(mktemp -d "${TMPDIR:-/tmp}/jmodguard-timed-cargo-test.XXXXXX")"
trap 'rm -rf "$fixture"' EXIT

# shellcheck source=scripts/ci-run-timed-cargo-test.sh
source "$root/scripts/ci-run-timed-cargo-test.sh"

fail() {
  printf 'ci-timed-cargo-test: %s\n' "$*" >&2
  exit 1
}

if ! command -v timeout >/dev/null 2>&1; then
  echo 'ci-timed-cargo-test: GNU timeout not installed; skipping'
  exit 0
fi

# Timed-out command must not leave a background child running.
hang_script="$fixture/hang.sh"
cat >"$hang_script" <<'SH'
#!/bin/bash
set -euo pipefail
( sleep 360 & echo "$!" >"$CHILD_PID_FILE" )
sleep 999
SH
chmod +x "$hang_script"
export CHILD_PID_FILE="$fixture/child.pid"
export JMODGUARD_TEST_TIMEOUT_SEC=1
set +o errexit
run_with_test_timeout hang-probe bash "$hang_script" || hang_rc=$?
hang_rc=${hang_rc:-0}
set -o errexit
[ "$hang_rc" -eq 124 ] || fail "hang probe expected exit 124, got $hang_rc"
[ -f "$CHILD_PID_FILE" ] || fail 'hang probe: missing child pid file'
child_pid="$(cat "$CHILD_PID_FILE")"
if kill -0 "$child_pid" 2>/dev/null; then
  fail "hang probe: child $child_pid still alive after timeout"
fi

ws="$fixture/ws"
mkdir -p "$ws/src"
printf 'ok\n' >"$ws/probe-data.txt"
cat >"$ws/Cargo.toml" <<'EOF'
[package]
name = "timed-runner-probe"
version = "0.1.0"
edition = "2021"
EOF
cat >"$ws/src/lib.rs" <<'RS'
use std::path::PathBuf;

#[test]
fn cwd_is_manifest_dir() {
    assert_eq!(
        std::env::current_dir().unwrap(),
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    );
}

#[test]
fn reads_package_relative_file() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("probe-data.txt");
    assert_eq!(std::fs::read_to_string(path).unwrap().trim(), "ok");
}
RS

export CARGO_TARGET_DIR="$fixture/target"
export RUSTFLAGS='-C prefer-dynamic'
export JMODGUARD_TEST_TIMEOUT_SEC=180
cd "$ws"

build_log="$fixture/build.log"
if ! cargo test -p timed-runner-probe --lib --no-run >"$build_log" 2>&1; then
  cat "$build_log" >&2
  fail 'probe crate failed to build'
fi
grep -qE '^   Compiling ' "$build_log" || fail 'expected initial compile in build log'

run_log="$fixture/run.log"
set +e
run_timed_cargo_test_gnu -p timed-runner-probe --lib -- >"$run_log" 2>&1
run_rc=$?
set -e
if [ "$run_rc" -ne 0 ]; then
  cat "$run_log" >&2
  fail "run_timed_cargo_test_gnu failed with $run_rc"
fi
if grep -qE '^   Compiling ' "$run_log"; then
  cat "$run_log" >&2
  fail 'per-test cargo test runs must not recompile (same CARGO_TARGET_DIR/profile)'
fi
grep -q 'test result: ok' "$run_log" || fail 'expected passing test summary in run log'

echo 'ci-timed-cargo-test: cargo context, prefer-dynamic, timeout cleanup verified'
