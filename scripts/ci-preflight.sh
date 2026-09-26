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

usage() {
  cat <<'EOF'
Usage: bash scripts/ci-preflight.sh [--base <git-revision>] [--only <group>]

Run the Buildkite test gates locally or on a Buildkite linux agent.
With no --only, run the full suite (local hooks and a complete check).
Buildkite uploads one step per selected group. Known groups: contracts,
frontend, fmt, api, bot, conductor, runtime, updater, core, integration,
boundaries, publish-tests, bootstrap, pullfrog, supply-chain.
By default the whitespace diff base is origin/main. Pass --base origin/<branch>
for a PR whose base is not main. Scheduled Buildkite builds set
JMODGUARD_AUDIT_FAIL_CLOSED=1 so cargo-audit fails the job.
EOF
}

base_ref="${JMODGUARD_PREFLIGHT_BASE:-origin/main}"
only_names=""
known_only="contracts frontend fmt api bot conductor runtime updater core integration boundaries publish-tests bootstrap pullfrog supply-chain"
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
  if command -v "$bin" >/dev/null 2>&1; then
    return 0
  fi
  echo "ci-preflight: installing $crate $version"
  cargo install "$crate" --locked --version "$version"
}

if should_run fmt; then
  require_command rustfmt 'rustup component add rustfmt'
fi
if should_run supply-chain; then
  ensure_cargo_tool cargo-deny cargo-deny 0.20.2
  ensure_cargo_tool cargo-audit cargo-audit 0.22.2
fi

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
  if should_run fmt || should_run api || should_run bot || should_run conductor || should_run runtime || should_run updater || should_run core || should_run integration || should_run boundaries || should_run supply-chain; then
    return 0
  fi
  return 1
}

if run_cargo; then
  echo 'ci-preflight: cargo update --workspace --locked'
  cargo update --workspace --locked
fi

if should_run fmt; then
  echo 'ci-preflight: cargo fmt --check'
  cargo fmt --check
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
  echo 'ci-preflight: cargo test --locked --workspace'
  cargo test --locked --workspace

  echo 'ci-preflight: cargo test --locked -p jmodguard-api --no-default-features --lib'
  cargo test --locked -p jmodguard-api --no-default-features --lib

  echo 'ci-preflight: cargo clippy --locked --workspace --all-targets -- -D warnings'
  cargo clippy --locked --workspace --all-targets -- -D warnings

  echo 'ci-preflight: cargo clippy --locked -p jmodguard-api --no-default-features --all-targets -- -D warnings'
  cargo clippy --locked -p jmodguard-api --no-default-features --all-targets -- -D warnings

  echo 'ci-preflight: cargo build --locked -p jmodguard-conductor'
  cargo build --locked -p jmodguard-conductor
else
  if should_run api; then
    echo 'ci-preflight: cargo test --locked -p jmodguard-api'
    cargo test --locked -p jmodguard-api
    echo 'ci-preflight: cargo test --locked -p jmodguard-api --no-default-features --lib'
    cargo test --locked -p jmodguard-api --no-default-features --lib
    echo 'ci-preflight: cargo clippy --locked -p jmodguard-api --all-targets -- -D warnings'
    cargo clippy --locked -p jmodguard-api --all-targets -- -D warnings
    echo 'ci-preflight: cargo clippy --locked -p jmodguard-api --no-default-features --all-targets -- -D warnings'
    cargo clippy --locked -p jmodguard-api --no-default-features --all-targets -- -D warnings
    echo 'ci-preflight: cargo build --locked -p jmodguard-api'
    cargo build --locked -p jmodguard-api
    echo 'ci-preflight: cargo build --locked -p jmodguard-api --no-default-features'
    cargo build --locked -p jmodguard-api --no-default-features
  fi
  if should_run bot; then
    echo 'ci-preflight: cargo test --locked -p jmodguard-bot'
    cargo test --locked -p jmodguard-bot
    echo 'ci-preflight: cargo clippy --locked -p jmodguard-bot --all-targets -- -D warnings'
    cargo clippy --locked -p jmodguard-bot --all-targets -- -D warnings
    echo 'ci-preflight: cargo build --locked -p jmodguard-bot'
    cargo build --locked -p jmodguard-bot
  fi
  if should_run conductor; then
    echo 'ci-preflight: cargo test --locked -p jmodguard-conductor'
    cargo test --locked -p jmodguard-conductor
    echo 'ci-preflight: cargo clippy --locked -p jmodguard-conductor --all-targets -- -D warnings'
    cargo clippy --locked -p jmodguard-conductor --all-targets -- -D warnings
    echo 'ci-preflight: cargo build --locked -p jmodguard-conductor'
    cargo build --locked -p jmodguard-conductor
  fi
  if should_run runtime; then
    echo 'ci-preflight: cargo test --locked -p jmodguard-runtime'
    cargo test --locked -p jmodguard-runtime
    echo 'ci-preflight: cargo clippy --locked -p jmodguard-runtime --all-targets -- -D warnings'
    cargo clippy --locked -p jmodguard-runtime --all-targets -- -D warnings
  fi
  if should_run updater; then
    echo 'ci-preflight: cargo test --locked -p jmodguard-updater'
    cargo test --locked -p jmodguard-updater
    echo 'ci-preflight: cargo clippy --locked -p jmodguard-updater --all-targets -- -D warnings'
    cargo clippy --locked -p jmodguard-updater --all-targets -- -D warnings
  fi
  if should_run core; then
    echo 'ci-preflight: cargo test --locked -p jmodguard'
    cargo test --locked -p jmodguard
    echo 'ci-preflight: cargo clippy --locked -p jmodguard --all-targets -- -D warnings'
    cargo clippy --locked -p jmodguard --all-targets -- -D warnings
  fi
  if should_run integration; then
    echo 'ci-preflight: cargo test --locked -p jmodguard-integration-tests'
    cargo test --locked -p jmodguard-integration-tests
    echo 'ci-preflight: cargo clippy --locked -p jmodguard-integration-tests --all-targets -- -D warnings'
    cargo clippy --locked -p jmodguard-integration-tests --all-targets -- -D warnings
    echo 'ci-preflight: cargo build --locked -p jmodguard-bot'
    cargo build --locked -p jmodguard-bot
    echo 'ci-preflight: cargo build --locked -p jmodguard-api'
    cargo build --locked -p jmodguard-api
    echo 'ci-preflight: cargo test --locked -p jmodguard-integration-tests --test dual_process_smoke -- --include-ignored --nocapture'
    cargo test --locked -p jmodguard-integration-tests --test dual_process_smoke -- --include-ignored --nocapture
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
  cargo test --locked -p jmodguard-integration-tests --test dual_process_smoke -- --include-ignored --nocapture

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
