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

rust="contracts fmt dylint api bot runtime updater core integration boundaries supply-chain remote-database publish-tests bootstrap"
all="contracts frontend fmt dylint api bot runtime updater core integration boundaries publish-tests bootstrap pullfrog supply-chain remote-database"

expect "contracts" docs/INSTALL.md
expect "contracts" README.md
expect "contracts" config.example.toml
expect "contracts frontend api supply-chain" frontend/app.js frontend/administration/index.html
expect "contracts frontend api publish-tests supply-chain" tools/webui/package-lock.json
expect "contracts frontend api publish-tests supply-chain" tools/webui/build.mjs
expect "contracts frontend" tools/eslint/package.json
expect "contracts frontend" tools/eslint-web-selftest.sh
expect "contracts fmt dylint api boundaries integration supply-chain" crates/api/src/lib.rs
expect "contracts fmt dylint integration supply-chain" crates/integration-tests/tests/dual_process_smoke.rs
expect "contracts fmt dylint remote-database supply-chain" crates/integration-tests/tests/remote_database_smoke.rs
expect "contracts fmt dylint remote-database supply-chain" crates/integration-tests/tests/guild_store_smoke.rs
expect "contracts frontend publish-tests supply-chain" Dockerfile.webui
expect "contracts fmt dylint bot boundaries integration supply-chain" crates/bot/src/main.rs
expect "contracts fmt bot updater bootstrap publish-tests supply-chain" Dockerfile.bot
expect "contracts fmt api updater boundaries integration bootstrap publish-tests supply-chain" Dockerfile.api
expect "contracts fmt dylint runtime boundaries core integration supply-chain" crates/runtime/src/lib.rs
expect "contracts fmt dylint updater api bot boundaries supply-chain" crates/updater/src/lib.rs
expect "contracts dylint supply-chain" dylint.toml scripts/dylint.sh scripts/dylint-test.sh
expect "contracts supply-chain" \
  scripts/ci-cargo-log.sh \
  scripts/ci-ensure-cargo-tool.sh \
  scripts/ci-preflight-test.sh \
  scripts/ci-run-timed-cargo-test.sh \
  scripts/ci-timed-cargo-test.sh
# Core consumers need coverage, but independent packages and shell fixtures do not.
core="contracts fmt dylint api bot updater core integration boundaries supply-chain remote-database"
# all_rust groups shared src/migrations skip on purpose. A new rust group must
# join $core or this list; leaving it out of both is a silent shared-source skip.
core_omits="runtime publish-tests bootstrap"
expect "$core" src/config.rs src/db.rs
expect "$core" config.example.toml src/config.rs
expect "$core" migrations/guild/0009_message_snapshots.sql
for group in $rust; do
  case " $core $core_omits " in
    *" $group "*) ;;
    *) fail "rust group $group is neither a core consumer nor an explicit omission" ;;
  esac
done
for group in $core_omits; do
  case " $rust " in
    *" $group "*) ;;
    *) fail "core omission $group is not in the rust plan" ;;
  esac
  case " $core " in
    *" $group "*) fail "core omission $group is selected for shared source" ;;
  esac
done
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
for group in runtime bootstrap publish-tests; do
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
if bash "$selector" plan --files crates/api/src/lib.rs | grep -Eq '^(frontend|bot|remote-database)$'; then
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

fetch_log="$(mktemp)"
fetch_fail_count="$(BUILDKITE_PULL_REQUEST=true BUILDKITE_PULL_REQUEST_BASE_BRANCH=__no_such_branch__ bash "$selector" plan 2>"$fetch_log" | wc -l)"
if [ "$fetch_fail_count" -ne 16 ]; then
  fail "fetch failure did not select every group (got ${fetch_fail_count})"
fi
grep -Fq 'select-checks: all groups: full validation requested or diff unavailable' "$fetch_log" \
  || fail 'missing diff-unavailable reason'

reason_log="$(mktemp)"
bash "$selector" plan --files mystery.bin >/dev/null 2>"$reason_log"
grep -Fq 'select-checks: all groups: unclassified path: mystery.bin' "$reason_log" \
  || fail 'missing unclassified-path reason'
bash "$selector" plan --files .buildkite/pipeline.yml >/dev/null 2>"$reason_log"
grep -Fq 'select-checks: all groups: CI control changed: .buildkite/pipeline.yml' "$reason_log" \
  || fail 'missing CI-control reason'

git init -q --initial-branch=main "$scratch/empty"
(
  cd "$scratch/empty"
  git config user.name 'Selector test'
  git config user.email 'selector@example.invalid'
  git commit -q --allow-empty -m base
  git remote add origin .
  git switch -qc empty-pr
  git commit -q --allow-empty -m empty
  BUILDKITE_PULL_REQUEST=123 BUILDKITE_PULL_REQUEST_BASE_BRANCH=main \
    BUILDKITE_SOURCE=webhook BUILDKITE_TAG= JMODGUARD_PUBLISH_SOURCE= \
    bash "$selector" plan >"$scratch/empty-plan" 2>"$scratch/empty.log"
)
grep -Fq 'select-checks: all groups: empty diff' "$scratch/empty.log" || fail 'missing empty-diff reason'
empty_count="$(wc -l <"$scratch/empty-plan")"
if [ "$empty_count" -ne 16 ]; then
  fail "empty diff did not select every group (got ${empty_count})"
fi

# Scope the expensive lint compilation to selected Rust targets/consumers.
expect_lint_packages() {
  local expected="$1" actual
  shift
  actual="$(BUILDKITE=false bash "$selector" upload --files "$@" | sed -n 's/^      JMODGUARD_DYLINT_PACKAGES: "\(.*\)"$/\1/p')"
  [[ "$actual" == "$expected" ]] || fail "lint scope for $*: expected [$expected], got [$actual]"
}
expect_lint_packages 'jmodguard-api jmodguard-integration-tests' crates/api/src/lib.rs
expect_lint_packages 'jmodguard-api jmodguard-bot jmodguard-integration-tests' crates/bot/src/main.rs
expect_lint_packages 'jmodguard-api jmodguard-bot jmodguard-runtime jmodguard-updater jmodguard jmodguard-integration-tests' crates/runtime/src/lib.rs
expect_lint_packages 'jmodguard-api jmodguard-bot jmodguard-updater jmodguard jmodguard-integration-tests' src/config.rs
expect_lint_packages 'jmodguard jmodguard-integration-tests' crates/integration-tests/tests/remote_database_smoke.rs
expect_lint_packages '' Cargo.lock
expect_lint_packages '' dylint.toml crates/api/src/lib.rs
expect_lint_packages '' mystery.bin
echo 'select-checks-test: ok'
