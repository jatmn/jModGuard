#!/usr/bin/env bash
# Exercise the actual plan transport with real Git and a simulated artifact API.
set -euo pipefail
root="$(git rev-parse --show-toplevel)"
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/repo/scripts" "$scratch/bin" "$scratch/runner" "$scratch/artifact"
cp "$root/scripts/publish-image.sh" "$root/scripts/prepare-publication.sh" "$scratch/repo/scripts/"
git init -q -b main "$scratch/repo"
cd "$scratch/repo"
git config user.name 'Publication fixture'
git config user.email 'publication@example.invalid'
git config commit.gpgsign false
git config tag.gpgsign false
git add .
git commit -qm base
base="$(git rev-parse HEAD)"
git tag v1.2.3
git remote add origin .
git commit -q --allow-empty -m later
later="$(git rev-parse HEAD)"
git checkout -q --detach "$base"
cat >"$scratch/bin/gh" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
if [[ "$1" == api ]]; then
  [[ "${API_FAIL:-0}" == 0 ]] || exit 1
  if [[ -f "$ARTIFACT_PLAN" ]]; then
    printf '123\t%s\n' "${ARTIFACT_EXPIRED:-false}"
  fi
elif [[ "$1 $2" == 'run download' ]]; then
  cp "$ARTIFACT_PLAN" "$RUNNER_TEMP/publication/plan.json"
else
  exit 2
fi
SH
chmod +x "$scratch/bin/gh"
export PATH="$scratch/bin:$PATH" RUNNER_TEMP="$scratch/runner" ARTIFACT_PLAN="$scratch/artifact/plan.json"
export GITHUB_ACTIONS=true GITHUB_REPOSITORY=jatmn/jModGuard GITHUB_EVENT_NAME=workflow_dispatch
export GITHUB_REF=refs/heads/main GITHUB_SHA="$base" GITHUB_RUN_ID=123
export GITHUB_RUN_ATTEMPT=1
export GITHUB_ACTOR=jatmn GITHUB_ACTOR_ID=12479882 GITHUB_TRIGGERING_ACTOR=jatmn
export GITHUB_OUTPUT="$scratch/outputs" JMODGUARD_PUBLISH_SOURCE=refs/tags/v1.2.3
bash scripts/prepare-publication.sh
cp "$RUNNER_TEMP/publication/plan.json" "$ARTIFACT_PLAN"
PLAN="$ARTIFACT_PLAN" BASE="$base" node -e 'const a=require("node:assert/strict"),fs=require("node:fs");const p=JSON.parse(fs.readFileSync(process.env.PLAN));a.equal(p.sha,process.env.BASE);a.equal(p.ref,"refs/tags/v1.2.3");'
git tag -f v1.2.3 "$later" >/dev/null
export GITHUB_RUN_ATTEMPT=2
rm -rf "$RUNNER_TEMP/publication"
: >"$GITHUB_OUTPUT"
bash scripts/prepare-publication.sh
cmp "$ARTIFACT_PLAN" "$RUNNER_TEMP/publication/plan.json"
grep -Fxq 'reused=true' "$GITHUB_OUTPUT"

# API failure and expired plans cannot be treated as permission to resolve again.
for failure in API_FAIL=1 ARTIFACT_EXPIRED=true; do
  if env "$failure" bash scripts/prepare-publication.sh >/dev/null 2>&1; then
    echo "prepare-publication-test: accepted $failure" >&2; exit 1
  fi
done

# Deleted/fully expired artifacts disappear from the API, too. A retry must not
# resolve the moved tag again even when GitHub returns a successful empty list.
mv "$ARTIFACT_PLAN" "$ARTIFACT_PLAN.saved"
rm -rf "$RUNNER_TEMP/publication"
if bash scripts/prepare-publication.sh >/dev/null 2>&1; then
  echo 'prepare-publication-test: rerun accepted missing plan' >&2; exit 1
fi
mv "$ARTIFACT_PLAN.saved" "$ARTIFACT_PLAN"

# Release Please can select a commit newer or older than the triggering event.
# A retry must restore that release even with empty action outputs and a moved tag.
export GITHUB_EVENT_NAME=push JMODGUARD_PUBLISH_SOURCE= JMODGUARD_RELEASE_TAG=v1.2.3
for sources in "$base:$later" "$later:$base" "$base:$base"; do
  export GITHUB_SHA="${sources%%:*}" JMODGUARD_RELEASE_SHA="${sources#*:}"
  git checkout -q --detach "$GITHUB_SHA"
  export GITHUB_RUN_ATTEMPT=1
  git tag -f v1.2.3 "$JMODGUARD_RELEASE_SHA" >/dev/null
  rm -f "$ARTIFACT_PLAN" "$RUNNER_TEMP/publication/plan.json"
  : >"$GITHUB_OUTPUT"
  bash scripts/prepare-publication.sh
  cp "$RUNNER_TEMP/publication/plan.json" "$ARTIFACT_PLAN"
  PLAN="$ARTIFACT_PLAN" node - <<'JS'
const a = require('node:assert/strict'), fs = require('node:fs');
const p = JSON.parse(fs.readFileSync(process.env.PLAN));
a.equal(p.sha, process.env.JMODGUARD_RELEASE_SHA, 'plan must select the release source');
a.equal(p.commit, process.env.GITHUB_SHA, 'plan must retain the workflow identity');
a.equal(p.mode, 'release');
a.equal(p.tag, 'v1.2.3');
JS
  release_sha="$JMODGUARD_RELEASE_SHA"
  unset JMODGUARD_RELEASE_TAG JMODGUARD_RELEASE_SHA
  git tag -f v1.2.3 "$GITHUB_SHA" >/dev/null
  export GITHUB_RUN_ATTEMPT=2
  rm -rf "$RUNNER_TEMP/publication"
  : >"$GITHUB_OUTPUT"
  bash scripts/prepare-publication.sh
  grep -Fxq 'release_tag=v1.2.3' "$GITHUB_OUTPUT"
  grep -Fxq "release_sha=$release_sha" "$GITHUB_OUTPUT"
  cmp "$ARTIFACT_PLAN" "$RUNNER_TEMP/publication/plan.json"
  export JMODGUARD_RELEASE_TAG=v1.2.3
done

# Invalid action identities and genuine tag-push drift must fail without a plan.
rm -f "$ARTIFACT_PLAN" "$RUNNER_TEMP/publication/plan.json"
export GITHUB_RUN_ATTEMPT=1
git tag -f v1.2.3 "$later" >/dev/null
for release_sha in '' invalid "$base"; do
  if JMODGUARD_RELEASE_SHA="$release_sha" bash scripts/prepare-publication.sh >"$scratch/rejected" 2>&1; then
    echo 'prepare-publication-test: accepted missing, invalid or mismatched release SHA' >&2; exit 1
  fi
  test ! -f "$RUNNER_TEMP/publication/plan.json"
done
unset JMODGUARD_RELEASE_TAG JMODGUARD_RELEASE_SHA
if GITHUB_REF=refs/tags/v1.2.3 bash scripts/prepare-publication.sh >"$scratch/rejected" 2>&1; then
  echo 'prepare-publication-test: accepted tag-push drift' >&2; exit 1
fi
test ! -f "$RUNNER_TEMP/publication/plan.json"

echo 'prepare-publication-test: ok'
