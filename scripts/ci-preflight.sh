#!/usr/bin/env bash
# Single source of truth for the Buildkite `test` job.
# The remote-database job (libSQL containers) stays separate.
# GitHub Actions workflows and GitHub-hosted runners are forbidden.
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
cd "$root"

usage() {
  cat <<'EOF'
Usage: bash scripts/ci-preflight.sh [--base <git-revision>]

Run the Buildkite `test` job gates locally or on a Buildkite linux agent.
By default the whitespace diff base is origin/main. Pass --base origin/<branch>
for a PR whose base is not main. Scheduled Buildkite builds set
JMODGUARD_AUDIT_FAIL_CLOSED=1 so cargo-audit fails the job.
EOF
}

base_ref="${JMODGUARD_PREFLIGHT_BASE:-origin/main}"
while [ "$#" -gt 0 ]; do
  case "$1" in
    --base)
      [ "$#" -ge 2 ] || { usage >&2; exit 2; }
      base_ref="$2"
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

if [ -d "$root/.github/workflows" ]; then
  echo 'ci-preflight: GitHub Actions workflows are forbidden; CI runs on Buildkite' >&2
  exit 1
fi

if ! grep -Fq 'bash scripts/ci-preflight.sh' "$root/.buildkite/pipeline.yml"; then
  echo 'ci-preflight: .buildkite/pipeline.yml must invoke bash scripts/ci-preflight.sh' >&2
  exit 1
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

require_command rustfmt 'rustup component add rustfmt'
ensure_cargo_tool cargo-deny cargo-deny 0.20.2
ensure_cargo_tool cargo-audit cargo-audit 0.22.2

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

echo 'ci-preflight: cargo update --workspace --locked'
cargo update --workspace --locked

echo 'ci-preflight: cargo fmt --check'
cargo fmt --check

whitespace_check

echo 'ci-preflight: python3 .buildkite/publish-image-test.py'
python3 .buildkite/publish-image-test.py

echo 'ci-preflight: python3 .buildkite/release-test.py'
python3 .buildkite/release-test.py

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

echo 'ci-preflight: python3 scripts/check-service-boundaries.py'
python3 scripts/check-service-boundaries.py

echo 'ci-preflight: python3 scripts/public-install-acceptance-test.py'
python3 scripts/public-install-acceptance-test.py

echo 'ci-preflight: node --test scripts/frontend-state-test.mjs'
node --test scripts/frontend-state-test.mjs

echo 'ci-preflight: node --test scripts/frontend-warmup-test.mjs'
node --test scripts/frontend-warmup-test.mjs

echo 'ci-preflight: node --test scripts/publish-frontend-test.mjs'
node --test scripts/publish-frontend-test.mjs

echo 'ci-preflight: python3 scripts/frontend-release-test.py'
python3 scripts/frontend-release-test.py

echo 'ci-preflight: cargo test --locked -p jmodguard-integration-tests --test dual_process_smoke -- --nocapture'
cargo test --locked -p jmodguard-integration-tests --test dual_process_smoke -- --nocapture

echo 'ci-preflight: cargo build --locked -p jmodguard-bot'
cargo build --locked -p jmodguard-bot

echo 'ci-preflight: cargo build --locked -p jmodguard-api'
cargo build --locked -p jmodguard-api

echo 'ci-preflight: cargo build --locked -p jmodguard-api --no-default-features'
cargo build --locked -p jmodguard-api --no-default-features

echo 'ci-preflight: cargo deny check bans licenses sources'
cargo deny check bans licenses sources

echo 'ci-preflight: cargo audit'
set +e
cargo audit
audit_rc=$?
set -e
# cargo-audit 0.22.2: 0 clean, 1 vulnerability report (and some advisory-DB
# fetch failures), 2 lockfile/report errors. Only 1 is the CVE fail-open path.
if [ "$audit_rc" -eq 0 ]; then
  :
elif [ "$audit_rc" -eq 1 ] && [ "${JMODGUARD_AUDIT_FAIL_CLOSED:-0}" != "1" ]; then
  echo 'ci-preflight: cargo audit reported advisories; pull requests and ordinary main pushes do not fail the test job' >&2
elif [ "${JMODGUARD_AUDIT_FAIL_CLOSED:-0}" = "1" ]; then
  echo 'ci-preflight: cargo audit failed closed (scheduled Buildkite)' >&2
  exit 1
else
  echo "ci-preflight: cargo audit failed to run (exit ${audit_rc})" >&2
  exit 1
fi

echo 'ci-preflight: Buildkite test gates passed'
