#!/usr/bin/env bash
# Regression tests for the GNU per-test cargo runner (real Cargo, real rustc).
# shellcheck disable=SC2030,SC2031
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
fixture="$(mktemp -d "${TMPDIR:-/tmp}/jmodguard-timed-cargo-test.XXXXXX")"
trap 'rm -rf "$fixture"' EXIT

# shellcheck source=scripts/ci-cargo-log.sh
source "$root/scripts/ci-cargo-log.sh"
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

esc=$(printf '\033')
colored_log="$fixture/colored-compile.log"
printf '%s[1;32m   Compiling timed-runner-probe v0.1.0 (%s/ws/probe)%s[0m\n' "$esc" "$fixture" "$esc" >"$colored_log"
ci_cargo_log_has_compiling "$colored_log" || fail 'colored Compiling line must be detected after ANSI strip'
printf '   Finished test [unoptimized + debuginfo] target(s) in 0.01s\n' >"$fixture/no-compile.log"
if ci_cargo_log_has_compiling "$fixture/no-compile.log"; then
  fail 'Finished-only log must not match compile detector'
fi
log="$(mktemp)"
printf '%s[32m   Compiling fake v0.0.0%s[0m\n' "$esc" "$esc" >"$log"
if ! ci_cargo_log_has_compiling "$log"; then
  fail 'compile guard must trip on colored Compiling output'
fi
rm -f "$log"

large_log="$fixture/large-compile.log"
{
  printf '   Compiling big-crate v0.1.0 (/tmp/ws)\n'
  head -c 210000 </dev/zero | tr '\0' 'x'
} >"$large_log"
ci_cargo_log_has_compiling "$large_log" || fail 'large log with Compiling first line must match under pipefail'
large_colored="$fixture/large-compile-colored.log"
{
  printf '%s[1;32m   Compiling big-crate v0.1.0 (/tmp/ws)%s[0m\n' "$esc" "$esc"
  head -c 210000 </dev/zero | tr '\0' 'x'
} >"$large_colored"
ci_cargo_log_has_compiling "$large_colored" || fail 'large colored log with Compiling first line must match under pipefail'
large_plain="$fixture/large-no-compile.log"
head -c 210000 </dev/zero | tr '\0' 'y' >"$large_plain"
if ci_cargo_log_has_compiling "$large_plain"; then
  fail 'large log without Compiling must not match under pipefail'
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
export JMODGUARD_TEST_TIMEOUT_SEC=180

# Cancel path: TERM to runner must kill the setsid session (by pgid) and exit 143.
cancel_slow="$fixture/cancel-slow.sh"
cat >"$cancel_slow" <<'SH'
#!/bin/bash
set -euo pipefail
( sleep 360 & echo "$!" >"$CANCEL_CHILD_PID_FILE" )
sleep 999
SH
chmod +x "$cancel_slow"
export CANCEL_CHILD_PID_FILE="$fixture/cancel-child.pid"
cancel_runner="$fixture/cancel-runner.sh"
cat >"$cancel_runner" <<SH
#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=scripts/ci-run-timed-cargo-test.sh
source "$root/scripts/ci-run-timed-cargo-test.sh"
export JMODGUARD_TEST_TIMEOUT_SEC=180
export CANCEL_CHILD_PID_FILE="$fixture/cancel-child.pid"
run_with_test_timeout cancel-probe bash "$cancel_slow"
SH
chmod +x "$cancel_runner"
set +o errexit
"$cancel_runner" &
cancel_runner_pid=$!
sleep 2
kill -TERM "$cancel_runner_pid" 2>/dev/null || true
wait "$cancel_runner_pid"
cancel_rc=$?
set -o errexit
[ "$cancel_rc" -eq 143 ] || fail "cancel probe expected exit 143 after TERM, got $cancel_rc"
[ -f "$CANCEL_CHILD_PID_FILE" ] || fail 'cancel probe: missing child pid file'
cancel_child_pid="$(cat "$CANCEL_CHILD_PID_FILE")"
if kill -0 "$cancel_child_pid" 2>/dev/null; then
  fail "cancel probe: child $cancel_child_pid still alive after TERM"
fi
if kill -0 "$cancel_runner_pid" 2>/dev/null; then
  fail "cancel probe: runner $cancel_runner_pid still alive"
fi

ws="$fixture/ws"
mkdir -p "$ws/probe/src" "$ws/doctest-probe/src" "$ws/buildrs-probe/src"
cat >"$ws/Cargo.toml" <<'EOF'
[workspace]
resolver = "2"
members = ["probe", "doctest-probe", "buildrs-probe", "bin-only-probe"]
EOF
mkdir -p "$ws/bin-only-probe/src"
cat >"$ws/bin-only-probe/Cargo.toml" <<'EOF'
[package]
name = "timed-bin-only-probe"
version = "0.1.0"
edition = "2021"
autobins = false

[[bin]]
name = "timed-bin-only-probe"
path = "src/main.rs"
EOF
cat >"$ws/bin-only-probe/src/main.rs" <<'RS'
fn main() {}
RS
printf 'ok\n' >"$ws/probe/probe-data.txt"
cat >"$ws/probe/Cargo.toml" <<'EOF'
[package]
name = "timed-runner-probe"
version = "0.1.0"
edition = "2021"
EOF
cat >"$ws/probe/src/lib.rs" <<'RS'
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
cat >"$ws/doctest-probe/Cargo.toml" <<'EOF'
[package]
name = "timed-doctest-probe"
version = "0.1.0"
edition = "2021"
EOF
cat >"$ws/doctest-probe/src/lib.rs" <<'RS'
/// ```
/// assert_eq!(2 + 2, 4);
/// ```
pub fn documented() {}
RS
cat >"$ws/buildrs-probe/Cargo.toml" <<'EOF'
[package]
name = "timed-buildrs-probe"
version = "0.1.0"
edition = "2021"
build = "build.rs"
EOF
cat >"$ws/buildrs-probe/build.rs" <<'RS'
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
}
RS
cat >"$ws/buildrs-probe/src/lib.rs" <<'RS'
#[test]
fn build_rs_crate_passes() {
    assert!(true);
}
RS

export CARGO_TARGET_DIR="$fixture/target"
export RUSTFLAGS='-C prefer-dynamic'
export JMODGUARD_TEST_TIMEOUT_SEC=180
export CARGO_TERM_COLOR=always
cd "$ws"

build_log="$fixture/build.log"
if ! cargo test -p timed-runner-probe --lib --no-run >"$build_log" 2>&1; then
  cat "$build_log" >&2
  fail 'probe crate failed to build'
fi
ci_cargo_log_has_compiling "$build_log" || fail 'expected initial compile in build log (with CARGO_TERM_COLOR=always)'

color_before="${CARGO_TERM_COLOR:-}"
run_log="$fixture/run.log"
set +e
run_timed_cargo_test_gnu -p timed-runner-probe --lib -- >"$run_log" 2>&1
run_rc=$?
set -e
color_after="${CARGO_TERM_COLOR:-}"
[ "$color_before" = "$color_after" ] || fail "CARGO_TERM_COLOR changed in caller (before=$color_before after=$color_after)"
[ "${CARGO_TERM_COLOR:-}" = always ] || fail 'caller CARGO_TERM_COLOR must remain always'
if [ "$run_rc" -ne 0 ]; then
  cat "$run_log" >&2
  fail "run_timed_cargo_test_gnu failed with $run_rc"
fi
if ci_cargo_log_has_compiling "$run_log"; then
  cat "$run_log" >&2
  fail 'per-test cargo test runs must not recompile (same CARGO_TARGET_DIR/profile)'
fi
grep -q 'test result: ok' "$run_log" || fail 'expected passing test summary in run log'

doctest_log="$fixture/doctest-run.log"
set +e
run_timed_cargo_test_gnu -p timed-doctest-probe -- >"$doctest_log" 2>&1
doctest_rc=$?
set -e
[ "$doctest_rc" -eq 0 ] || { cat "$doctest_log" >&2; fail "doctest probe failed with $doctest_rc"; }

if ! command -v jq >/dev/null 2>&1; then
  fail 'jq is required for timed-cargo contract tests'
fi
real_cargo="$(command -v cargo)"
CARGO_INVOCATION_LOG="$fixture/cargo-invocations.log"
: >"$CARGO_INVOCATION_LOG"
mkdir -p "$fixture/bin"
cat >"$fixture/bin/cargo" <<SH
#!/usr/bin/env bash
printf '%s\n' "\$*" >>"$CARGO_INVOCATION_LOG"
exec "$real_cargo" "\$@"
SH
chmod +x "$fixture/bin/cargo"
PATH="$fixture/bin:$PATH"
contract_log="$fixture/contract-no-target.log"
set +e
run_timed_cargo_test_gnu -p timed-doctest-probe -- >"$contract_log" 2>&1
contract_rc=$?
set -e
[ "$contract_rc" -eq 0 ] || { cat "$contract_log" >&2; fail "contract no-target runner failed with $contract_rc"; }
no_run_line="$(grep ' --no-run' "$CARGO_INVOCATION_LOG" | head -n1 || true)"
[ -n "$no_run_line" ] || fail 'contract: expected cargo test --no-run prebuild'
case "$no_run_line" in
  *' --doc '*) fail 'contract: --no-run prebuild must not include --doc' ;;
esac
grep -q ' --doc ' "$CARGO_INVOCATION_LOG" || fail 'contract: expected separate cargo test --doc invocation'
if grep -E ' --doc .* --no-run | --no-run .* --doc ' "$CARGO_INVOCATION_LOG" >/dev/null; then
  fail 'contract: cargo must not combine --doc with --no-run'
fi
: >"$CARGO_INVOCATION_LOG"
bin_log="$fixture/contract-bin-only.log"
set +e
run_timed_cargo_test_gnu -p timed-bin-only-probe -- >"$bin_log" 2>&1
bin_rc=$?
set -e
[ "$bin_rc" -eq 0 ] || { cat "$bin_log" >&2; fail "bin-only contract runner failed with $bin_rc"; }
if grep -q ' --doc ' "$CARGO_INVOCATION_LOG"; then
  fail 'contract: bin-only package must skip cargo test --doc'
fi
run_timed_cargo_test_package_has_lib -p timed-bin-only-probe && fail 'bin-only metadata must not report lib target'
run_timed_cargo_test_package_has_lib -p timed-doctest-probe || fail 'doctest probe must have lib target'
PATH="${PATH#"$fixture/bin:"}"

buildrs_log="$fixture/buildrs-run.log"
set +e
run_timed_cargo_test_gnu -p timed-buildrs-probe --lib -- >"$buildrs_log" 2>&1
buildrs_rc=$?
set -e
[ "$buildrs_rc" -eq 0 ] || { cat "$buildrs_log" >&2; fail "build.rs probe failed with $buildrs_rc"; }
if ci_cargo_log_has_compiling "$buildrs_log"; then
  cat "$buildrs_log" >&2
  fail 'build.rs crate per-test runs must not false-trip compile guard'
fi

printf 'pub fn ci_touch_%s() {}\n' "$(date +%s)" >>"$ws/probe/src/lib.rs"
set +e
run_cargo_test_timed recompile-probe -p timed-runner-probe --lib -- --exact cwd_is_manifest_dir --test-threads=1 \
  >"$fixture/recompile.log" 2>&1
guard_rc=$?
set -e
[ "$guard_rc" -eq 1 ] || fail "compile guard expected exit 1 after source change, got $guard_rc"
ci_cargo_log_has_compiling "$fixture/recompile.log" || fail 'recompile log should contain Compiling for guard test'

if [ -n "${JMODGUARD_ACTIVE_TEST_SESSION_PGID:-}" ]; then
  fail 'normal pass must clear active test session pgid'
fi

echo 'ci-timed-cargo-test: cargo context, prefer-dynamic, color-safe compile guard, timeout and cancel cleanup verified'
