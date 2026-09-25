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
expect "contracts frontend api supply-chain" frontend/app.js frontend/administration/index.html
expect "contracts frontend" tools/eslint/package.json
expect "contracts frontend" tools/eslint-web-selftest.sh
expect "contracts fmt dylint api boundaries integration supply-chain" crates/api/src/lib.rs
expect "contracts fmt dylint integration supply-chain" crates/integration-tests/tests/dual_process_smoke.rs
expect "contracts fmt dylint remote-database supply-chain" crates/integration-tests/tests/remote_database_smoke.rs
expect "contracts frontend publish-tests supply-chain" Dockerfile.webui
expect "contracts fmt dylint bot boundaries integration supply-chain" crates/bot/src/main.rs
expect "contracts fmt dylint conductor boundaries supply-chain" crates/conductor/src/install.rs
expect "contracts fmt conductor boundaries frontend supply-chain" crates/conductor/frontend/app.js
expect "contracts fmt bot updater bootstrap publish-tests supply-chain" Dockerfile.bot
expect "contracts fmt api updater boundaries integration bootstrap publish-tests supply-chain" Dockerfile.api
expect "contracts fmt dylint runtime boundaries core conductor integration supply-chain" crates/runtime/src/lib.rs
expect "contracts fmt dylint updater api bot boundaries supply-chain" crates/updater/src/lib.rs
expect "contracts dylint supply-chain" dylint.toml scripts/dylint.sh
expect "$rust" src/db/mod.rs
expect "$rust" Cargo.lock
expect "$all" .buildkite/pipeline.yml
expect "$all" mystery.bin
expect "contracts publish-tests supply-chain" .buildkite/publish-image.sh
expect "contracts frontend publish-tests supply-chain" Dockerfile.webui

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
