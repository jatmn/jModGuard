#!/usr/bin/env bash
# Open or merge Release Please Stable PRs from Buildkite. Nightlies stay separate.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

fail() {
  echo "Release Please failed: $*" >&2
  exit 1
}

node --input-type=module - "$root/release-please-config.json" "$root/.release-please-manifest.json" <<'JS'
import fs from "node:fs";

const config = JSON.parse(fs.readFileSync(process.argv[2], "utf8"));
const manifest = JSON.parse(fs.readFileSync(process.argv[3], "utf8"));
if (config["release-type"] !== "simple" || config["include-v-in-tag"] !== true) {
  process.stderr.write("Release Please must manage v-prefixed Stable tags only\n");
  process.exit(1);
}
if (!manifest || typeof manifest !== "object" || !("." in manifest)) {
  process.stderr.write("Release Please manifest must version the repository root\n");
  process.exit(1);
}
JS

if [[ "${BUILDKITE_PULL_REQUEST:-false}" != "false" ]]; then
  echo 'Release Please skipped on pull requests'
  exit 0
fi
if [[ "${BUILDKITE_SOURCE:-}" != "webhook" || "${BUILDKITE_BRANCH:-}" != "main" ]]; then
  echo 'Release Please skipped outside trusted main pushes'
  exit 0
fi
if [[ -n "${BUILDKITE_TAG:-}" ]]; then
  echo 'Release Please skipped on existing tags'
  exit 0
fi

token="${GITHUB_TOKEN:-}"
if [[ -z "$token" || "$token" == '$GITHUB_TOKEN' ]]; then
  token="$(buildkite-agent secret get GITHUB_TOKEN 2>/dev/null || true)"
fi
if [[ -z "$token" ]]; then
  echo 'Release Please skipped without GITHUB_TOKEN'
  exit 0
fi

repo="${BUILDKITE_REPO:-https://github.com/jatmn/jModGuard}"
for command in release-pr github-release; do
  npx --yes release-please "$command" "--token=${token}" "--repo-url=${repo}" \
    || fail "npx release-please $command"
done
