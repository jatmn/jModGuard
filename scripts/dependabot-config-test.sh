#!/usr/bin/env bash
# Guard Dependabot YAML and the operator-copy contract for version updates.
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
cd "$root"

fail() {
  echo "dependabot-config-test: $*" >&2
  exit 1
}

require_contains() {
  local path="$1"
  local needle="$2"
  grep -Fq -- "$needle" "$root/$path" || fail "missing in $path: $needle"
}

require_line() {
  local path="$1"
  local line="$2"
  grep -Fxq -- "$line" "$root/$path" || fail "missing line in $path: $line"
}

require_missing_key_skip() {
  local path="$1"
  # The missing-key exit must be inside its guard and before the first scan.
  awk '
    $0 == "          if [ -z \"$SOCKET_SECURITY_API_KEY\" ]; then" && !guard { guard = NR; inside = 1; next }
    inside && $0 == "            exit 0" { skip = NR; next }
    inside && $0 == "          fi" { end = NR; inside = 0; next }
    $0 == "          socketcli \\" && !scan { scan = NR }
    END { exit !(guard && skip > guard && end > skip && scan > end) }
  ' "$root/$path" || fail "missing-key guard must exit 0 before socketcli in $path"
}

require_absent() {
  local path="$1"
  local needle="$2"
  if grep -Fq -- "$needle" "$root/$path"; then
    fail "forbidden in $path: $needle"
  fi
}

yaml=".github/dependabot.yml"
development="development.md"
buildkite="docs/BUILDKITE.md"
deploy="docs/DEPLOY-BUNNY.md"
agents="AGENTS.md"
preflight="scripts/ci-preflight.sh"

for needle in \
  "version: 2" \
  "package-ecosystem: cargo" \
  'directory: "/"' \
  "interval: weekly" \
  "open-pull-requests-limit: 5" \
  "cargo-minor-and-patch" \
  "- minor" \
  "- patch" \
  "dependency-name: libsql-sqlite3-parser"
do
  require_contains "$yaml" "$needle"
done

for needle in \
  "package-ecosystem: npm" \
  'directory: "/tools/eslint"' \
  "npm-minor-and-patch"
do
  require_contains "$yaml" "$needle"
done

require_absent "$yaml" "package-ecosystem: github-actions"
require_absent "$yaml" "package-ecosystem: docker"
require_absent "$yaml" '${{'
if [ -d "$root/.github/workflows" ]; then
  for required in pullfrog.yml socket.yml; do
    if [ ! -f "$root/.github/workflows/$required" ]; then
      fail "missing .github/workflows/$required"
    fi
  done
  while IFS= read -r -d '' workflow; do
    case "$workflow" in
      "$root/.github/workflows/pullfrog.yml"|"$root/.github/workflows/socket.yml") ;;
      *) fail "only the Pullfrog and Socket Security workflows are allowed under .github/workflows" ;;
    esac
  done < <(find "$root/.github/workflows" -mindepth 1 -maxdepth 1 -print0)
fi
socket_workflow=".github/workflows/socket.yml"
for needle in \
  "name: socket-security-workflow" \
  "SOCKET_SECURITY_API_KEY" \
  "socketsecurity==2.10.2" \
  "socketcli" \
  "--scm github" \
  "actions/checkout@d23441a48e516b6c34aea4fa41551a30e30af803" \
  "--exit-code-on-api-error 0" \
  "persist-credentials: false" \
  "fetch-depth: 0" \
  "issues: write"
do
  require_contains "$socket_workflow" "$needle"
done
require_line "$socket_workflow" '    types: [opened, synchronize, reopened]'
require_line "$socket_workflow" "          EVENT_ACTION: \${{ github.event_name == 'pull_request' && github.event.action == 'reopened' && 'synchronize' || '' }}"
require_missing_key_skip "$socket_workflow"
require_absent "$socket_workflow" "issue_comment:"
require_absent "$socket_workflow" "repo-is-public"
require_absent "$socket_workflow" "pull-requests: write"
require_contains "$socket_workflow" "head.repo.full_name == github.repository"
require_contains "$socket_workflow" "skipping the Socket scan"
require_contains "$socket_workflow" 'group: socket-scan-${{ github.ref }}'
require_contains "$socket_workflow" "cancel-in-progress: true"
# The group is the branch or pull-request ref. Including github.sha gives every
# commit its own group, so cancel-in-progress cannot stop the scan it replaces.
require_absent "$socket_workflow" "github.sha"
for doc in AGENTS.md development.md docs/BUILDKITE.md docs/DEPLOY-BUNNY.md docs/PULLFROG.md; do
  require_contains "$doc" "SOCKET_SECURITY_API_KEY"
  require_contains "$doc" "not stored in git."
  require_contains "$doc" "uploads discovered Cargo and npm manifests outside ignored paths to Socket"
  require_contains "$doc" "A new blocking alert can fail this check only when Socket compares a pull request change that touches a supported manifest."
  require_contains "$doc" "Push and full-report scans do not fail for alerts. Socket API errors do not fail this check."
  require_contains "$doc" "without a main baseline, Socket may treat existing dependencies as newly added."
done

for needle in \
  "Dependabot on self-hosted runners" \
  '`dependabot`' \
  "no GitHub Actions self-hosted runners" \
  "24 hours" \
  "GitHub Packages"
do
  require_contains "$yaml" "$needle"
done

for path in "$development" "$buildkite" "$deploy" "$agents"; do
  require_contains "$path" "Dependabot"
done

require_contains "$development" "Actions → Dependabot Updates"
require_contains "$development" "Dependabot on self-hosted runners"
require_contains "$development" "24 hours"
require_contains "$development" "idle"
require_contains "$development" "no GitHub Actions self-hosted runners"
require_contains "$development" "bash scripts/dependabot-config-test.sh"
require_contains "$buildkite" "Actions → Dependabot Updates"
require_contains "$buildkite" "Dependabot on self-hosted"
require_contains "$buildkite" "no registered GitHub Actions self-hosted runners"
require_absent "$buildkite" "No Actions jobs should start for this repo."
require_contains "$deploy" "no GitHub Actions CI or"
require_contains "$deploy" "Dependabot on self-hosted runners"
require_absent "$deploy" "still use GitHub-hosted"
require_absent "$deploy" "This repository has no GitHub Actions jobs"
require_contains "$agents" "bash scripts/dependabot-config-test.sh"
require_contains "$agents" "Dependabot on self-hosted runners"
require_contains "$agents" "24 hours"
require_contains "$agents" "no GitHub Actions self-hosted"
require_contains "$preflight" "bash scripts/dependabot-config-test.sh"
require_contains "$preflight" "GitHub Actions workflows and GitHub-hosted CI runners are forbidden"
require_contains "$preflight" "Dependabot Updates dynamic workflows are not CI"
require_contains "$preflight" "npm audit --prefix tools/eslint --audit-level=high"
require_contains "$agents" "npm audit --prefix tools/eslint --audit-level=high"
require_contains "$buildkite" "npm audit --prefix tools/eslint --audit-level=high"
require_contains "$development" "five open version-update PRs per ecosystem"

echo 'dependabot-config-test: ok'
