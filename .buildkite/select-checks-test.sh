#!/usr/bin/env bash
# Selector expectations are the path groups, not the script's internal order.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
selector="$root/.buildkite/select-checks.sh"

fail() {
  echo "select-checks-test: $*" >&2
  exit 1
}

expect() {
  local name="$1"
  shift
  local actual
  actual="$(bash "$selector" plan --files "$@" | sort)"
  local expected
  expected="$(printf '%s\n' "$@" >/dev/null; printf '%s\n' "$name" | tr ' ' '\n' | sort)"
  # $name is the expected groups as a single string. Re-parse below.
  expected="$(printf '%s\n' $name | sort)"
  if [ "$actual" != "$expected" ]; then
    echo "select-checks-test: ${1:-files} expected:" >&2
    printf '%s\n' "$expected" >&2
    echo "actual:" >&2
    printf '%s\n' "$actual" >&2
    exit 1
  fi
}

rust="contracts fmt dylint api bot conductor runtime updater core integration boundaries supply-chain remote-database publish-tests bootstrap"
all="contracts frontend fmt dylint api bot conductor runtime updater core integration boundaries publish-tests bootstrap pullfrog supply-chain remote-database"

expect "contracts" docs/INSTALL.md
expect "contracts" README.md
expect "contracts" config.example.toml
expect "contracts frontend api supply-chain" frontend/app.js frontend/administration/index.html
expect "contracts frontend" tools/eslint/package.json
expect "contracts frontend" tools/eslint-web-selftest.sh
expect "contracts fmt dylint api boundaries integration supply-chain" crates/api/src/lib.rs
expect "contracts fmt dylint integration supply-chain" crates/integration-tests/tests/dual_process_smoke.rs
expect "contracts fmt dylint remote-database supply-chain" crates/integration-tests/tests/remote_database_smoke.rs
expect "contracts fmt dylint remote-database supply-chain" crates/integration-tests/tests/guild_store_smoke.rs
expect "contracts frontend publish-tests supply-chain" Dockerfile.webui
expect "contracts fmt dylint bot boundaries integration supply-chain" crates/bot/src/main.rs
expect "contracts fmt dylint conductor boundaries supply-chain" crates/conductor/src/install.rs
expect "contracts fmt conductor boundaries frontend supply-chain" crates/conductor/frontend/app.js
expect "contracts fmt bot updater bootstrap publish-tests supply-chain" Dockerfile.bot
expect "contracts fmt api updater boundaries integration bootstrap publish-tests supply-chain" Dockerfile.api
expect "contracts fmt dylint runtime boundaries core conductor integration supply-chain" crates/runtime/src/lib.rs
expect "contracts fmt dylint updater api bot boundaries supply-chain" crates/updater/src/lib.rs
expect "contracts dylint supply-chain" dylint.toml scripts/dylint.sh scripts/dylint-test.sh
# Core consumers need coverage, but independent packages and shell fixtures do not.
core="contracts fmt dylint api bot updater core integration boundaries supply-chain remote-database"
expect "$core" src/config.rs src/db.rs
expect "$core" config.example.toml src/config.rs
# Conductor tests embed migration SQL even though conductor cannot import core.
expect "$core conductor" migrations/guild/0009_message_snapshots.sql
expect "$core conductor" src/config.rs crates/conductor/src/install.rs
expect "$rust" vendor/tokio-tungstenite/src/lib.rs
expect "$rust" Cargo.lock
expect "$all" .buildkite/pipeline.yml
expect "$all" mystery.bin
expect "contracts publish-tests supply-chain" .buildkite/publish-image.sh
expect "contracts frontend publish-tests supply-chain" Dockerfile.webui

# Exercise diff discovery and the generated pipeline with real local Git history.
# A frontend commit followed by a core edit on one PR must select the union.
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
git init -q --initial-branch=main "$scratch/repo"
(
  cd "$scratch/repo"
  git config user.name 'Selector test'
  git config user.email 'selector@example.invalid'
  mkdir -p .buildkite src frontend
  cp "$root/.buildkite/pipeline.steps.yml" .buildkite/
  echo base >src/config.rs
  git add .
  git commit -qm base
  git remote add origin .
  git switch -qc test-pr
  echo frontend >frontend/app.js
  git add .
  git commit -qm frontend
  echo change >>src/config.rs
  git commit -qam core
  BUILDKITE=false BUILDKITE_PULL_REQUEST=123 BUILDKITE_PULL_REQUEST_BASE_BRANCH=main \
    BUILDKITE_SOURCE=webhook BUILDKITE_TAG= JMODGUARD_PUBLISH_SOURCE= \
    bash "$selector" upload >"$scratch/pipeline.yml" 2>"$scratch/selection.log"
)
# Extract only enabled check keys, not the unchanged publication expressions.
actual="$(awk '/^    key:/ { key = $2 } /^    if: "true"$/ { print key }' "$scratch/pipeline.yml" | sort)"
expected="$(printf '%s\n' $core frontend | sort)"
[ "$actual" = "$expected" ] || fail "PR upload selected unexpected check steps: $actual"
grep -Fq 'PR diff origin/main...HEAD' "$scratch/selection.log" || fail 'missing diff diagnostic'
grep -Fq 'select-checks: selected:' "$scratch/selection.log" || fail 'missing selected-group diagnostic'
# Skipped keys must remain available to unchanged publication dependencies.
for group in runtime conductor bootstrap publish-tests; do
  awk -v wanted="$group" '
    /^    key:/ { key = $2 }
    /^    if: "false"$/ && key == wanted { found = 1 }
    END { exit !found }
  ' "$scratch/pipeline.yml" || fail "$group not uploaded as skipped"
done
sed -n '/  # group: publish$/,$p' "$root/.buildkite/pipeline.steps.yml" | sed '/^  # group: /d' >"$scratch/publish.expected"
sed -n '/  - label: "prepare image publication"/,$p' "$scratch/pipeline.yml" >"$scratch/publish.actual"
cmp "$scratch/publish.expected" "$scratch/publish.actual" || fail 'publication steps changed'

schedule="$(BUILDKITE_SOURCE=schedule bash "$selector" plan)"
if [ "$(printf '%s\n' "$schedule" | sort)" != "$(printf '%s\n' $all | sort)" ]; then
  fail "schedule did not select every group"
fi

if ! bash "$selector" plan --files docs/INSTALL.md | grep -qx contracts; then
  fail "docs plan was not only contracts"
fi
if bash "$selector" plan --files docs/INSTALL.md | grep -qx remote-database; then
  fail "docs plan included remote-database"
fi
if bash "$selector" plan --files frontend/app.js | grep -Eq '^(bot|remote-database)$'; then
  fail "frontend plan included an unrelated rust group"
fi
if ! bash "$selector" plan --files frontend/app.js | grep -qx api; then
  fail "root frontend plan omitted api (embed-ui include_str)"
fi
if bash "$selector" plan --files crates/api/src/lib.rs | grep -Eq '^(frontend|bot|conductor|remote-database)$'; then
  fail "api plan included an unrelated group"
fi

if ! grep -Fq 'bash .buildkite/select-checks-test.sh' "$root/scripts/ci-preflight.sh"; then
  fail "ci-preflight does not run select-checks-test.sh"
fi

if ! grep -Fq 'BUILDKITE_STEP_KEY' "$root/.buildkite/with-cargo-auth.sh"; then
  fail "with-cargo-auth must isolate CARGO_TARGET_DIR per Buildkite step"
fi
if ! grep -Fq "'\$GITHUB_TOKEN'" "$root/.buildkite/with-cargo-auth.sh"; then
  fail "with-cargo-auth must treat the literal \$GITHUB_TOKEN placeholder as unset"
fi
if grep -Fq "'\$\$GITHUB_TOKEN'" "$root/.buildkite/with-cargo-auth.sh"; then
  fail "with-cargo-auth must not match only the YAML-escaped placeholder"
fi

fetch_fail_count="$(BUILDKITE_PULL_REQUEST=true BUILDKITE_PULL_REQUEST_BASE_BRANCH=__no_such_branch__ bash "$selector" plan | wc -l)"
if [ "$fetch_fail_count" -ne 17 ]; then
  fail "fetch failure did not select every group (got ${fetch_fail_count})"
fi

echo 'select-checks-test: ok'
