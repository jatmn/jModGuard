#!/usr/bin/env bash
# Selector expectations are the path groups, not the script's internal order.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
selector="$root/scripts/select-checks.sh"

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

expect_each() {
  local groups="$1" path
  shift
  for path in "$@"; do
    expect "$groups" "$path"
  done
}

rust="contracts fmt dylint api bot runtime updater core integration boundaries supply-chain remote-database publish-tests bootstrap"
all="contracts frontend fmt dylint api bot runtime updater core integration boundaries publish-tests bootstrap pullfrog runner-tests supply-chain remote-database"

expect "contracts" docs/INSTALL.md
expect "contracts" README.md
expect "contracts" config.example.toml
expect_each "contracts runner-tests" AGENTS.md docs/CI.md
# Browser files reach Rust only through the default embed-ui feature, so they
# skip the API built without it. A real API change still runs both halves once.
expect "contracts frontend api-embed supply-chain" frontend/app.js frontend/administration/index.html
expect "contracts frontend api-embed publish-tests supply-chain" tools/webui/package-lock.json
expect "contracts frontend api-embed publish-tests supply-chain" tools/webui/build.mjs
expect "contracts frontend fmt dylint api boundaries integration supply-chain" frontend/app.js crates/api/src/lib.rs
expect_each "contracts frontend" \
  scripts/frontend-state-test.mjs scripts/frontend-test-assert.mjs scripts/frontend-contrast-test.mjs \
  scripts/frontend-warmup-test.mjs scripts/frontend-sr-only-test.mjs scripts/package-webui.mjs \
  scripts/package-webui-test.mjs scripts/administration-state-test.mjs scripts/require-host-eslint-node.sh \
  tools/eslint/package.json tools/eslint-web-selftest.sh
expect "contracts fmt dylint api boundaries integration supply-chain" crates/api/src/lib.rs
expect "contracts fmt dylint api boundaries integration supply-chain" crates/api/Cargo.toml
expect "contracts frontend fmt dylint api boundaries integration supply-chain" crates/api/src/web/template_preview_cases.json
expect "contracts fmt dylint integration supply-chain" crates/integration-tests/tests/dual_process_smoke.rs
expect "contracts fmt dylint integration remote-database supply-chain" crates/integration-tests/Cargo.toml
expect_each "contracts fmt dylint remote-database supply-chain" \
  crates/integration-tests/tests/remote_database_smoke.rs crates/integration-tests/tests/guild_store_smoke.rs
expect "contracts frontend publish-tests supply-chain" Dockerfile.webui
expect_each "contracts fmt dylint bot api updater boundaries integration supply-chain" crates/bot/src/main.rs crates/bot/Cargo.toml
expect "contracts fmt bot updater core bootstrap publish-tests supply-chain" Dockerfile.bot
expect "contracts fmt api updater core boundaries integration bootstrap publish-tests supply-chain" Dockerfile.api
expect_each "contracts fmt dylint runtime boundaries core integration supply-chain" crates/runtime/src/lib.rs crates/runtime/Cargo.toml
expect_each "contracts fmt dylint updater api bot boundaries supply-chain" crates/updater/src/lib.rs crates/updater/Cargo.toml
expect_each "contracts dylint supply-chain" \
  dylint.toml tools/dylint/test-flake/src/lib.rs scripts/dylint.sh scripts/dylint-test.sh scripts/ci-dylint-libs.sh
expect_each "contracts frontend dylint supply-chain" scripts/check-test-flakes.mjs scripts/check-test-flakes.test.mjs tools/test-flake-allowlist.tsv
expect_each "contracts bootstrap supply-chain" scripts/bootstrap-container.sh scripts/bootstrap-container-test.sh
expect "contracts remote-database supply-chain" scripts/ci-remote-database.sh
expect "contracts boundaries supply-chain" scripts/check-service-boundaries.sh
expect_each "contracts pullfrog" \
  scripts/pullfrog-command-check.mjs scripts/pullfrog-command-check.test.mjs .github/workflows/pullfrog.yml .github/dependabot.yml
expect_each "contracts" scripts/public-install-acceptance-test.sh scripts/dependabot-config-test.sh .githooks/pre-push scripts/install-git-hooks.sh
# The runner regression suites follow their own inputs, not application code.
expect "contracts runner-tests supply-chain" scripts/ci-ensure-cargo-tool.sh
expect "contracts frontend runner-tests" scripts/ci-frontend-node-timeout.sh
expect_each "contracts runner-tests" \
  scripts/ci-cargo-log.sh \
  scripts/ci-preflight-test.sh \
  scripts/ci-preflight-node-test-files.txt \
  scripts/ci-run-timed-cargo-test.sh \
  scripts/ci-timed-cargo-test.sh
expect "$rust runner-tests" rust-toolchain.toml
# Core consumers need coverage, but independent packages and shell fixtures do not.
core="contracts fmt dylint api bot updater core integration boundaries supply-chain remote-database"
# all_rust groups shared src/migrations skip on purpose. A new rust group must
# join $core or this list; leaving it out of both is a silent shared-source skip.
core_omits="runtime publish-tests bootstrap"
expect "$core" src/config.rs src/db.rs
expect "$core" config.example.toml src/config.rs
expect "$core" migrations/guild/0016_baseline.sql
expect "$core frontend" src/log_embeds.rs
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
expect_each "$rust" Cargo.lock deny.toml Cargo.toml
expect_each "$all" .github/workflows/ci.yml .github/workflows/publish.yml \
  scripts/select-checks.sh scripts/select-checks-test.sh scripts/ci-workflow-test.mjs scripts/ci-preflight.sh scripts/ci-setup.sh
expect_each "contracts frontend publish-tests supply-chain" LICENSE LICENSE.md
expect "$all" mystery.bin
# After --files --, a path named like an option is still a path.
expect "$all" -- --odd.bin
expect "contracts" -- docs/INSTALL.md
expect "contracts frontend api-embed supply-chain" -- frontend/app.js --odd.md
expect "contracts updater publish-tests supply-chain" scripts/publish-image.sh
expect_each "contracts publish-tests supply-chain" \
  scripts/publish-image-test.sh scripts/prepare-publication.sh scripts/prepare-publication-test.sh

expect_scan() {
  local expected="$1"
  shift
  [[ "$(bash "$selector" socket --files "$@")" == "$expected" ]] || fail "dependency scan selection for $*"
}
for path in docs/INSTALL.md README.md frontend/app.js crates/api/src/lib.rs crates/bot/src/main.rs src/db.rs tools/webui/build.mjs; do
  expect_scan false "$path"
done
for path in Cargo.lock crates/api/Cargo.toml tools/webui/package-lock.json tools/eslint/package.json docs/example/package.json .github/workflows/socket.yml .gitignore docs/example/.gitignore mystery.bin; do
  expect_scan true "$path"
done
expect_scan true docs/INSTALL.md tools/webui/package-lock.json

scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT

# A typo in a route must fail before emitting a partial plan.
sed 's/^      add bootstrap$/      add bootstap/' "$selector" >"$scratch/invalid-selector.sh"
unknown_status=0
bash "$scratch/invalid-selector.sh" plan --files scripts/bootstrap-container.sh >"$scratch/unknown-plan" 2>"$scratch/unknown-error" || unknown_status=$?
[[ "$unknown_status" == 2 ]] || fail "unknown check group exited $unknown_status; expected 2"
[[ ! -s "$scratch/unknown-plan" ]] || fail 'unknown check group emitted a plan'
grep -Fxq 'select-checks: unknown check group: bootstap' "$scratch/unknown-error" || fail 'unknown check group diagnostic omitted the misspelling'

# Exercise actual Git ranges, deletions/renames, and job selection.
git init -q --initial-branch=main "$scratch/repo"
(
  cd "$scratch/repo"
  git config user.name 'Selector test'
  git config user.email 'selector@example.invalid'
  mkdir -p src frontend docs
  echo base >src/config.rs
  git add .
  git commit -qm base
  base="$(git rev-parse HEAD)"
  git switch -qc test-pr
  echo frontend >frontend/app.js
  git add .
  git commit -qm frontend
  echo change >>src/config.rs
  git commit -qam core
  head="$(git rev-parse HEAD)"
  # A later docs-only commit must not hide earlier code changes in the PR/push.
  echo notes >docs/new.md
  git add .
  git commit -qm docs
  export JMODGUARD_DIFF_BASE="$base" JMODGUARD_DIFF_HEAD="$(git rev-parse HEAD)"
  for event in pull_request push; do
    GITHUB_EVENT_NAME="$event" GITHUB_REF_TYPE=branch bash "$selector" plan >"$scratch/$event"
  done
  # A deletion and rename must select both affected surfaces.
  git rm -q frontend/app.js
  git mv src/config.rs docs/moved.md
  git commit -qm rename
  JMODGUARD_DIFF_BASE="$head" JMODGUARD_DIFF_HEAD="$(git rev-parse HEAD)" \
    GITHUB_EVENT_NAME=pull_request bash "$selector" plan >"$scratch/rename"
  # Proven empty PR changes are cheap; unavailable history is a full plan.
  JMODGUARD_DIFF_BASE="$base" JMODGUARD_DIFF_HEAD="$base" GITHUB_EVENT_NAME=pull_request \
    bash "$selector" plan >"$scratch/empty"
  JMODGUARD_DIFF_BASE=0000000000000000000000000000000000000000 GITHUB_EVENT_NAME=push \
    bash "$selector" plan >"$scratch/missing"
  JMODGUARD_DIFF_BASE=0000000000000000000000000000000000000000 GITHUB_EVENT_NAME=push \
    bash "$selector" socket >"$scratch/scan-missing"
  # A later docs commit must not hide a dependency change in the same PR.
  mkdir -p tools/webui
  echo '{}' >tools/webui/package.json
  git add .
  git commit -qm manifest
  echo more >>docs/new.md
  git commit -qam docs-after-manifest
  JMODGUARD_DIFF_BASE="$base" JMODGUARD_DIFF_HEAD="$(git rev-parse HEAD)" GITHUB_EVENT_NAME=pull_request \
    bash "$selector" socket >"$scratch/scan-history"
  # Deleting or renaming a manifest still needs a scan.
  manifest_head="$(git rev-parse HEAD)"
  git mv tools/webui/package.json docs/former-manifest.txt
  git commit -qm move-manifest
  JMODGUARD_DIFF_BASE="$manifest_head" JMODGUARD_DIFF_HEAD="$(git rev-parse HEAD)" GITHUB_EVENT_NAME=push \
    bash "$selector" socket >"$scratch/scan-rename"
)
for scan in missing history rename; do
  [[ "$(cat "$scratch/scan-$scan")" == true ]] || fail "scan $scan omitted dependency changes"
done
expected="$(printf '%s\n' $core frontend | sort)"
for event in pull_request push rename; do
  [[ "$(sort "$scratch/$event")" == "$expected" ]] || fail "$event omitted touched consumers"
done
[[ "$(cat "$scratch/empty")" == contracts ]] || fail 'proven empty diff should run contracts only'
[[ "$(sort "$scratch/missing")" == "$(printf '%s\n' $all | sort)" ]] || fail 'missing history must run all'
for event in schedule workflow_dispatch; do
  [[ "$(GITHUB_EVENT_NAME="$event" bash "$selector" plan | sort)" == "$(printf '%s\n' $all | sort)" ]] || fail "$event must run all"
done
[[ "$(GITHUB_EVENT_NAME=push GITHUB_REF_TYPE=tag bash "$selector" plan | sort)" == "$(printf '%s\n' $all | sort)" ]] || fail 'tags must run all'

expect_lint_packages() {
  local expected="$1" actual
  shift
  actual="$(bash "$selector" matrix --files "$@" | node -e '''let s="";process.stdin.on("data",x=>s+=x).on("end",()=>console.log(JSON.parse(s).include.find(j=>j.lane==="dylint")?.dylint_packages ?? ""))''')"
  [[ "$actual" == "$expected" ]] || fail "lint scope for $*: expected [$expected], got [$actual]"
}
expect_lint_packages 'jmodguard-api jmodguard-integration-tests' crates/api/src/lib.rs
expect_lint_packages 'jmodguard-api jmodguard-bot jmodguard-updater jmodguard-integration-tests' crates/bot/src/main.rs
expect_lint_packages 'jmodguard-api jmodguard-bot jmodguard-runtime jmodguard-updater jmodguard jmodguard-integration-tests' crates/runtime/src/lib.rs
expect_lint_packages 'jmodguard-api jmodguard-bot jmodguard-updater jmodguard jmodguard-integration-tests' src/config.rs
expect_lint_packages 'jmodguard jmodguard-integration-tests' crates/integration-tests/tests/remote_database_smoke.rs
expect_lint_packages '' Cargo.lock
expect_lint_packages '' dylint.toml crates/api/src/lib.rs
expect_lint_packages '' mystery.bin
bash "$selector" matrix --files docs/INSTALL.md >"$scratch/docs.json"
bash "$selector" matrix --files src/db.rs >"$scratch/core.json"
node --input-type=module - "$scratch" <<'JS'
import fs from "node:fs";
import assert from "node:assert/strict";
const read = name => JSON.parse(fs.readFileSync(`${process.argv[2]}/${name}.json`));
const docs = read("docs").include;
assert.equal(docs.length, 1);
assert.equal(docs[0].groups, "contracts");
assert.equal(docs[0].rust, false);
const core = read("core").include;
assert.deepEqual(core.filter(j => j.lane.startsWith("rust-")).map(j => [j.lane, j.groups]), [
  ["rust-api", "api boundaries"],
  ["rust-bot", "bot integration"],
  ["rust-core", "updater core"],
]);
assert.equal(new Set(core.flatMap(j => j.groups.split(" "))).size, core.flatMap(j => j.groups.split(" ")).length);
JS
# Matrix scheduling must neither omit nor duplicate a selected gate, including
# narrow service changes and the full fallback plan.
for path in docs/INSTALL.md crates/api/src/lib.rs crates/bot/src/main.rs crates/runtime/src/lib.rs crates/updater/src/main.rs crates/integration-tests/src/lib.rs src/db.rs Cargo.lock mystery.bin; do
  bash "$selector" plan --files "$path" >"$scratch/plan"
  bash "$selector" matrix --files "$path" >"$scratch/matrix.json"
  node --input-type=module - "$scratch" <<'JS'
import fs from "node:fs";
import assert from "node:assert/strict";
const root = process.argv[2];
const expected = fs.readFileSync(`${root}/plan`, "utf8").trim().split(/\s+/).sort();
const jobs = JSON.parse(fs.readFileSync(`${root}/matrix.json`, "utf8")).include;
assert.deepEqual(jobs.flatMap(job => job.groups.split(" ")).sort(), expected);
for (const job of jobs.filter(job => job.lane.startsWith("rust-"))) {
  assert.equal(job.rust, true);
  assert.equal(job.timeout, 90);
}
JS
done
echo 'select-checks-test: ok'
