#!/usr/bin/env bash
# Shared selected GitHub Actions check groups and the full local suite.
# The remote-database job (libSQL containers) stays separate.
# Dependabot Updates dynamic workflows are not CI.
set -euo pipefail

# UTC calendar dates are YYYY-MM-DD. cargo-audit has no expiry field.
audit_ignore_is_expired() {
  local today=$1 expiry=$2
  (
    LC_ALL=C
    [[ "$today" == "$expiry" || "$today" > "$expiry" ]]
  )
}

if [ "${1:-}" = "--audit-expiry-self-check" ]; then
  fail() { echo "ci-preflight: audit expiry self-check: $*" >&2; exit 1; }
  audit_ignore_is_expired 2027-01-14 2027-01-15 && fail "2027-01-14 must stay valid"
  audit_ignore_is_expired 2027-01-15 2027-01-15 || fail "2027-01-15 must expire"
  audit_ignore_is_expired 2027-01-16 2027-01-15 || fail "2027-01-16 must expire"
  echo "ci-preflight: audit expiry self-check passed"
  exit 0
fi

root="$(git rev-parse --show-toplevel)"
cd "$root"

if [ -z "${JMODGUARD_CI_PREFIX:-}" ]; then
  if [ -n "${HOME:-}" ]; then
    JMODGUARD_CI_PREFIX="$HOME/.cache/jmodguard-ci"
  else
    JMODGUARD_CI_PREFIX=/opt/jmodguard-ci
  fi
fi
# shellcheck source=scripts/ci-ensure-cargo-tool.sh
source "$root/scripts/ci-ensure-cargo-tool.sh"
# shellcheck source=scripts/ci-run-timed-cargo-test.sh
source "$root/scripts/ci-run-timed-cargo-test.sh"

usage() {
  cat <<'EOF'
Usage: bash scripts/ci-preflight.sh [--base <git-revision>] [--only <group>]

Run the CI gates locally or on an ephemeral GitHub-hosted Linux runner.
With no --only, run the full suite (local hooks and a complete check).
GitHub Actions groups affected checks into jobs. Known groups: contracts,
frontend, fmt, dylint, api, bot, runtime, updater, core, integration,
boundaries, publish-tests, bootstrap, pullfrog, supply-chain.
By default the whitespace diff base is origin/main. Pass --base origin/<branch>
for a PR whose base is not main. Scheduled builds set
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
  node scripts/ci-workflow-test.mjs
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

if should_run fmt; then
  require_command rustfmt 'rustup component add rustfmt'
fi

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

# cargo-audit 0.22.2 uses exit 1 for both advisory reports and some operational
# failures (for example advisory DB fetch/load). Classify from JSON stdout and
# stderr instead of trusting exit 1 alone.
run_cargo_audit_gate() {
  local audit_out audit_err audit_rc advisory_count node_rc ignore_0258
  audit_out="$(mktemp)"
  audit_err="$(mktemp)"
  set +e
  cargo audit --format json >"$audit_out" 2>"$audit_err"
  audit_rc=$?
  set -e
  if [ -s "$audit_err" ]; then
    cat "$audit_err" >&2
  fi

  if [ "$audit_rc" -eq 2 ]; then
    echo "ci-preflight: cargo audit failed to run (exit ${audit_rc})" >&2
    rm -f "$audit_out" "$audit_err"
    return 1
  fi

  if [ "$audit_rc" -ne 0 ] && [ "$audit_rc" -ne 1 ]; then
    echo "ci-preflight: cargo audit failed to run (exit ${audit_rc})" >&2
    rm -f "$audit_out" "$audit_err"
    return 1
  fi

  if [ "$audit_rc" -eq 1 ]; then
    if grep -qE '^error:' "$audit_err" || [ ! -s "$audit_out" ]; then
      echo 'ci-preflight: cargo audit failed to run (operational error)' >&2
      rm -f "$audit_out" "$audit_err"
      return 1
    fi
  fi

  set +e
  advisory_count="$(
    AUDIT_OUT="$audit_out" node --input-type=commonjs - <<'JS'
const fs = require("fs");
let report;
try {
  report = JSON.parse(fs.readFileSync(process.env.AUDIT_OUT, "utf8"));
} catch {
  process.exit(1);
}

const vulnerabilities = report.vulnerabilities || {};
for (const entry of vulnerabilities.list || []) {
  const id = entry.advisory?.id;
  const pkg = entry.package;
  if (!id || !pkg?.name || !pkg?.version) {
    process.exit(1);
  }
  console.error(`ci-preflight: advisory ${id} ${pkg.name} ${pkg.version}`);
}

for (const id of report.settings?.ignore || []) {
  if (id === "RUSTSEC-2026-0258") {
    console.error(
      "ci-preflight: advisory RUSTSEC-2026-0258 ignored (expires 2027-01-15; see #349)"
    );
  } else {
    console.error(`ci-preflight: advisory ${id} ignored`);
  }
}

const warnings = report.warnings || {};
for (const kind of ["yanked", "unmaintained"]) {
  for (const entry of warnings[kind] || []) {
    const pkg = entry.package;
    if (!pkg?.name || !pkg?.version) {
      process.exit(1);
    }
    const adv = entry.advisory?.id;
    if (adv) {
      console.error(
        `ci-preflight: warning ${kind} ${pkg.name} ${pkg.version} ${adv}`
      );
    } else {
      console.error(
        `ci-preflight: warning ${kind} ${pkg.name} ${pkg.version}`
      );
    }
  }
}

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
  )"
  node_rc=$?
  set -e

  if [ "$node_rc" -ne 0 ] && [ "$audit_rc" -eq 0 ]; then
    echo 'ci-preflight: cargo audit failed to run (operational error)' >&2
    rm -f "$audit_out" "$audit_err"
    return 1
  fi

  if [ "$node_rc" -ne 0 ]; then
    advisory_count=-1
  fi

  ignore_0258=0
  if AUDIT_OUT="$audit_out" node --input-type=commonjs -e '
const report = JSON.parse(require("fs").readFileSync(process.env.AUDIT_OUT, "utf8"));
process.exit((report.settings?.ignore || []).includes("RUSTSEC-2026-0258") ? 0 : 1);
' 2>/dev/null; then
    ignore_0258=1
  fi

  if [ "$ignore_0258" -eq 1 ] && audit_ignore_is_expired "$(date -u +%Y-%m-%d)" 2027-01-15; then
    echo 'ci-preflight: cargo audit exception RUSTSEC-2026-0258 expired on 2027-01-15; see #349; re-review the exception' >&2
    rm -f "$audit_out" "$audit_err"
    return 1
  fi

  if [ "$advisory_count" -lt 0 ]; then
    if [ "$audit_rc" -eq 0 ]; then
      rm -f "$audit_out" "$audit_err"
      return 0
    fi
    echo 'ci-preflight: cargo audit failed to run (unexpected exit 1 without advisories)' >&2
    rm -f "$audit_out" "$audit_err"
    return 1
  fi

  if [ "${JMODGUARD_AUDIT_FAIL_CLOSED:-0}" = "1" ]; then
    echo "ci-preflight: cargo audit failed closed (scheduled CI; ${advisory_count} advisories)" >&2
    rm -f "$audit_out" "$audit_err"
    return 1
  fi

  echo "ci-preflight: cargo audit reported ${advisory_count} advisories; pull requests and ordinary main pushes do not fail the supply-chain check" >&2
  rm -f "$audit_out" "$audit_err"
  return 0
}

whitespace_check() {
  local merge_base
  if [[ "${JMODGUARD_DIFF_BASE:-}" =~ ^[0-9a-f]{40}$ && "${JMODGUARD_DIFF_HEAD:-}" =~ ^[0-9a-f]{40}$ ]] \
      && git cat-file -e "${JMODGUARD_DIFF_BASE}^{commit}" 2>/dev/null \
      && git cat-file -e "${JMODGUARD_DIFF_HEAD}^{commit}" 2>/dev/null; then
    local range_base="$JMODGUARD_DIFF_BASE"
    if [ "${GITHUB_EVENT_NAME:-}" = pull_request ]; then
      range_base="$(git merge-base "$range_base" "$JMODGUARD_DIFF_HEAD")"
    fi
    git diff --check "$range_base" "$JMODGUARD_DIFF_HEAD"
    return 0
  fi
  if git rev-parse --verify --quiet "${base_ref}^{commit}" >/dev/null; then
    if ! merge_base="$(git merge-base "$base_ref" HEAD)"; then
      merge_base="$base_ref"
    fi
    # Main checkouts have origin/main == HEAD, so merge-base vs the
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
    # Full local gates must not inherit an affected-only CI scope.
    env JMODGUARD_DYLINT_PACKAGES= bash scripts/dylint.sh
  else
    bash scripts/dylint.sh
  fi
fi

if should_run contracts; then
  whitespace_check
  echo 'ci-preflight: bash scripts/select-checks-test.sh'
  bash scripts/select-checks-test.sh

  echo 'ci-preflight: bash scripts/ci-timed-cargo-test.sh'
  bash scripts/ci-timed-cargo-test.sh
fi

if should_run publish-tests; then
  echo 'ci-preflight: bash scripts/publish-image-test.sh'
  bash scripts/publish-image-test.sh
  bash scripts/prepare-publication-test.sh
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
  run_timed_cargo_test cargo test --locked --workspace || exit $?

  echo 'ci-preflight: cargo test --locked -p jmodguard-api --no-default-features --lib'
  run_timed_cargo_test cargo test --locked -p jmodguard-api --no-default-features --lib || exit $?
else
  if should_run api; then
    echo 'ci-preflight: cargo clippy --locked -p jmodguard-api --all-targets -- -D warnings'
    cargo clippy --locked -p jmodguard-api --all-targets -- -D warnings
    echo 'ci-preflight: cargo clippy --locked -p jmodguard-api --no-default-features --all-targets -- -D warnings'
    cargo clippy --locked -p jmodguard-api --no-default-features --all-targets -- -D warnings
    echo 'ci-preflight: cargo test --locked -p jmodguard-api'
    run_timed_cargo_test cargo test --locked -p jmodguard-api || exit $?
    echo 'ci-preflight: cargo test --locked -p jmodguard-api --no-default-features --lib'
    run_timed_cargo_test cargo test --locked -p jmodguard-api --no-default-features --lib || exit $?
    echo 'ci-preflight: cargo build --locked -p jmodguard-api'
    cargo build --locked -p jmodguard-api
    echo 'ci-preflight: cargo build --locked -p jmodguard-api --no-default-features'
    cargo build --locked -p jmodguard-api --no-default-features
  fi
  if should_run bot; then
    echo 'ci-preflight: cargo clippy --locked -p jmodguard-bot --all-targets -- -D warnings'
    cargo clippy --locked -p jmodguard-bot --all-targets -- -D warnings
    echo 'ci-preflight: cargo test --locked -p jmodguard-bot'
    run_timed_cargo_test cargo test --locked -p jmodguard-bot || exit $?
    echo 'ci-preflight: cargo build --locked -p jmodguard-bot'
    cargo build --locked -p jmodguard-bot
  fi
  if should_run runtime; then
    echo 'ci-preflight: cargo clippy --locked -p jmodguard-runtime --all-targets -- -D warnings'
    cargo clippy --locked -p jmodguard-runtime --all-targets -- -D warnings
    echo 'ci-preflight: cargo test --locked -p jmodguard-runtime'
    run_timed_cargo_test cargo test --locked -p jmodguard-runtime || exit $?
  fi
  if should_run updater; then
    echo 'ci-preflight: cargo clippy --locked -p jmodguard-updater --all-targets -- -D warnings'
    cargo clippy --locked -p jmodguard-updater --all-targets -- -D warnings
    echo 'ci-preflight: cargo test --locked -p jmodguard-updater'
    run_timed_cargo_test cargo test --locked -p jmodguard-updater || exit $?
  fi
  if should_run core; then
    echo 'ci-preflight: cargo clippy --locked -p jmodguard --all-targets -- -D warnings'
    cargo clippy --locked -p jmodguard --all-targets -- -D warnings
    echo 'ci-preflight: cargo test --locked -p jmodguard'
    run_timed_cargo_test cargo test --locked -p jmodguard || exit $?
  fi
  if should_run integration; then
    echo 'ci-preflight: cargo clippy --locked -p jmodguard-integration-tests --all-targets -- -D warnings'
    cargo clippy --locked -p jmodguard-integration-tests --all-targets -- -D warnings
    echo 'ci-preflight: cargo test --locked -p jmodguard-integration-tests'
    run_timed_cargo_test cargo test --locked -p jmodguard-integration-tests || exit $?
    echo 'ci-preflight: cargo build --locked -p jmodguard-bot'
    cargo build --locked -p jmodguard-bot
    echo 'ci-preflight: cargo build --locked -p jmodguard-api'
    cargo build --locked -p jmodguard-api
    echo 'ci-preflight: cargo test --locked -p jmodguard-integration-tests --test dual_process_smoke -- --include-ignored --nocapture'
    run_timed_cargo_test cargo test --locked -p jmodguard-integration-tests --test dual_process_smoke -- --include-ignored --nocapture || exit $?
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
  run_timed_cargo_test cargo test --locked -p jmodguard-integration-tests --test dual_process_smoke -- --include-ignored --nocapture || exit $?

  echo 'ci-preflight: cargo build --locked -p jmodguard-api --no-default-features'
  cargo build --locked -p jmodguard-api --no-default-features
fi

if should_run supply-chain; then
  echo 'ci-preflight: bash scripts/ci-preflight-test.sh'
  bash scripts/ci-preflight-test.sh

  ensure_cargo_tool cargo-deny cargo-deny 0.20.2
  ensure_cargo_tool cargo-audit cargo-audit 0.22.2

  echo 'ci-preflight: cargo deny check bans licenses sources'
  cargo deny check bans licenses sources

  echo 'ci-preflight: cargo audit'
  run_cargo_audit_gate
fi

echo 'ci-preflight: CI test gates passed'
