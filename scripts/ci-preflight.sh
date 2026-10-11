#!/usr/bin/env bash
# Single source of truth for Buildkite check groups and the full local suite.
# The remote-database job (libSQL containers) stays separate.
# GitHub Actions workflows and GitHub-hosted CI runners are forbidden for CI.
# Allowed workflow files are the owner-command Pullfrog workflow and the
# Socket Security manifest scan. CI runs on Buildkite.
# Dependabot Updates dynamic workflows are not CI.
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
cd "$root"

JMODGUARD_CI_PREFIX="${JMODGUARD_CI_PREFIX:-/opt/jmodguard-ci}"

usage() {
  cat <<'EOF'
Usage: bash scripts/ci-preflight.sh [--base <git-revision>] [--only <group>]

Run the Buildkite test gates locally or on a Buildkite linux agent.
With no --only, run the full suite (local hooks and a complete check).
Buildkite uploads one step per selected group. Known groups: contracts,
frontend, fmt, dylint, api, bot, runtime, updater, core, integration,
boundaries, publish-tests, bootstrap, pullfrog, supply-chain.
By default the whitespace diff base is origin/main. Pass --base origin/<branch>
for a PR whose base is not main. Scheduled Buildkite builds set
JMODGUARD_AUDIT_FAIL_CLOSED=1 so cargo-audit fails the job.
EOF
}

base_ref="${JMODGUARD_PREFLIGHT_BASE:-origin/main}"
only_names=""
known_only="contracts frontend fmt dylint api bot runtime updater core integration boundaries publish-tests bootstrap pullfrog supply-chain"
while [ "$#" -gt 0 ]; do
  case "$1" in
    --base)
      [ "$#" -ge 2 ] || { usage >&2; exit 2; }
      base_ref="$2"
      shift 2
      ;;
    --only)
      [ "$#" -ge 2 ] || { usage >&2; exit 2; }
      case " ${known_only} " in
        *" $2 "*) ;;
        *) echo "ci-preflight: unknown --only group: $2" >&2; exit 2 ;;
      esac
      only_names="${only_names} $2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      usage >&2
      exit 2
      ;;
  esac
done

should_run() {
  if [ -z "${only_names}" ]; then
    return 0
  fi
  case " ${only_names} " in
    *" $1 "*) return 0 ;;
    *) return 1 ;;
  esac
}

if should_run contracts; then
  if ! grep -Fq 'bash .buildkite/select-checks.sh upload' "$root/.buildkite/pipeline.yml"; then
    echo 'ci-preflight: .buildkite/pipeline.yml must invoke bash .buildkite/select-checks.sh upload' >&2
    exit 1
  fi
  if ! grep -Fq 'bash scripts/ci-preflight.sh' "$root/.buildkite/pipeline.steps.yml"; then
    echo 'ci-preflight: .buildkite/pipeline.steps.yml must invoke bash scripts/ci-preflight.sh' >&2
    exit 1
  fi
fi

if should_run contracts; then
for hook in pre-commit pre-push; do
  hook_path="$root/.githooks/$hook"
  if [ ! -f "$hook_path" ]; then
    continue
  fi
  if [ ! -x "$hook_path" ]; then
    echo "ci-preflight: .githooks/$hook must be executable (git skips non-executable hooks)" >&2
    exit 1
  fi
  if ! grep -Fq 'scripts/ci-preflight.sh' "$hook_path"; then
    echo "ci-preflight: .githooks/$hook must invoke scripts/ci-preflight.sh" >&2
    exit 1
  fi
done
fi

require_command() {
  local command="$1"
  local install="$2"
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "ci-preflight: missing $command; install it with: $install" >&2
    exit 2
  fi
}

ensure_cargo_tool() {
  local bin="$1"
  local crate="$2"
  local version="$3"
  local pinned="$JMODGUARD_CI_PREFIX/${crate}-${version}/bin/${bin}"
  local pinned_ver out

  if [ -x "$pinned" ]; then
    out="$("$pinned" --version 2>&1 || true)"
    pinned_ver="$(printf '%s\n' "$out" | awk '{print $NF}')"
    if [ "$pinned_ver" = "$version" ]; then
      PATH="$(dirname "$pinned"):$PATH"
      export PATH
      echo "ci-preflight: $bin version: $out"
      return 0
    fi
    echo "ci-preflight: pinned $pinned absent or not $version; using PATH"
  else
    echo "ci-preflight: pinned $pinned absent or not $version; using PATH"
  fi

  if command -v "$bin" >/dev/null 2>&1; then
    out="$("$bin" --version 2>&1 || true)"
    echo "ci-preflight: $bin version: $out"
    return 0
  fi
  echo "ci-preflight: installing $crate $version"
  cargo install "$crate" --locked --version "$version"
  out="$("$bin" --version 2>&1 || true)"
  echo "ci-preflight: $bin version: $out"
}

run_timed_cargo_test() {
  if [ "${1:-}" != cargo ] || [ "${2:-}" != test ]; then
    echo 'ci-preflight: run_timed_cargo_test expects cargo test' >&2
    exit 1
  fi
  shift 2

  local cargo_args=()
  local harness=()
  local past_dd=0 arg
  for arg in "$@"; do
    if [ "$past_dd" -eq 1 ]; then
      harness+=("$arg")
      continue
    fi
    if [ "$arg" = -- ]; then
      past_dd=1
      continue
    fi
    cargo_args+=("$arg")
  done

  local nextest_help nextest_ok=0 cfg
  if command -v cargo-nextest >/dev/null 2>&1; then
    nextest_help="$(cargo nextest run --help 2>&1 || true)"
    if printf '%s\n' "$nextest_help" | grep -Fq -- '--config-file'; then
      nextest_ok=1
    fi
  fi

  local can_nextest=0
  if [ "$nextest_ok" -eq 1 ]; then
    local bad_harness=0 h locked=0 workspace=0 has_pkg=0
    for h in "${harness[@]}"; do
      case "$h" in
        --include-ignored|--ignored|--nocapture) ;;
        *) bad_harness=1 ;;
      esac
    done
    if [ "$bad_harness" -eq 0 ]; then
      local i=0 need_val=0
      while [ "$i" -lt "${#cargo_args[@]}" ]; do
        arg="${cargo_args[$i]}"
        if [ "$need_val" -eq 1 ]; then
          need_val=0
          i=$((i + 1))
          continue
        fi
        case "$arg" in
          --locked) locked=1 ;;
          --workspace) workspace=1 ;;
          --no-default-features|--lib) ;;
          -p|--test)
            need_val=1
            [ "$arg" = -p ] && has_pkg=1
            ;;
          *)
            bad_harness=1
            ;;
        esac
        i=$((i + 1))
      done
      [ "$need_val" -eq 1 ] && bad_harness=1
      if [ "$bad_harness" -eq 0 ] && [ "$locked" -eq 1 ] && { [ "$workspace" -eq 1 ] || [ "$has_pkg" -eq 1 ]; }; then
        can_nextest=1
      fi
    fi
  fi

  if [ "$can_nextest" -eq 1 ]; then
    local nextest_args=(nextest run --locked --profile default)
    cfg="$(mktemp)"
    cat >"$cfg" <<'EOF'
[profile.default.slow-timeout]
period = "60s"
terminate-after = 3
EOF
    nextest_args+=(--config-file "$cfg" --failure-output immediate)
    local i=0
    while [ "$i" -lt "${#cargo_args[@]}" ]; do
      arg="${cargo_args[$i]}"
      case "$arg" in
        --locked) ;;
        --workspace) nextest_args+=(--workspace) ;;
        --no-default-features) nextest_args+=(--no-default-features) ;;
        -p)
          i=$((i + 1))
          nextest_args+=(-p "${cargo_args[$i]}")
          ;;
        --lib) nextest_args+=(--lib) ;;
        --test)
          i=$((i + 1))
          nextest_args+=(--test "${cargo_args[$i]}")
          ;;
        *) nextest_args+=("$arg") ;;
      esac
      i=$((i + 1))
    done
    local h
    for h in "${harness[@]}"; do
      case "$h" in
        --include-ignored)
          if printf '%s\n' "$nextest_help" | grep -Fq -- '--run-ignored'; then
            nextest_args+=(--run-ignored all)
          else
            can_nextest=0
          fi
          ;;
        --nocapture)
          if printf '%s\n' "$nextest_help" | grep -Fq -- '--no-capture'; then
            nextest_args+=(--no-capture)
          else
            can_nextest=0
          fi
          ;;
        --ignored) can_nextest=0 ;;
      esac
    done
    if [ "$can_nextest" -eq 1 ]; then
      echo 'ci-preflight: per-test timeout via cargo-nextest slow-timeout period 60s terminate-after 3; doctests use GNU timeout 180s'
      set +e
      cargo "${nextest_args[@]}"
      local nextest_rc=$?
      set -e
      rm -f "$cfg"
      [ "$nextest_rc" -eq 0 ] || return "$nextest_rc"
      run_timed_cargo_test_doctests "${cargo_args[@]}"
      return $?
    fi
    rm -f "$cfg"
    echo 'ci-preflight: cargo-nextest cannot express this cargo test command; using GNU timeout per test'
  fi

  if ! command -v timeout >/dev/null 2>&1; then
    echo 'ci-preflight: GNU timeout is not installed; running without a per-test timeout'
    if [ "${#harness[@]}" -gt 0 ]; then
      cargo test "${cargo_args[@]}" -- "${harness[@]}"
    else
      cargo test "${cargo_args[@]}"
    fi
    return $?
  fi

  echo 'ci-preflight: per-test timeout 180s via GNU timeout'
  run_timed_cargo_test_gnu "${cargo_args[@]}" -- "${harness[@]}"
}

run_timed_cargo_test_skip_doctests() {
  local arg
  for arg in "$@"; do
    case "$arg" in
      --lib|--test|--bins|--examples|--tests) return 0 ;;
    esac
  done
  return 1
}

# `if ! cmd; then rc=$?` is 0: the if-test succeeded. Capture the status outside if.
run_with_test_timeout() {
  local name=$1 rc
  shift
  set +e
  timeout --foreground --kill-after=10s 180s "$@"
  rc=$?
  set -e
  if [ "$rc" -ne 0 ]; then
    if [ "$rc" -eq 124 ]; then
      echo "ci-preflight: test timed out after 180s: $name" >&2
    fi
    return "$rc"
  fi
}

# `--list --format terse` prints `name: test` for ignored tests too. The ignore
# bit is not on that line; `--list --ignored` is the list of ignored names.
test_list_name() {
  local line=$1 rest name
  [ -n "$line" ] || return 1
  rest="${line#*: }"
  [ "$rest" != "$line" ] || return 1
  case "$rest" in
    test|test,*) ;;
    *) return 1 ;;
  esac
  name="${line%%:*}"
  [ -n "$name" ] || return 1
  printf '%s\n' "$name"
}

# rustdoc splits every --test-args value on whitespace, so
# `cargo test --doc -- --exact "src/lib.rs - foo (line 2)"` matches nothing
# and exits 0. Return a whitespace-free substring that selects this doctest.
doctest_run_token() {
  local name=$1
  shift
  local candidate best="" best_len=0 count n item
  local -a parts=()
  read -r -a parts <<<"$name"
  for candidate in "${parts[@]}"; do
    case "$candidate" in
      ''|-) continue ;;
      -*) continue ;;
    esac
    count=0
    for n in "$@"; do
      case "$n" in
        *"$candidate"*) count=$((count + 1)) ;;
      esac
    done
    if [ "$count" -eq 1 ] && [ "${#candidate}" -gt "$best_len" ]; then
      best=$candidate
      best_len=${#candidate}
    fi
  done
  if [ -n "$best" ]; then
    printf '%s\n' "$best"
    return 0
  fi
  item="${name#* - }"
  item="${item%% \(line *}"
  case "$item" in
    ''|*' '*|-*) return 1 ;;
  esac
  printf '%s\n' "$item"
}

run_timed_cargo_test_doctests() {
  if run_timed_cargo_test_skip_doctests "$@"; then
    return 0
  fi
  if ! command -v timeout >/dev/null 2>&1; then
    cargo test "$@" --doc
    return $?
  fi
  local list_out list_rc ignored_out ignored_rc name token n rc
  local -a doc_names=()
  local -A ignored_names=() covered=()
  set +e
  list_out="$(cargo test "$@" --doc -- --list --format terse)"
  list_rc=$?
  set -e
  if [ "$list_rc" -ne 0 ]; then
    return "$list_rc"
  fi
  while IFS= read -r line; do
    name="$(test_list_name "$line")" || continue
    doc_names+=("$name")
  done <<<"$list_out"
  set +e
  ignored_out="$(cargo test "$@" --doc -- --list --ignored --format terse)"
  ignored_rc=$?
  set -e
  if [ "$ignored_rc" -ne 0 ]; then
    return "$ignored_rc"
  fi
  while IFS= read -r line; do
    name="$(test_list_name "$line")" || continue
    ignored_names["$name"]=1
  done <<<"$ignored_out"
  for name in "${doc_names[@]}"; do
    [ -n "${covered[$name]+x}" ] && continue
    [ -n "${ignored_names[$name]+x}" ] && continue
    token="$(doctest_run_token "$name" "${doc_names[@]}")" || token=""
    if [ -z "$token" ]; then
      run_with_test_timeout "$name" cargo test "$@" --doc -- --test-threads=1
      return $?
    fi
    run_with_test_timeout "$name" cargo test "$@" --doc -- "$token" --test-threads=1
    rc=$?
    [ "$rc" -eq 0 ] || return "$rc"
    for n in "${doc_names[@]}"; do
      case "$n" in
        *"$token"*) covered["$n"]=1 ;;
      esac
    done
  done
}

run_timed_cargo_test_gnu() {
  local cargo_args=()
  local harness=()
  local past_dd=0 arg
  for arg in "$@"; do
    if [ "$past_dd" -eq 1 ]; then
      harness+=("$arg")
      continue
    fi
    if [ "$arg" = -- ]; then
      past_dd=1
      continue
    fi
    cargo_args+=("$arg")
  done

  local json_out name ignored_only=0 include_ignored=0 rc list_out
  local -a test_exes=()
  for arg in "${harness[@]}"; do
    case "$arg" in
      --ignored) ignored_only=1 ;;
      --include-ignored) include_ignored=1 ;;
    esac
  done

  json_out="$(mktemp)"
  local exe_list
  exe_list="$(mktemp)"
  set +e
  cargo test "${cargo_args[@]}" --no-run --message-format=json >"$json_out"
  rc=$?
  set -e
  if [ "$rc" -ne 0 ]; then
    rm -f "$json_out" "$exe_list"
    return "$rc"
  fi

  set +e
  node --input-type=commonjs - "$json_out" >"$exe_list" <<'JS'
const fs = require("fs");
const seen = new Set();
for (const line of fs.readFileSync(process.argv[2], "utf8").split("\n")) {
  if (!line) continue;
  let j;
  try {
    j = JSON.parse(line);
  } catch {
    continue;
  }
  if (j.reason === "compiler-artifact" && j.executable) {
    seen.add(j.executable);
  }
}
for (const exe of seen) {
  process.stdout.write(`${exe}\n`);
}
JS
  rc=$?
  set -e
  if [ "$rc" -ne 0 ]; then
    rm -f "$json_out" "$exe_list"
    return "$rc"
  fi
  mapfile -t test_exes <"$exe_list"
  rm -f "$exe_list"
  local exe run_args=()
  local -A ignored_names=()
  for exe in "${test_exes[@]}"; do
    [ -n "$exe" ] || continue
    ignored_names=()
    set +e
    list_out="$("$exe" --list --ignored --format terse)"
    rc=$?
    set -e
    if [ "$rc" -ne 0 ]; then
      rm -f "$json_out"
      return "$rc"
    fi
    while IFS= read -r tline; do
      name="$(test_list_name "$tline")" || continue
      ignored_names["$name"]=1
    done <<<"$list_out"
    set +e
    list_out="$("$exe" --list --format terse)"
    rc=$?
    set -e
    if [ "$rc" -ne 0 ]; then
      rm -f "$json_out"
      return "$rc"
    fi
    while IFS= read -r tline; do
      name="$(test_list_name "$tline")" || continue
      if [ "$ignored_only" -eq 1 ]; then
        [ -n "${ignored_names[$name]+x}" ] || continue
        run_args=(--exact "$name" --ignored --test-threads=1)
      elif [ "$include_ignored" -eq 1 ]; then
        run_args=(--exact "$name" --include-ignored --test-threads=1)
      else
        [ -n "${ignored_names[$name]+x}" ] && continue
        run_args=(--exact "$name" --test-threads=1)
      fi
      for arg in "${harness[@]}"; do
        [ "$arg" = --nocapture ] && run_args+=(--nocapture)
      done
      run_with_test_timeout "$name" "$exe" "${run_args[@]}"
      rc=$?
      if [ "$rc" -ne 0 ]; then
        rm -f "$json_out"
        return "$rc"
      fi
    done <<<"$list_out"
  done
  rm -f "$json_out"

  run_timed_cargo_test_doctests "${cargo_args[@]}"
}

# cargo-audit 0.22.2 uses exit 1 for both advisory reports and some operational
# failures (for example advisory DB fetch/load). Classify from JSON stdout and
# stderr instead of trusting exit 1 alone.
run_cargo_audit_gate() {
  local audit_out audit_err audit_rc advisory_count
  audit_out="$(mktemp)"
  audit_err="$(mktemp)"
  set +e
  cargo audit --format json >"$audit_out" 2>"$audit_err"
  audit_rc=$?
  set -e
  if [ -s "$audit_err" ]; then
    cat "$audit_err" >&2
  fi

  if [ "$audit_rc" -eq 0 ]; then
    rm -f "$audit_out" "$audit_err"
    return 0
  fi

  if [ "$audit_rc" -eq 2 ]; then
    echo "ci-preflight: cargo audit failed to run (exit ${audit_rc})" >&2
    rm -f "$audit_out" "$audit_err"
    return 1
  fi

  if [ "$audit_rc" -ne 1 ]; then
    echo "ci-preflight: cargo audit failed to run (exit ${audit_rc})" >&2
    rm -f "$audit_out" "$audit_err"
    return 1
  fi

  if grep -qE '^error:' "$audit_err" || [ ! -s "$audit_out" ]; then
    echo 'ci-preflight: cargo audit failed to run (operational error)' >&2
    rm -f "$audit_out" "$audit_err"
    return 1
  fi

  advisory_count="$(
    AUDIT_OUT="$audit_out" node --input-type=commonjs - <<'JS'
const fs = require("fs");
const report = JSON.parse(fs.readFileSync(process.env.AUDIT_OUT, "utf8"));
const vulnerabilities = report.vulnerabilities || {};
let count = vulnerabilities.count;
if (count == null) {
  count = (vulnerabilities.list || []).length;
}
if (vulnerabilities.found || count) {
  process.stdout.write(String(Number(count)));
} else {
  process.stdout.write("-1");
}
JS
  )" || advisory_count=-1

  if [ "$advisory_count" -lt 0 ]; then
    echo 'ci-preflight: cargo audit failed to run (unexpected exit 1 without advisories)' >&2
    rm -f "$audit_out" "$audit_err"
    return 1
  fi

  if [ "${JMODGUARD_AUDIT_FAIL_CLOSED:-0}" = "1" ]; then
    echo "ci-preflight: cargo audit failed closed (scheduled Buildkite; ${advisory_count} advisories)" >&2
    rm -f "$audit_out" "$audit_err"
    return 1
  fi

  echo "ci-preflight: cargo audit reported ${advisory_count} advisories; pull requests and ordinary main pushes do not fail the supply-chain check" >&2
  rm -f "$audit_out" "$audit_err"
  return 0
}

whitespace_check() {
  local merge_base
  if [ "${BUILDKITE_PULL_REQUEST:-false}" != "false" ]; then
    local pr_base="${BUILDKITE_PULL_REQUEST_BASE_BRANCH:-main}"
    git fetch --no-tags origin "$pr_base"
    echo "ci-preflight: git diff --check origin/${pr_base}...HEAD"
    git diff --check "origin/${pr_base}...HEAD"
    return 0
  fi
  if git rev-parse --verify --quiet "${base_ref}^{commit}" >/dev/null; then
    if ! merge_base="$(git merge-base "$base_ref" HEAD)"; then
      merge_base="$base_ref"
    fi
    # Buildkite main checkouts have origin/main == HEAD, so merge-base vs the
    # working tree is empty. Compare the first-parent patch in that case.
    if [ "$(git rev-parse "$merge_base")" = "$(git rev-parse HEAD)" ]; then
      if git rev-parse --verify --quiet HEAD^ >/dev/null; then
        echo "ci-preflight: git diff --check HEAD^ HEAD (base $base_ref is HEAD)"
        git diff --check HEAD^ HEAD
        return 0
      fi
    fi
    echo "ci-preflight: git diff --check $merge_base"
    git diff --check "$merge_base"
    return 0
  fi
  echo "ci-preflight: git diff --check (working tree; base $base_ref not found)"
  git diff --check
}

run_cargo() {
  if should_run fmt || should_run dylint || should_run api || should_run bot || should_run runtime || should_run updater || should_run core || should_run integration || should_run boundaries || should_run supply-chain; then
    return 0
  fi
  return 1
}

# Rust's default embed-ui and the publication tests consume the same built assets.
if should_run api || should_run integration || should_run dylint || should_run frontend || should_run publish-tests; then
  bash scripts/require-host-eslint-node.sh
  npm ci --ignore-scripts --prefix tools/webui
  npm audit --prefix tools/webui --audit-level=high
  npm run build --prefix tools/webui
fi

if run_cargo; then
  echo 'ci-preflight: cargo update --workspace --locked'
  cargo update --workspace --locked
fi

if should_run fmt; then
  echo 'ci-preflight: cargo fmt --check'
  cargo fmt --check
fi

if should_run dylint; then
  echo 'ci-preflight: bash scripts/dylint-test.sh'
  bash scripts/dylint-test.sh
  echo 'ci-preflight: bash scripts/dylint.sh'
  if [ -z "${only_names}" ]; then
    # Full local gates must not inherit an affected-only Buildkite scope.
    env JMODGUARD_DYLINT_PACKAGES= bash scripts/dylint.sh
  else
    bash scripts/dylint.sh
  fi
fi

if should_run contracts; then
  whitespace_check
  echo 'ci-preflight: bash .buildkite/select-checks-test.sh'
  bash .buildkite/select-checks-test.sh
fi

if should_run publish-tests; then
  echo 'ci-preflight: bash .buildkite/publish-image-test.sh'
  bash .buildkite/publish-image-test.sh
fi

if should_run bootstrap; then
  echo 'ci-preflight: bash scripts/bootstrap-container-test.sh'
  bash scripts/bootstrap-container-test.sh
fi

if [ -z "${only_names}" ]; then
  echo 'ci-preflight: cargo clippy --locked --workspace --all-targets -- -D warnings'
  cargo clippy --locked --workspace --all-targets -- -D warnings

  echo 'ci-preflight: cargo clippy --locked -p jmodguard-api --no-default-features --all-targets -- -D warnings'
  cargo clippy --locked -p jmodguard-api --no-default-features --all-targets -- -D warnings

  echo 'ci-preflight: cargo test --locked --workspace'
  run_timed_cargo_test cargo test --locked --workspace

  echo 'ci-preflight: cargo test --locked -p jmodguard-api --no-default-features --lib'
  run_timed_cargo_test cargo test --locked -p jmodguard-api --no-default-features --lib
else
  if should_run api; then
    echo 'ci-preflight: cargo clippy --locked -p jmodguard-api --all-targets -- -D warnings'
    cargo clippy --locked -p jmodguard-api --all-targets -- -D warnings
    echo 'ci-preflight: cargo clippy --locked -p jmodguard-api --no-default-features --all-targets -- -D warnings'
    cargo clippy --locked -p jmodguard-api --no-default-features --all-targets -- -D warnings
    echo 'ci-preflight: cargo test --locked -p jmodguard-api'
    run_timed_cargo_test cargo test --locked -p jmodguard-api
    echo 'ci-preflight: cargo test --locked -p jmodguard-api --no-default-features --lib'
    run_timed_cargo_test cargo test --locked -p jmodguard-api --no-default-features --lib
    echo 'ci-preflight: cargo build --locked -p jmodguard-api'
    cargo build --locked -p jmodguard-api
    echo 'ci-preflight: cargo build --locked -p jmodguard-api --no-default-features'
    cargo build --locked -p jmodguard-api --no-default-features
  fi
  if should_run bot; then
    echo 'ci-preflight: cargo clippy --locked -p jmodguard-bot --all-targets -- -D warnings'
    cargo clippy --locked -p jmodguard-bot --all-targets -- -D warnings
    echo 'ci-preflight: cargo test --locked -p jmodguard-bot'
    run_timed_cargo_test cargo test --locked -p jmodguard-bot
    echo 'ci-preflight: cargo build --locked -p jmodguard-bot'
    cargo build --locked -p jmodguard-bot
  fi
  if should_run runtime; then
    echo 'ci-preflight: cargo clippy --locked -p jmodguard-runtime --all-targets -- -D warnings'
    cargo clippy --locked -p jmodguard-runtime --all-targets -- -D warnings
    echo 'ci-preflight: cargo test --locked -p jmodguard-runtime'
    run_timed_cargo_test cargo test --locked -p jmodguard-runtime
  fi
  if should_run updater; then
    echo 'ci-preflight: cargo clippy --locked -p jmodguard-updater --all-targets -- -D warnings'
    cargo clippy --locked -p jmodguard-updater --all-targets -- -D warnings
    echo 'ci-preflight: cargo test --locked -p jmodguard-updater'
    run_timed_cargo_test cargo test --locked -p jmodguard-updater
  fi
  if should_run core; then
    echo 'ci-preflight: cargo clippy --locked -p jmodguard --all-targets -- -D warnings'
    cargo clippy --locked -p jmodguard --all-targets -- -D warnings
    echo 'ci-preflight: cargo test --locked -p jmodguard'
    run_timed_cargo_test cargo test --locked -p jmodguard
  fi
  if should_run integration; then
    echo 'ci-preflight: cargo clippy --locked -p jmodguard-integration-tests --all-targets -- -D warnings'
    cargo clippy --locked -p jmodguard-integration-tests --all-targets -- -D warnings
    echo 'ci-preflight: cargo test --locked -p jmodguard-integration-tests'
    run_timed_cargo_test cargo test --locked -p jmodguard-integration-tests
    echo 'ci-preflight: cargo build --locked -p jmodguard-bot'
    cargo build --locked -p jmodguard-bot
    echo 'ci-preflight: cargo build --locked -p jmodguard-api'
    cargo build --locked -p jmodguard-api
    echo 'ci-preflight: cargo test --locked -p jmodguard-integration-tests --test dual_process_smoke -- --include-ignored --nocapture'
    run_timed_cargo_test cargo test --locked -p jmodguard-integration-tests --test dual_process_smoke -- --include-ignored --nocapture
  fi
fi

if should_run boundaries; then
  echo 'ci-preflight: bash scripts/check-service-boundaries.sh'
  bash scripts/check-service-boundaries.sh
fi

if should_run contracts; then
  echo 'ci-preflight: bash scripts/public-install-acceptance-test.sh'
  bash scripts/public-install-acceptance-test.sh

  echo 'ci-preflight: bash scripts/dependabot-config-test.sh'
  bash scripts/dependabot-config-test.sh
fi

if should_run pullfrog; then
  echo 'ci-preflight: node --test scripts/pullfrog-command-check.test.mjs'
  node --test scripts/pullfrog-command-check.test.mjs
fi

if should_run frontend; then
  echo 'ci-preflight: bash scripts/require-host-eslint-node.sh'
  bash scripts/require-host-eslint-node.sh

  echo 'ci-preflight: npm ci --ignore-scripts --prefix tools/eslint'
  npm ci --ignore-scripts --prefix tools/eslint

  echo 'ci-preflight: npm audit --prefix tools/eslint --audit-level=high'
  npm audit --prefix tools/eslint --audit-level=high

  echo 'ci-preflight: bash tools/eslint-web-selftest.sh'
  bash tools/eslint-web-selftest.sh

  echo 'ci-preflight: node --test scripts/frontend-state-test.mjs'
  node --test scripts/frontend-state-test.mjs

  echo 'ci-preflight: node --test scripts/frontend-contrast-test.mjs'
  node --test scripts/frontend-contrast-test.mjs

  echo 'ci-preflight: node --test scripts/frontend-warmup-test.mjs'
  node --test scripts/frontend-warmup-test.mjs

  echo 'ci-preflight: node --test scripts/publish-frontend-test.mjs'
  node --test scripts/publish-frontend-test.mjs

  echo 'ci-preflight: node --test scripts/package-webui-test.mjs'
  node --test scripts/package-webui-test.mjs

  echo 'ci-preflight: node --test scripts/administration-state-test.mjs'
  node --test scripts/administration-state-test.mjs
fi

if [ -z "${only_names}" ]; then
  echo 'ci-preflight: cargo build --locked -p jmodguard-bot'
  cargo build --locked -p jmodguard-bot

  echo 'ci-preflight: cargo build --locked -p jmodguard-api'
  cargo build --locked -p jmodguard-api

  echo 'ci-preflight: cargo test --locked -p jmodguard-integration-tests --test dual_process_smoke -- --include-ignored --nocapture'
  run_timed_cargo_test cargo test --locked -p jmodguard-integration-tests --test dual_process_smoke -- --include-ignored --nocapture

  echo 'ci-preflight: cargo build --locked -p jmodguard-api --no-default-features'
  cargo build --locked -p jmodguard-api --no-default-features
fi

if should_run supply-chain; then
  echo 'ci-preflight: cargo deny check bans licenses sources'
  cargo deny check bans licenses sources

  echo 'ci-preflight: cargo audit'
  run_cargo_audit_gate
fi

echo 'ci-preflight: Buildkite test gates passed'
