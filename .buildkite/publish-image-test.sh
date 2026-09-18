#!/usr/bin/env bash
# Offline publication contract tests; never log in, push, or contact Buildkite.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

fail() {
  echo "publish-image-test: $*" >&2
  exit 1
}

# shellcheck disable=SC1091
JMODGUARD_PUBLISH_IMAGE_LIB=1 source "$root/.buildkite/publish-image.sh"

HEAD_SHA="$(printf 'a%.0s' {1..40})"
BASE_SHA="$(printf 'b%.0s' {1..40})"

with_main() {
  BUILDKITE=true
  BUILDKITE_SOURCE=webhook
  BUILDKITE_BRANCH=main
  BUILDKITE_COMMIT="$HEAD_SHA"
  BUILDKITE_PULL_REQUEST=false
  BUILDKITE_RETRY_COUNT=0
  BUILDKITE_TAG=""
  BUILDKITE_GITHUB_EVENT=push
  JMODGUARD_PUBLISH_SOURCE=""
  export BUILDKITE BUILDKITE_SOURCE BUILDKITE_BRANCH BUILDKITE_COMMIT \
    BUILDKITE_PULL_REQUEST BUILDKITE_RETRY_COUNT BUILDKITE_TAG BUILDKITE_GITHUB_EVENT \
    JMODGUARD_PUBLISH_SOURCE
}

assert_eq() {
  [[ "$1" == "$2" ]] || fail "expected '$2', got '$1'"
}

assert_request() {
  local got
  got="$(publish_request)"
  assert_eq "$got" "$1"
}

with_main
assert_request $'main\t'
BUILDKITE_TAG=v1 BUILDKITE_BRANCH=v1 assert_request $'tag\trefs/tags/v1'

BUILDKITE_PULL_REQUEST=7 assert_request skip
BUILDKITE_PULL_REQUEST=false
BUILDKITE_BRANCH=feature assert_request skip
BUILDKITE_BRANCH=main
BUILDKITE_SOURCE=ui assert_request skip
BUILDKITE_SOURCE=webhook
BUILDKITE_GITHUB_EVENT=pull_request assert_request skip
BUILDKITE_GITHUB_EVENT=release assert_request skip
BUILDKITE_GITHUB_EVENT=push
BUILDKITE_SOURCE=schedule assert_request $'nightly\t'
BUILDKITE_SOURCE=webhook
JMODGUARD_PUBLISH_SOURCE="$HEAD_SHA" assert_request $'main\t'
BUILDKITE_TAG=v1 BUILDKITE_BRANCH=v1 JMODGUARD_PUBLISH_SOURCE="$HEAD_SHA" \
  assert_request $'tag\trefs/tags/v1'
unset BUILDKITE
if ( publish_request ) >/dev/null 2>&1; then
  fail 'missing BUILDKITE must fail'
fi
with_main

BUILDKITE_SOURCE=ui
JMODGUARD_PUBLISH_SOURCE="$HEAD_SHA" assert_request $'manual\t'"$HEAD_SHA"
JMODGUARD_PUBLISH_SOURCE='refs/tags/v1' assert_request $'manual\trefs/tags/v1'
JMODGUARD_PUBLISH_SOURCE='refs/tags/release/v1+meta' assert_request $'manual\trefs/tags/release/v1+meta'
for ref in main HEAD abc123 refs/heads/main v1 -oops; do
  JMODGUARD_PUBLISH_SOURCE="$ref"
  if ( publish_request ) >/dev/null 2>&1; then
    fail "manual source $ref must fail"
  fi
done
JMODGUARD_PUBLISH_SOURCE="$HEAD_SHA"
BUILDKITE_BRANCH=feature
if ( publish_request ) >/dev/null 2>&1; then
  fail 'manual off main must fail'
fi
with_main

grep -Eq 'eval |bash -c|python3|shell=True' "$root/.buildkite/publish-image.sh" \
  && fail 'publisher must not eval, invoke python, or use bash -c'
grep -Fq 'run_git() { git "$@" || publish_fail "git failed: $*"; }' "$root/.buildkite/publish-image.sh" \
  || fail 'publisher must invoke git as an argv list'
grep -Fq 'run_docker() { docker "$@" || publish_fail "docker failed: $*"; }' "$root/.buildkite/publish-image.sh" \
  || fail 'publisher must invoke docker as an argv list'
grep -Fq 'run_bk() { buildkite-agent "$@" || publish_fail "buildkite-agent failed: $*"; }' "$root/.buildkite/publish-image.sh" \
  || fail 'publisher must invoke buildkite-agent as an argv list'

commands=0
run_git() { commands=$((commands + 1)); fail "unexpected git $*"; }
run_docker() { commands=$((commands + 1)); fail "unexpected docker $*"; }
run_bk() { commands=$((commands + 1)); fail "unexpected buildkite $*"; }
BUILDKITE_PULL_REQUEST=1
prepare
publish_component bot
BUILDKITE_PULL_REQUEST=false
BUILDKITE_SOURCE=ui
JMODGUARD_PUBLISH_SOURCE=""
prepare
publish_component bot
assert_eq "$commands" "0"
with_main

# Restore real helpers after the skip test.
# shellcheck disable=SC1091
JMODGUARD_PUBLISH_IMAGE_LIB=1 source "$root/.buildkite/publish-image.sh"

log=()
run_git() {
  case "$1 $2" in
    'rev-parse HEAD') printf '%s\n' "$HEAD_SHA" ;;
    'status --porcelain') printf '' ;;
    *) fail "unexpected git $*" ;;
  esac
}
run_bk() {
  log+=("$*")
  case "$1 $2 $3" in
    'meta-data exists image-publication') return 100 ;;
    'meta-data set image-publication') return 0 ;;
    *) fail "unexpected buildkite $*" ;;
  esac
}
bk_meta_exists() { return 100; }
resolve_source() {
  assert_eq "$1" manual
  assert_eq "$2" refs/tags/v1
  assert_eq "$3" "$HEAD_SHA"
  printf '%s\t%s\n' "$BASE_SHA" v1
}
BUILDKITE_SOURCE=ui
JMODGUARD_PUBLISH_SOURCE='refs/tags/v1'
prepare
[[ "${log[0]:-}" == meta-data\ set\ image-publication\ * ]] || fail "prepare must write the plan: ${log[*]:-}"
plan="${log[0]#meta-data set image-publication }"
node --input-type=commonjs - <<JS
const plan = ${plan};
if (plan.mode !== "manual" || plan.ref !== "refs/tags/v1" || plan.commit !== "${HEAD_SHA}"
    || plan.sha !== "${BASE_SHA}" || plan.tag !== "v1") {
  process.exit(1);
}
JS

# shellcheck disable=SC1091
JMODGUARD_PUBLISH_IMAGE_LIB=1 source "$root/.buildkite/publish-image.sh"
run_bk() {
  case "$1 $2" in
    'meta-data exists') return 0 ;;
    *) fail "retry must not resolve again: $*" ;;
  esac
}
bk_meta_exists() { return 0; }
read_plan() {
  printf '%s\t%s\n' "$BASE_SHA" v1
}
resolve_source() { fail 'retry must not resolve_source'; }
checked_request() { printf 'manual\t%s\t%s\n' 'refs/tags/v1' "$HEAD_SHA"; }
prepare

# shellcheck disable=SC1091
JMODGUARD_PUBLISH_IMAGE_LIB=1 source "$root/.buildkite/publish-image.sh"
run_git() {
  if [[ "$1" == rev-parse ]]; then
    printf '%s\n' "$BASE_SHA"
    return
  fi
  if [[ "$1" == status ]]; then
    printf ' M src/lib.rs\n'
    return
  fi
  fail "unexpected git $*"
}
run_docker() { fail 'bad checkout must not touch docker'; }
run_bk() { fail 'bad checkout must not touch buildkite'; }
if ( publish_component bot ) >/dev/null 2>&1; then
  fail 'dirty checkout must fail'
fi

contains_arg() {
  local needle="$1"
  shift
  local arg
  for arg in "$@"; do
    [[ "$arg" == "$needle" ]] && return 0
  done
  return 1
}

readarray -d '' -t nightly_args < <(build_args bot "$HEAD_SHA" "nightly-20260915-${HEAD_SHA:0:12}" . nightly)
contains_arg "ghcr.io/jatmn/jmodguard-bot:latest" "${nightly_args[@]}" \
  && fail 'nightly must not move latest'
contains_arg "ghcr.io/jatmn/jmodguard-bot:${HEAD_SHA}" "${nightly_args[@]}" \
  || fail 'nightly must tag the SHA'
contains_arg "ghcr.io/jatmn/jmodguard-bot:nightly-20260915-${HEAD_SHA:0:12}" "${nightly_args[@]}" \
  || fail 'nightly must tag the unique label'

readarray -d '' -t stable_args < <(build_args bot "$HEAD_SHA" v1.2.3 . stable)
contains_arg "ghcr.io/jatmn/jmodguard-bot:latest" "${stable_args[@]}" \
  || fail 'stable must move latest'
contains_arg "ghcr.io/jatmn/jmodguard-bot:v1.2.3" "${stable_args[@]}" \
  || fail 'stable must tag the release'

# shellcheck disable=SC1091
JMODGUARD_PUBLISH_IMAGE_LIB=1 source "$root/.buildkite/publish-image.sh"
run_git() {
  case "$1" in
    status|rev-parse) if [[ "$1" == status ]]; then printf ''; else printf '%s\n' "$HEAD_SHA"; fi ;;
    *) fail "unexpected git $*" ;;
  esac
}
run_docker() { fail 'main conductor/manifest must not use docker'; }
run_bk() { fail 'main conductor/manifest must not use buildkite'; }
GHCR_USERNAME=jatmn
export GHCR_USERNAME
with_main
publish_component conductor
publish_component manifest

scratch="$(mktemp -d "${TMPDIR:-/tmp}/publish-image-git-XXXXXX")"
cleanup_git() { rm -rf "$scratch"; }
trap cleanup_git EXIT
git init -b main "$scratch" >/dev/null
git -C "$scratch" config user.name Fixture
git -C "$scratch" config user.email fixture@example.invalid
mkdir -p "$scratch/src"
echo old >"$scratch/src/lib.rs"
git -C "$scratch" add .
git -C "$scratch" commit -m base >/dev/null
base="$(git -C "$scratch" rev-parse HEAD)"
git -C "$scratch" tag -a 'release/v1+meta' -m release
git -C "$scratch" remote add origin "$scratch"

pushd "$scratch" >/dev/null
# shellcheck disable=SC1091
JMODGUARD_PUBLISH_IMAGE_LIB=1 source "$root/.buildkite/publish-image.sh"
mkdir -p frontend
echo changed >frontend/style.css
git add .
git commit -m change >/dev/null
head="$(git rev-parse HEAD)"
resolved="$(resolve_source manual refs/tags/release/v1+meta "$head")"
assert_eq "$resolved" "$(printf '%s\t%s\n' "$base" 'release-v1-meta')"
resolved="$(resolve_source manual "$base" "$head")"
assert_eq "$resolved" "$(printf '%s\t\n' "$base")"
if ( resolve_source tag refs/tags/release/v1+meta "$head" ) >/dev/null 2>&1; then
  fail 'moved tag must fail in tag mode'
fi
if ( resolve_source manual 'refs/tags/../bad' "$head" ) >/dev/null 2>&1; then
  fail 'bad ref must fail'
fi
popd >/dev/null

echo 'publish-image-test: ok'
