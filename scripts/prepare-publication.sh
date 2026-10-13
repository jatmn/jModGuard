#!/usr/bin/env bash
# Persist only this trusted workflow run's immutable source plan, never PR artifacts.
set -euo pipefail
mkdir -p "$RUNNER_TEMP/publication"
export JMODGUARD_PUBLICATION_PLAN_FILE="$RUNNER_TEMP/publication/plan.json"
# An API failure is not proof of absence. Do not re-resolve a tag on failure.
artifacts="$(gh api "repos/$GITHUB_REPOSITORY/actions/runs/$GITHUB_RUN_ID/artifacts" --paginate --jq '.artifacts[] | select(.name == "publication-plan") | [.id, .expired] | @tsv')"
reused=false
if [[ -n "$artifacts" ]]; then
  [[ "$artifacts" != *$'\n'* && "$artifacts" == *$'\tfalse' ]] || {
    echo 'Publication plan is expired or ambiguous; start a new run explicitly' >&2; exit 1;
  }
  gh run download "$GITHUB_RUN_ID" --repo "$GITHUB_REPOSITORY" --name publication-plan --dir "$RUNNER_TEMP/publication"
  reused=true
  # Release Please may have already created the release on the first attempt.
  # Keep that original tag even if the retried action now reports no new release.
  if [[ "$GITHUB_EVENT_NAME" == push && "$GITHUB_REF" == refs/heads/main ]]; then
    JMODGUARD_RELEASE_TAG="$(node -e 'const fs=require("node:fs"); const p=JSON.parse(fs.readFileSync(process.env.JMODGUARD_PUBLICATION_PLAN_FILE)); console.log(p.mode === "tag" ? p.ref.replace(/^refs\/tags\//, "") : "")')"
    export JMODGUARD_RELEASE_TAG
  fi
elif [[ "${GITHUB_RUN_ATTEMPT:-1}" != 1 ]]; then
  echo 'Publication plan is missing on a rerun; start a new run explicitly' >&2
  exit 1
fi
bash scripts/publish-image.sh prepare
plan="$(cat "$JMODGUARD_PUBLICATION_PLAN_FILE")"
# JSON is serialized as a single line before entering the workflow output file.
plan="$(PLAN_JSON="$plan" node -e 'console.log(JSON.stringify(JSON.parse(process.env.PLAN_JSON)))')"
{
  echo "plan=$plan"
  echo "reused=$reused"
  echo "release_tag=${JMODGUARD_RELEASE_TAG:-}"
} >>"$GITHUB_OUTPUT"
