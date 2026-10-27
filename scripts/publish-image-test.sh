#!/usr/bin/env bash
# Offline publication contract tests; never log in, push, or contact GitHub Actions.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

fail() {
  echo "publish-image-test: $*" >&2
  exit 1
}

# shellcheck disable=SC1091
JMODGUARD_PUBLISH_IMAGE_LIB=1 source "$root/scripts/publish-image.sh"

HEAD_SHA="$(printf 'a%.0s' {1..40})"
BASE_SHA="$(printf 'b%.0s' {1..40})"

with_main() {
  GITHUB_ACTIONS=true
  GITHUB_REPOSITORY=jatmn/jModGuard
  GITHUB_EVENT_NAME=push
  GITHUB_REF=refs/heads/main
  GITHUB_SHA="$HEAD_SHA"
  GITHUB_RUN_ATTEMPT=1
  GITHUB_ACTOR=jatmn
  GITHUB_TRIGGERING_ACTOR=jatmn
  GITHUB_ACTOR_ID=12479882
  JMODGUARD_PUBLISH_SOURCE=""
  JMODGUARD_RELEASE_TAG=""
  JMODGUARD_RELEASE_SHA=""
  export GITHUB_ACTIONS GITHUB_REPOSITORY GITHUB_EVENT_NAME GITHUB_REF GITHUB_SHA \
    GITHUB_RUN_ATTEMPT GITHUB_ACTOR GITHUB_TRIGGERING_ACTOR GITHUB_ACTOR_ID JMODGUARD_PUBLISH_SOURCE \
    JMODGUARD_RELEASE_TAG JMODGUARD_RELEASE_SHA
}

assert_eq() {
  [[ "$1" == "$2" ]] || fail "expected '$2', got '$1'"
}

assert_request() {
  local got
  got="$(publish_request)"
  assert_eq "$got" "$1"
}

# Stable releases retain the root package and version manifest contract.
node --input-type=commonjs - <<'JS'
const assert = require("node:assert/strict");
const fs = require("node:fs");
const config = JSON.parse(fs.readFileSync("release-please-config.json", "utf8"));
const manifest = JSON.parse(fs.readFileSync(".release-please-manifest.json", "utf8"));
assert.equal(config["release-type"], "simple");
assert.equal(config["include-v-in-tag"], true);
assert.deepEqual(Object.keys(config.packages), ["."]);
assert.deepEqual(Object.keys(manifest), ["."]);
assert.match(manifest["."], /^[0-9]+\.[0-9]+\.[0-9]+$/);
JS

with_main
assert_request $'main\t'
JMODGUARD_RELEASE_TAG=v1.2.3 JMODGUARD_RELEASE_SHA="$BASE_SHA" assert_request $'release\trefs/tags/v1.2.3'
GITHUB_REF=refs/tags/v1 assert_request $'tag\trefs/tags/v1'
for event in pull_request pull_request_target merge_group release issue_comment; do
  GITHUB_EVENT_NAME="$event" assert_request skip
done
GITHUB_REPOSITORY=outsider/jModGuard assert_request skip
GITHUB_REF=refs/heads/feature assert_request skip
GITHUB_EVENT_NAME=schedule assert_request $'nightly\t'
GITHUB_EVENT_NAME=workflow_dispatch assert_request skip
unset GITHUB_ACTIONS
if (publish_request) >/dev/null 2>&1; then fail 'non-Actions publication must fail'; fi
with_main
GITHUB_EVENT_NAME=workflow_dispatch
JMODGUARD_PUBLISH_SOURCE="$HEAD_SHA" assert_request $'manual\t'"$HEAD_SHA"
JMODGUARD_PUBLISH_SOURCE='refs/tags/release/v1+meta' assert_request $'manual\trefs/tags/release/v1+meta'
for ref in main HEAD abc123 refs/heads/main v1 -oops; do
  if (JMODGUARD_PUBLISH_SOURCE="$ref" publish_request) >/dev/null 2>&1; then fail "manual source $ref must fail"; fi
done
JMODGUARD_PUBLISH_SOURCE="$HEAD_SHA"
for actor in GITHUB_ACTOR GITHUB_TRIGGERING_ACTOR GITHUB_ACTOR_ID; do
  if (export "$actor=outsider"; publish_request) >/dev/null 2>&1; then fail "$actor must be owner-only"; fi
done
if (GITHUB_REF=refs/heads/feature publish_request) >/dev/null 2>&1; then fail 'manual publication must use main'; fi
with_main

# Preserve the publisher's argv-only execution contract.
grep -Eq 'eval |bash -c|python3|shell=True' "$root/scripts/publish-image.sh" \
  && fail 'publisher must not eval, invoke python, or use bash -c'
grep -Fq 'run_git() { git "$@" || publish_fail "git failed: $*"; }' "$root/scripts/publish-image.sh" \
  || fail 'publisher must invoke git as an argv list'
grep -Fq 'run_docker() { docker "$@" || publish_fail "docker failed: $*"; }' "$root/scripts/publish-image.sh" \
  || fail 'publisher must invoke docker as an argv list'

# Real file-backed plan survives retries without resolving a moved tag again.
plan_scratch="$(mktemp -d)"
JMODGUARD_PUBLICATION_PLAN_FILE="$plan_scratch/plan.json"
export JMODGUARD_PUBLICATION_PLAN_FILE
run_git() {
  case "$1 $2" in
    'rev-parse HEAD') printf '%s\n' "$HEAD_SHA" ;;
    'status --porcelain') printf '' ;;
    *) fail "unexpected git $*" ;;
  esac
}
resolve_source() { printf '%s\t%s\n' "$BASE_SHA" v1; }
GITHUB_EVENT_NAME=workflow_dispatch JMODGUARD_PUBLISH_SOURCE=refs/tags/v1 prepare
JMODGUARD_PUBLISH_SOURCE=refs/tags/v1 GITHUB_EVENT_NAME=workflow_dispatch read_plan manual refs/tags/v1 "$HEAD_SHA" >"$plan_scratch/resolved"
assert_eq "$(cat "$plan_scratch/resolved")" "$(printf '%s\t%s' "$BASE_SHA" v1)"
resolve_source() { fail 'retry must not resolve the source again'; }
GITHUB_EVENT_NAME=workflow_dispatch JMODGUARD_PUBLISH_SOURCE=refs/tags/v1 prepare
rm -rf "$plan_scratch"
unset JMODGUARD_PUBLICATION_PLAN_FILE

JMODGUARD_PUBLISH_IMAGE_LIB=1 source "$root/scripts/publish-image.sh"
# Untrusted events must stop before reading plans, obtaining credentials or using Git/Docker.
run_git() { fail "unexpected git $*"; }
run_docker() { fail "unexpected docker $*"; }
read_publication_plan() { fail 'untrusted event read publication plan'; }
GITHUB_EVENT_NAME=pull_request prepare
GITHUB_EVENT_NAME=pull_request publish_component bot
with_main
JMODGUARD_PUBLISH_IMAGE_LIB=1 source "$root/scripts/publish-image.sh"
run_git() {
  case "$1" in
    rev-parse) printf '%s\n' "$HEAD_SHA" ;;
    status) printf ' M src/lib.rs\n' ;;
    *) fail "unexpected git $*" ;;
  esac
}
if (publish_component bot) >/dev/null 2>&1; then fail 'dirty checkout must fail'; fi
# Verify the actual runner masking protocol, including control characters.
assert_eq "$(redact_tokens 'a%b' $'c\nd')" $'::add-mask::a%25b\n::add-mask::c%0Ad'

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
contains_arg "ghcr.io/jatmn/jmodguard-bot:nightly" "${nightly_args[@]}" \
  && fail 'Nightly pointer must move only after verification'

readarray -d '' -t stable_args < <(build_args bot "$HEAD_SHA" v1.2.3 . stable)
contains_arg "ghcr.io/jatmn/jmodguard-bot:latest" "${stable_args[@]}" \
  && fail 'stable must not move latest before verification'
contains_arg "ghcr.io/jatmn/jmodguard-bot:v1.2.3" "${stable_args[@]}" \
  || fail 'stable must tag the release'
contains_arg "ghcr.io/jatmn/jmodguard-bot:stable" "${stable_args[@]}" \
  && fail 'Stable pointer must move only after verification'

# Every component must describe the core schema in the selected build source.
# Migration 0016 is already active; advertising 15 admitted a bot-only migration
# and then stranded the old API behind the same compatibility check.
for component in bot api webui; do
  readarray -d '' -t schema_args < <(build_args "$component" "$HEAD_SHA" "" .)
  for field in min-schema max-schema result-schema; do
    contains_arg "io.jmodguard.${field}=20" "${schema_args[@]}" \
      || fail "${component} must advertise current schema20 (${field})"
  done
done

# Migration origins are distinct from exact application runtime ranges.
for field in update-control=3 upgrade-min-core=15 upgrade-min-guild=1 guild-schema=16; do
  contains_arg "io.jmodguard.${field}" "${schema_args[@]}" || fail "missing coordinated metadata ${field}"
done

# Capability labels come from the selected source, independently of SHA.
readarray -d '' -t launcher_args < <(build_args bot "$HEAD_SHA" "" .)
for field in launcher-revision=17 min-launcher-revision=7; do
  contains_arg "io.jmodguard.${field}" "${launcher_args[@]}" || fail "missing launcher requirement ${field}"
done
launcher_source="$(mktemp -d)"
mkdir -p "$launcher_source/src/db" "$launcher_source/migrations/guild"
cp src/db.rs "$launcher_source/src/db.rs"
cp migrations/guild/current_version.rs "$launcher_source/migrations/guild/current_version.rs"
sed -e 's/^pub const LAUNCHER_REVISION: u32 = 17;$/pub const LAUNCHER_REVISION: u32 = 18;/' \
    -e 's/^pub const REQUIRED_LAUNCHER_REVISION: u32 = 7;$/pub const REQUIRED_LAUNCHER_REVISION: u32 = 8;/' \
    src/db/update_control.rs > "$launcher_source/src/db/update_control.rs"
readarray -d '' -t launcher_args < <(build_args api "$HEAD_SHA" "" "$launcher_source")
for field in launcher-revision=18 min-launcher-revision=8; do
  contains_arg "io.jmodguard.${field}" "${launcher_args[@]}" || fail "selected source lost ${field}"
done
rm -rf "$launcher_source"

# Publishing an older source must keep its schema, not the publisher's schema.
schema_source="$(mktemp -d)"
mkdir -p "$schema_source/src"
sed -e '/include_str!("..\/migrations\/001[6789]_/d' -e '/include_str!("..\/migrations\/0020_/d' src/db.rs >"$schema_source/src/db.rs"
readarray -d '' -t old_schema_args < <(build_args bot "$BASE_SHA" "" "$schema_source")
contains_arg "io.jmodguard.update-control=2" "${old_schema_args[@]}" && fail 'historical sources cannot claim coordinated-update support'

for field in min-schema max-schema result-schema; do
  contains_arg "io.jmodguard.${field}=15" "${old_schema_args[@]}" \
    || fail "historical source must retain schema15 (${field})"
done
# The pre-fix schema16 source had literal schema15 launcher observations.
# Republishing it may produce a dashboard image, never an eligible launcher.
sed -e '/include_str!("..\/migrations\/001[789]_/d' -e '/include_str!("..\/migrations\/0020_/d' src/db.rs >"$schema_source/src/db.rs"
mkdir -p "$schema_source/crates/updater/src"
printf 'min_schema: 15,\nmax_schema: 15,\n' >"$schema_source/crates/updater/src/main.rs"
for component in bot api; do
  readarray -d '' -t broken_launcher_args < <(build_args "$component" "$BASE_SHA" "" "$schema_source")
  contains_arg "io.jmodguard.result-schema=16" "${broken_launcher_args[@]}" \
    || fail 'historical schema16 labels must remain truthful'
  contains_arg "io.jmodguard.launcher-protocol=1" "${broken_launcher_args[@]}" \
    && fail 'historical schema15-reporting launcher must not be installable on schema16'
done
printf 'invalid source\n' >"$schema_source/src/db.rs"
if (build_args bot "$HEAD_SHA" "" "$schema_source") >/dev/null 2>&1; then
  fail 'unknown schema must not produce installable image labels'
fi
rm -rf "$schema_source"

reload_publisher() {
  # shellcheck disable=SC1091
  JMODGUARD_PUBLISH_IMAGE_LIB=1 source "$root/scripts/publish-image.sh"
}

plan_json() {
  local mode="${1:-main}"
  local ref="${2:-}"
  local commit="${3:-$HEAD_SHA}"
  local sha="${4:-$HEAD_SHA}"
  local tag="${5:-}"
  MODE="$mode" REF="$ref" COMMIT="$commit" SHA="$sha" TAG="$tag" node --input-type=commonjs - <<'JS'
process.stdout.write(JSON.stringify({
  mode: process.env.MODE,
  ref: process.env.REF,
  commit: process.env.COMMIT,
  sha: process.env.SHA,
  tag: process.env.TAG,
}));
JS
}

CALLS=()
LOGIN_INPUT=""
REDACTOR_INPUT=""
BUILD_LINE=""
GITHUB_TOKEN_SEEN=""
FAIL_BUILD=0
FAIL_VERIFY=0
FAIL_SIGN=0
PUBLISH_SHA="$HEAD_SHA"
SIGNING_KEY_VALUE=fixture-signing-key
SIGNING_PASSWORD_VALUE=fixture-signing-password
SOURCE_LAYOUT=current
TRACE_FILE=""
PUBLISH_TEST_TRACE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/publish-traces-XXXXXX")"

mock_publish_helpers() {
  CALLS=()
  LOGIN_INPUT=""
  REDACTOR_INPUT=""
  BUILD_LINE=""
  GITHUB_TOKEN_SEEN=""
  TRACE_FILE="$(mktemp "${PUBLISH_TEST_TRACE_DIR}/publish-trace-XXXXXX")"

  verify_published_image() {
    printf 'verify %s %s %s\n' "$1" "$2" "${4:-current}" >>"$TRACE_FILE"
    [[ "$FAIL_VERIFY" == 0 ]] || publish_fail 'effective image verification failed'
  }

  # The browser bundle itself is tested by package-webui-test.mjs. Here prove
  # publication installs/builds in the selected source worktree before packaging.
  npm() {
    printf 'npm %s\n' "$*" >>"$TRACE_FILE"
    if [[ "$1" == run && "$2" == build ]]; then
      local context="${4%/tools/webui}"
      mkdir -p "$context/dist"
      cp -R "$root/dist/frontend" "$context/dist/frontend"
    fi
  }

  run_git() {
    printf 'git %s\n' "$*" >>"$TRACE_FILE"
    case "$1" in
      rev-parse) printf '%s\n' "$HEAD_SHA" ;;
      status) printf '' ;;
      worktree)
        if [[ "$2" == add ]]; then
          mkdir -p "$4"
          cp "$root/Dockerfile.bot" "$root/Dockerfile.api" "$4/"
          if [[ "$SOURCE_LAYOUT" == current ]]; then
            mkdir -p "$4/scripts" "$4/crates/updater/src" "$4/src"
            cp "$root/src/db.rs" "$4/src/"
            cp "$root/scripts/bootstrap-container.sh" "$root/scripts/package-webui.mjs" "$4/scripts/"
            cp "$root/crates/updater/src/main.rs" "$4/crates/updater/src/"
            cp "$root/Dockerfile.webui" "$4/"
            mkdir -p "$4/tools/webui"
            cp "$root/tools/webui/package-lock.json" "$4/tools/webui/"
          elif [[ "$SOURCE_LAYOUT" == legacy ]]; then
            printf 'FROM scratch\nCOPY role /usr/local/bin/jmodguard-bot\n' >"$4/Dockerfile.bot"
            printf 'FROM scratch\nCOPY role /usr/local/bin/jmodguard-api\n' >"$4/Dockerfile.api"
          else
            fail "unknown source layout: $SOURCE_LAYOUT"
          fi
        elif [[ "$2" == remove ]]; then
          rm -rf "$4"
        fi
        ;;
      *) fail "unexpected git $*" ;;
    esac
  }
  run_docker() {
    printf 'docker %s\n' "$*" >>"$TRACE_FILE"
    if [[ "$1" == login ]]; then
      cat >"${TRACE_FILE}.login"
      mkdir -p "${DOCKER_CONFIG:-/tmp}"
      return 0
    fi
    if [[ "$1" == buildx ]]; then
      if [[ "$2" == imagetools ]]; then
        if [[ "$*" == *'{{json .Image}}'* ]]; then
          printf '%s' '{"config":{"Labels":{"io.jmodguard.published-at":"2026-10-27T12:00:00Z"}}}'
          return 0
        fi
        printf '{"digest":"sha256:%064d","size":10}\n' 0
        return 0
      fi
      printf '%s\n' "$*" >"${TRACE_FILE}.build"
      printf '%s' "${GITHUB_TOKEN:-}" >"${TRACE_FILE}.gtoken"
      if [[ "$FAIL_BUILD" == 1 ]]; then
        publish_fail 'docker failed: buildx build'
      fi
      return 0
    fi
    return 0
  }
  read_publication_plan() {
    printf 'plan get\n' >>"$TRACE_FILE"
    printf '%s\n' "$PLAN_JSON"
  }
  redact_tokens() {
    printf 'mask tokens\n' >>"$TRACE_FILE"
    GHCR_TOKEN="$1" SOURCE_TOKEN="$2" SIGNING_KEY="${3:-}" SIGNING_PASSWORD="${4:-}" \
      node -e 'process.stdout.write(JSON.stringify({GHCR_TOKEN:process.env.GHCR_TOKEN,GITHUB_TOKEN:process.env.SOURCE_TOKEN,SIGNING_KEY:process.env.SIGNING_KEY,SIGNING_PASSWORD:process.env.SIGNING_PASSWORD}))' >"${TRACE_FILE}.redactor"
  }
  sign_published_digest() {
    printf 'sign %s %s %s\n' "$1" "$2" "$3" >>"$TRACE_FILE"
    require_signing_secrets
    image_published_at "${2}@${3}" >/dev/null \
      || publish_fail 'published image lacks io.jmodguard.published-at'
    [[ "$FAIL_SIGN" == 0 ]] || publish_fail 'cosign failed: sign'
  }
}

load_trace() {
  CALLS=()
  if [[ -n "$TRACE_FILE" && -f "$TRACE_FILE" ]]; then
    mapfile -t CALLS <"$TRACE_FILE"
  fi
  LOGIN_INPUT=""
  REDACTOR_INPUT=""
  BUILD_LINE=""
  GITHUB_TOKEN_SEEN=""
  [[ -f "${TRACE_FILE}.login" ]] && LOGIN_INPUT="$(cat "${TRACE_FILE}.login")"
  [[ -f "${TRACE_FILE}.redactor" ]] && REDACTOR_INPUT="$(cat "${TRACE_FILE}.redactor")"
  [[ -f "${TRACE_FILE}.build" ]] && BUILD_LINE="$(cat "${TRACE_FILE}.build")"
  [[ -f "${TRACE_FILE}.gtoken" ]] && GITHUB_TOKEN_SEEN="$(cat "${TRACE_FILE}.gtoken")"
  return 0
}

exercise_publish() {
  local changed="$1"
  FAIL_BUILD="${2:-0}"
  FAIL_VERIFY="${7:-0}"
  SOURCE_LAYOUT="${8:-current}"
  local mode="${3:-main}"
  local source_token="${4:-}"
  local plan="${5:-}"
  local component="${6:-api}"
  local ref="" sha="$HEAD_SHA"
  with_main
  GHCR_USERNAME=publisher
  GHCR_TOKEN=test-secret
  GITHUB_TOKEN="$source_token"
  [[ -n "${JMODGUARD_UPDATE_SIGNING_KEY+set}" ]] || JMODGUARD_UPDATE_SIGNING_KEY="$SIGNING_KEY_VALUE"
  [[ -n "${JMODGUARD_UPDATE_SIGNING_PASSWORD+set}" ]] || JMODGUARD_UPDATE_SIGNING_PASSWORD="$SIGNING_PASSWORD_VALUE"
  export GHCR_USERNAME GHCR_TOKEN GITHUB_TOKEN JMODGUARD_UPDATE_SIGNING_KEY JMODGUARD_UPDATE_SIGNING_PASSWORD
  if [[ "$mode" == manual ]]; then
    ref="${9:-refs/tags/v1}"
    sha="$BASE_SHA"
    GITHUB_EVENT_NAME=workflow_dispatch
    JMODGUARD_PUBLISH_SOURCE="$ref"
    export GITHUB_EVENT_NAME JMODGUARD_PUBLISH_SOURCE
  elif [[ "$mode" == release ]]; then
    ref=refs/tags/v1.2.3
    sha="$BASE_SHA"
    JMODGUARD_RELEASE_TAG=v1.2.3
    JMODGUARD_RELEASE_SHA="$sha"
  fi
  if [[ -n "$plan" ]]; then
    PLAN_JSON="$plan"
  else
    PLAN_JSON="$(plan_json "$mode" "$ref" "$HEAD_SHA" "$sha" "")"
  fi
  export PLAN_JSON
  PUBLISH_SHA="$sha"
  local scratch saved_tmp
  saved_tmp="${TMPDIR:-/tmp}"
  scratch="$(mktemp -d "${saved_tmp}/publish-exercise-XXXXXX")"
  TMPDIR="$scratch"
  export TMPDIR
  reload_publisher
  mock_publish_helpers
  changed_since_publication() {
    [[ "$changed" == 1 ]]
  }
  resolve_source() {
    [[ "$mode" == manual || "$mode" == release ]] || fail 'only selected-source publication fetches a pinned SHA'
    assert_eq "$1" manual
    assert_eq "$2" "$BASE_SHA"
    assert_eq "$3" "$HEAD_SHA"
    printf '%s\t\n' "$BASE_SHA"
  }
  if [[ "$FAIL_BUILD" == 1 || "$FAIL_VERIFY" == 1 ]]; then
    if ( publish_component "$component" ) >/dev/null 2>&1; then
      fail 'failed build must not succeed'
    fi
  else
    publish_component "$component"
  fi
  load_trace
  [[ -z "$(ls -A "$scratch" 2>/dev/null || true)" ]] || fail "scratch leftover: $(ls -A "$scratch")"
  rm -rf "$scratch"
  TMPDIR="$saved_tmp"
  export TMPDIR
  SOURCE_LAYOUT=current
}

reload_publisher
exercise_publish 0
printf '%s\n' "${CALLS[@]}" | grep -q 'docker buildx build' && fail 'unchanged main must not build'
printf '%s\n' "${CALLS[@]}" | grep -q 'git worktree' && fail 'unchanged main must not use a worktree'
printf '%s\n' "${CALLS[@]}" | grep -q 'secret get' && fail 'unchanged main must not read secrets'
printf '%s\n' "${CALLS[@]}" | grep -q 'docker login' && fail 'unchanged main must not log in'
printf '%s\n' "${CALLS[@]}" | grep -q 'mask tokens' && fail 'unchanged main must not redact'

with_main
unset GHCR_USERNAME GHCR_TOKEN || true
GITHUB_TOKEN=""
export GITHUB_TOKEN
PLAN_JSON="$(plan_json main "" "$HEAD_SHA" "$HEAD_SHA" "")"
export PLAN_JSON
reload_publisher
mock_publish_helpers
changed_since_publication() { return 1; }
publish_component api
load_trace
printf '%s\n' "${CALLS[@]}" | grep -q 'plan get' \
  || fail 'skip without credentials must still read the plan'
printf '%s\n' "${CALLS[@]}" | grep -qE 'docker |secret get|mask tokens' \
  && fail "skip without credentials must not touch docker or secrets: ${CALLS[*]}"

exercise_publish 1
printf '%s\n' "$BUILD_LINE" | grep -q -- '--provenance=false' || fail 'build must disable provenance'
printf '%s\n' "$BUILD_LINE" | grep -q -- '--sbom=false' || fail 'build must disable sbom'
printf '%s\n' "$BUILD_LINE" | grep -q 'linux/amd64' || fail 'build must be linux/amd64'
printf '%s\n' "$BUILD_LINE" | grep -q "ghcr.io/jatmn/jmodguard-api:${HEAD_SHA}" || fail 'build must tag the SHA'
[[ "$BUILD_LINE" != *'--tag ghcr.io/jatmn/jmodguard-api:latest'* ]] \
  || fail 'main build must not move latest before verification'
printf '%s\n' "${CALLS[@]}" | grep -q 'docker buildx imagetools create --prefer-index=false --tag ghcr.io/jatmn/jmodguard-api:latest' \
  || fail 'verified main image must move latest by digest'
verify_line="$(printf '%s\n' "${CALLS[@]}" | grep -n '^verify api ' | head -1 | cut -d: -f1)"
sign_line="$(printf '%s\n' "${CALLS[@]}" | grep -n '^sign api ' | head -1 | cut -d: -f1)"
promote_line="$(printf '%s\n' "${CALLS[@]}" | grep -n 'docker buildx imagetools create --prefer-index=false --tag ghcr.io/jatmn/jmodguard-api:latest' | head -1 | cut -d: -f1)"
[[ -n "$verify_line" && -n "$sign_line" && -n "$promote_line" && "$verify_line" -lt "$sign_line" && "$sign_line" -lt "$promote_line" ]] \
  || fail 'latest must move only after verification and signing'
[[ "$GITHUB_TOKEN_SEEN" == test-secret ]] || fail "build secret must fall back to GHCR token, got '$GITHUB_TOKEN_SEEN'"
printf '%s\n' "${CALLS[@]}" | grep -q 'git worktree remove' || fail 'publish must remove the worktree'
printf '%s\n' "${CALLS[@]}" | grep -q 'test-secret' && fail 'secret must not appear on argv'
[[ "$LOGIN_INPUT" == test-secret ]] || fail 'login must read the token from stdin'
[[ "$(REDACTOR_INPUT="$REDACTOR_INPUT" node --input-type=commonjs - <<'JS'
const data = JSON.parse(process.env.REDACTOR_INPUT);
process.stdout.write(data.GITHUB_TOKEN || "");
JS
)" == test-secret ]] || fail 'redactor must receive the source token'
[[ "$(REDACTOR_INPUT="$REDACTOR_INPUT" node --input-type=commonjs - <<'JS'
const data = JSON.parse(process.env.REDACTOR_INPUT);
process.stdout.write(data.SIGNING_KEY || "");
JS
)" == "$SIGNING_KEY_VALUE" ]] || fail 'redactor must receive the signing key'
[[ "$(REDACTOR_INPUT="$REDACTOR_INPUT" node --input-type=commonjs - <<'JS'
const data = JSON.parse(process.env.REDACTOR_INPUT);
process.stdout.write(data.SIGNING_PASSWORD || "");
JS
)" == "$SIGNING_PASSWORD_VALUE" ]] || fail 'redactor must receive the signing password'

exercise_publish 1 1
printf '%s\n' "${CALLS[@]}" | grep -q 'git worktree remove' || fail 'failed build must still remove the worktree'

for component in bot api webui; do
  exercise_publish 1 0 main '' '' "$component" 1
  [[ "$BUILD_LINE" != *"--tag ghcr.io/jatmn/jmodguard-${component}:latest"* ]] \
    || fail "$component failed verification moved latest during build"
  printf '%s\n' "${CALLS[@]}" | grep -q -- "--tag ghcr.io/jatmn/jmodguard-${component}:latest" \
    && fail "$component failed verification promoted latest"
done

stable_plan="$(plan_json manual refs/tags/v1 "$HEAD_SHA" "$BASE_SHA" v1.2.3)"
exercise_publish 1 0 manual '' "$stable_plan" bot 1
printf '%s\n' "${CALLS[@]}" | grep -q -- '--tag ghcr.io/jatmn/jmodguard-bot:stable' \
  && fail 'failed Stable verification promoted stable'
printf '%s\n' "${CALLS[@]}" | grep -q -- '--tag ghcr.io/jatmn/jmodguard-bot:latest' \
  && fail 'failed Stable verification promoted latest'
exercise_publish 1 0 manual '' "$stable_plan" bot
verify_line="$(printf '%s\n' "${CALLS[@]}" | grep -n '^verify bot ' | head -1 | cut -d: -f1)"
sign_line="$(printf '%s\n' "${CALLS[@]}" | grep -n '^sign bot ' | head -1 | cut -d: -f1)"
latest_line="$(printf '%s\n' "${CALLS[@]}" | grep -n -- '--tag ghcr.io/jatmn/jmodguard-bot:latest' | head -1 | cut -d: -f1)"
stable_line="$(printf '%s\n' "${CALLS[@]}" | grep -n -- '--tag ghcr.io/jatmn/jmodguard-bot:stable' | head -1 | cut -d: -f1)"
[[ -n "$verify_line" && -n "$sign_line" && -n "$latest_line" && -n "$stable_line" && "$verify_line" -lt "$sign_line" && "$sign_line" -lt "$latest_line" && "$sign_line" -lt "$stable_line" ]] \
  || fail 'Stable publication must sign before promoting moving pointers'

# Automatic releases build R, not workflow commit H, for every eligible package.
release_plan="$(plan_json release refs/tags/v1.2.3 "$HEAD_SHA" "$BASE_SHA" v1.2.3)"
for component in bot api webui; do
  exercise_publish 0 0 release '' "$release_plan" "$component"
  [[ "$BUILD_LINE" == *"--tag ghcr.io/jatmn/jmodguard-${component}:${BASE_SHA}"* ]] \
    || fail "$component release must build the selected release SHA"
  [[ "$BUILD_LINE" == *"--tag ghcr.io/jatmn/jmodguard-${component}:v1.2.3"* ]] \
    || fail "$component release must retain its version tag"
  verify_line="$(printf '%s\n' "${CALLS[@]}" | grep -n "^verify $component " | cut -d: -f1)"
  sign_line="$(printf '%s\n' "${CALLS[@]}" | grep -n "^sign $component " | cut -d: -f1)"
  stable_line="$(printf '%s\n' "${CALLS[@]}" | grep -n -- "--tag ghcr.io/jatmn/jmodguard-${component}:stable" | cut -d: -f1)"
  [[ -n "$verify_line" && -n "$sign_line" && -n "$stable_line" && "$verify_line" -lt "$sign_line" && "$sign_line" -lt "$stable_line" ]] \
    || fail "$component release must sign before promoting Stable"

  # Reject mismatched source, workflow identity and version before registry login.
  for bad_plan in \
    "$(plan_json release refs/tags/v1.2.3 "$HEAD_SHA" "$HEAD_SHA" v1.2.3)" \
    "$(plan_json release refs/tags/v1.2.3 "$BASE_SHA" "$BASE_SHA" v1.2.3)" \
    "$(plan_json release refs/tags/v1.2.3 "$HEAD_SHA" "$BASE_SHA" v9.9.9)"; do
    : >"$TRACE_FILE"
    if (PLAN_JSON="$bad_plan" publish_component "$component") >/dev/null 2>&1; then
      fail "$component accepted a mismatched release plan"
    fi
    grep -q '^docker ' "$TRACE_FILE" && fail 'invalid release plan reached Docker'
  done
done

reload_publisher
saved_publish_tmp="${TMPDIR:-/tmp}"
publish_scratch_parent="$(mktemp -d "${saved_publish_tmp}/publish-scratch-parent-XXXXXX")"
TMPDIR="$publish_scratch_parent"
export TMPDIR
with_main
GHCR_USERNAME=fixture
GHCR_TOKEN=fixture-token
GITHUB_TOKEN=""
PLAN_JSON="$(plan_json main "" "$HEAD_SHA" "$HEAD_SHA" "")"
export GHCR_USERNAME GHCR_TOKEN GITHUB_TOKEN PLAN_JSON
run_git() {
  case "$1" in
    rev-parse) printf '%s\n' "$HEAD_SHA" ;;
    status) printf '' ;;
    worktree)
      if [[ "$2" == add ]]; then
        publish_fail 'worktree add failed'
      fi
      ;;
    *) fail "unexpected git $*" ;;
  esac
}
run_docker() {
  if [[ "$1" == login ]]; then
    cat >/dev/null
    return 0
  fi
  fail "unexpected docker $*"
}
read_publication_plan() { printf '%s\n' "$PLAN_JSON"; }
redact_tokens() { :; }
changed_since_publication() { return 0; }
if ( publish_component api ) >/dev/null 2>&1; then
  fail 'worktree failure must abort publish'
fi
if compgen -G "${publish_scratch_parent}/publish-*" >/dev/null; then
  fail "publish scratch must be removed after worktree failure: $(ls -A "$publish_scratch_parent")"
fi
rm -rf "$publish_scratch_parent"
TMPDIR="$saved_publish_tmp"
export TMPDIR

reload_publisher
digest="$(printf 'a%.0s' {1..64})"
run_docker() { printf '%s' "{\"digest\":\"sha256:${digest}\",\"size\":0}"; }
if ( inspect_digest "ghcr.io/jatmn/jmodguard-bot:${HEAD_SHA}" ) >/dev/null 2>&1; then
  fail 'inspect_digest must reject non-positive size'
fi
run_docker() { printf '%s' "{\"digest\":\"sha256:${digest}\",\"size\":9}"; }
got="$(inspect_digest "ghcr.io/jatmn/jmodguard-bot:${HEAD_SHA}")"
assert_eq "$got" "$(printf 'sha256:%s\t%s' "$digest" 9)"

for pair in 'read-only-secret:read-only-secret' '$GITHUB_TOKEN:test-secret'; do
  token="${pair%%:*}"
  expected="${pair#*:}"
  exercise_publish 1 0 main "$token"
  [[ "$GITHUB_TOKEN_SEEN" == "$expected" ]] || fail "source token '$token' must yield '$expected'"
  got="$(REDACTOR_INPUT="$REDACTOR_INPUT" node --input-type=commonjs - <<'JS'
const data = JSON.parse(process.env.REDACTOR_INPUT);
process.stdout.write(data.GITHUB_TOKEN || "");
JS
)"
  [[ "$got" == "$expected" ]] || fail "redactor token '$got' must be '$expected'"
done

exercise_publish 0 0 manual
printf '%s\n' "$BUILD_LINE" | grep -q "ghcr.io/jatmn/jmodguard-api:${BASE_SHA}" \
  || fail 'manual publication must build the planned SHA even when compare would skip'

for component in bot api webui; do
  exercise_publish 0 0 manual '' '' "$component" 0 current "$BASE_SHA"
  printf '%s\n' "$BUILD_LINE" | grep -q "ghcr.io/jatmn/jmodguard-${component}:${BASE_SHA}" \
    || fail "manual SHA publication must still build ${component}"
  printf '%s\n' "${CALLS[@]}" | grep -q "^verify ${component} " \
    || fail "manual SHA publication must verify ${component} before promotion"
done

# A historical manual source predates the WebUI package and service launcher.
# Its bot/API images remain publishable for dashboard rollback, while their
# labels cannot claim owner-updater compatibility.
for component in bot api; do
  exercise_publish 0 0 manual '' '' "$component" 0 legacy
  printf '%s\n' "${CALLS[@]}" | grep -q "^verify ${component} .* legacy$" \
    || fail "historical ${component} must receive the legacy final-image check"
  [[ "$BUILD_LINE" != *'io.jmodguard.launcher-protocol'* ]] \
    || fail "historical ${component} must not claim updater compatibility"
  printf '%s\n' "$BUILD_LINE" | grep -q 'io.jmodguard.published-at=' \
    || fail "historical ${component} must label a publication timestamp for signing"
  printf '%s\n' "${CALLS[@]}" | grep -q "^sign ${component} " \
    || fail "historical ${component} must sign before promoting latest"
  printf '%s\n' "${CALLS[@]}" | grep -q -- "--tag ghcr.io/jatmn/jmodguard-${component}:latest" \
    || fail "verified historical ${component} may still move latest for dashboard rollback"
done
exercise_publish 0 0 manual '' '' webui 0 legacy
[[ -z "$BUILD_LINE" ]] || fail 'historical source must skip the nonexistent WebUI package'
printf '%s\n' "${CALLS[@]}" | grep -q 'git worktree remove' \
  || fail 'historical WebUI skip must remove the selected source worktree'

reload_publisher
run_docker() {
  case "$1" in
    pull|rm) return 0 ;;
    create)
      [[ "$2" == --entrypoint && "$3" == /usr/local/bin/jmodguard-bot ]] \
        || fail 'historical verifier must inspect the direct-entrypoint image'
      printf '%s\n' legacy-image
      ;;
    cp)
      [[ "$2" == 'legacy-image:/usr/local/bin/jmodguard-bot' ]] \
        || fail "historical verifier requested an updater-only file: $2"
      printf 'historical executable\n' >"$3"
      ;;
    *) fail "unexpected historical verifier command: $*" ;;
  esac
}
verify_published_image bot historical-image . legacy

partial_source="$(mktemp -d "${TMPDIR:-/tmp}/partial-source-XXXXXX")"
mkdir -p "$partial_source/scripts"
cp "$root/Dockerfile.bot" "$partial_source/Dockerfile.bot"
cp "$root/scripts/bootstrap-container.sh" "$partial_source/scripts/"
if ( source_image_layout bot "$partial_source" ) >/dev/null 2>&1; then
  fail 'partial service layout must not downgrade to legacy verification'
fi
rm -rf "$partial_source"

reload_publisher
run_git() {
  case "$1" in
    rev-parse) printf '%s\n' "$HEAD_SHA" ;;
    status) printf '' ;;
    *) fail "unexpected git $*" ;;
  esac
}
run_docker() { fail 'invalid plan must not touch docker'; }
with_main
for plan in '[]' '{}' "$(plan_json main "" "$HEAD_SHA" "$BASE_SHA" "")"; do
  PLAN_JSON="$plan"
  read_publication_plan() { printf '%s\n' "$PLAN_JSON"; }
  if ( read_plan main "" "$HEAD_SHA" ) >/dev/null 2>&1; then
    fail "invalid plan must be rejected: $plan"
  fi
done

reload_publisher
digest="$(printf 'a%.0s' {1..64})"
run_docker() { printf '%s' "{\"digest\":\"sha256:${digest}\",\"size\":9}"; }
got="$(inspect_digest "ghcr.io/jatmn/jmodguard-bot:${HEAD_SHA}")"
assert_eq "$got" "$(printf 'sha256:%s\t%s' "$digest" 9)"

scratch="$(mktemp -d "${TMPDIR:-/tmp}/publish-image-git-XXXXXX")"
cleanup_git() { rm -rf "$scratch"; }
trap cleanup_git EXIT
git init -b main "$scratch" >/dev/null
git -C "$scratch" config user.name Fixture
git -C "$scratch" config user.email fixture@example.invalid
mkdir -p "$scratch/src"
echo old >"$scratch/src/lib.rs"
printf 'FROM scratch\nCOPY role /usr/local/bin/jmodguard-bot\n' >"$scratch/Dockerfile.bot"
git -C "$scratch" add .
git -C "$scratch" commit -m base >/dev/null
base="$(git -C "$scratch" rev-parse HEAD)"
git -C "$scratch" tag -a 'release/v1+meta' -m release
git -C "$scratch" remote add origin "$scratch"
fixture_git_root="$scratch"

pushd "$scratch" >/dev/null
reload_publisher
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

oci_config() {
  local revision="$1"
  REVISION="$revision" node --input-type=commonjs - <<'JS'
process.stdout.write(JSON.stringify({
  config: {
    Labels: {
      "org.opencontainers.image.revision": process.env.REVISION,
      "org.opencontainers.image.source": "https://github.com/jatmn/jModGuard",
    },
  },
}));
JS
}

registry_inspect() {
  local ref="$1"
  shift
  if [[ "$ref" == image:latest && "$*" == *--format* ]]; then
    printf '%s' "$(oci_config "$base")"
    return 0
  fi
  if [[ "$*" == *--raw* ]]; then
    [[ "$ref" == image:latest || "$ref" == "image:${base}" ]] || return 1
    printf '%s' '{"schemaVersion":2}'
    return 0
  fi
  return 1
}
if changed_since_publication bot "$head" image; then
  fail 'docs-only change must not rebuild bot'
fi
echo changed >src/lib.rs
git add .
git commit -m src >/dev/null
head="$(git rev-parse HEAD)"
mkdir -p docs
echo extra >docs/new.md
git add .
git commit -m docs >/dev/null
head="$(git rev-parse HEAD)"
if ! changed_since_publication bot "$head" image; then
  fail 'src change since last image must rebuild'
fi

# Actual Git diffs, with a complete matching registry publication, distinguish
# component inputs from source copied only to satisfy Cargo workspace layout.
while IFS='|' read -r path affected; do
  mkdir -p "$(dirname "$path")"
  echo changed >>"$path"
  git add .
  git commit -m "$path" >/dev/null
  head="$(git rev-parse HEAD)"
  INSPECT_REVISION="$(git rev-parse HEAD~1)"
  registry_inspect() {
    local ref="$1"
    shift
    if [[ "$ref" == image:latest && "$*" == *--format* ]]; then
      oci_config "$INSPECT_REVISION"
    elif [[ "$*" == *--raw* && ( "$ref" == image:latest || "$ref" == "image:${INSPECT_REVISION}" ) ]]; then
      printf '%s' '{"schemaVersion":2}'
    else
      return 1
    fi
  }
  for component in bot api webui; do
    actual=skip
    if changed_since_publication "$component" "$head" image; then actual=build; fi
    expected=skip
    case " $affected " in *" $component "*) expected=build ;; esac
    [[ "$actual" == "$expected" ]] || fail "$component after $path: expected $expected, got $actual"
  done
done <<'CASES'
crates/bot/src/main.rs|bot
crates/api/src/web/api.rs|api
crates/integration-tests/tests/dual_process_smoke.rs|
crates/api/tests/tokenless_startup.rs|
crates/updater/tests/registry.rs|
crates/runtime/src/health.rs|bot api
crates/updater/src/main.rs|bot api
crates/updater/build.rs|bot api
migrations/guild/0001_identity.sql|bot api
migrations/0012_guild_moves.sql|bot api
src/lib.rs|bot api
src/db.rs|bot api webui
vendor/tokio-tungstenite/src/lib.rs|bot api
src/db.rs|bot api webui
crates/runtime/Cargo.toml|bot api
Cargo.toml|bot api
Cargo.lock|bot api
rust-toolchain.toml|bot api
LICENSE.md|bot api webui
Dockerfile.bot|bot
Dockerfile.api|api
Dockerfile.webui|webui
.dockerignore|bot api webui
scripts/bootstrap-container.sh|bot api
frontend/style.css|webui
scripts/package-webui.mjs|webui
tools/webui/package-lock.json|webui
tools/webui/build.mjs|webui
scripts/publish-image.sh|bot api webui
scripts/select-checks.sh|
scripts/publish-image-test.sh|
docs/INSTALL.md|
CASES

reload_publisher
registry_inspect() {
  printf '%s' "$(oci_config "$(printf 'c%.0s' {1..40})")"
}
skip_source_diff() { return 128; }
if ! changed_since_publication bot "$base" image; then
  fail 'unknown git object must rebuild, not skip or abort'
fi

reload_publisher
match_base="$(git rev-parse HEAD)"
echo changed >docs/new.md
git add .
git commit -m docs >/dev/null
head="$(git rev-parse HEAD)"
manifest='{"schemaVersion":2,"config":{"digest":"sha256:fixture"}}'
for case in none:0 different:0 empty:0 match:1; do
  immutable_kind="${case%%:*}"
  expect="${case#*:}"
  registry_inspect() {
    local ref="$1"
    shift
    if [[ "$*" != *--raw* ]]; then
      printf '%s' "$(oci_config "$match_base")"
      return 0
    fi
    if [[ "$ref" == image:latest ]]; then
      printf '%s' "$manifest"
      return 0
    fi
    case "$immutable_kind" in
      none) return 1 ;;
      different) printf '%s' 'different manifest'; return 0 ;;
      empty) printf ''; return 0 ;;
      match) printf '%s' "$manifest"; return 0 ;;
    esac
  }
  rc=0
  if changed_since_publication bot "$head" image; then
    rc=0
  else
    rc=1
  fi
  [[ "$rc" -eq "$expect" ]] || fail "manifest pair $immutable_kind: expected $expect got $rc"
done
newer="$head"

# Partial publication: latest exists, SHA tag missing, must rebuild then skip.
reload_publisher
git checkout --detach "$base" >/dev/null 2>&1 || git checkout "$base" >/dev/null
image='ghcr.io/jatmn/jmodguard-bot'
declare -A REGISTRY=()
EXPORTS=0
PLAN_JSON="$(plan_json main "" "$base" "$base" "")"
export PLAN_JSON
with_main
GITHUB_SHA="$base"
HEAD_SHA="$base"
GHCR_USERNAME=fixture
GHCR_TOKEN=fixture-token
GITHUB_TOKEN=""
export GITHUB_SHA GHCR_USERNAME GHCR_TOKEN GITHUB_TOKEN
registry_inspect() {
  local ref="$1"
  shift
  if [[ -z "${REGISTRY[$ref]:-}" ]]; then
    return 1
  fi
  if [[ "$*" == *--raw* ]]; then
    printf '%s' "${REGISTRY[$ref]}"
    return 0
  fi
  printf '%s' "$(oci_config "$base")"
}
run_git() {
  case "$1" in
    rev-parse) printf '%s\n' "$base" ;;
    status) printf '' ;;
    worktree)
      if [[ "$2" == add ]]; then
        mkdir -p "$4"
        cp "$fixture_git_root/Dockerfile.bot" "$4/Dockerfile.bot"
      elif [[ "$2" == remove ]]; then rm -rf "$4"; fi
      ;;
    *) git "$@" ;;
  esac
}
read_publication_plan() { printf '%s\n' "$PLAN_JSON"; }
redact_tokens() { :; }
run_docker() {
  if [[ "$1" == login ]]; then cat >/dev/null; return 0; fi
  if [[ "$1 $2" == 'buildx imagetools' ]]; then printf '{"digest":"sha256:%064d","size":10}\n' 0; return 0; fi
  if [[ "$1" == buildx ]]; then
    EXPORTS=$((EXPORTS + 1))
    REGISTRY["${image}:latest"]="$manifest"
    if [[ "$EXPORTS" -eq 1 ]]; then
      publish_fail 'docker failed: buildx build'
    fi
    REGISTRY["${image}:${base}"]="$manifest"
    return 0
  fi
  return 0
}
verify_published_image() { :; }
sign_published_digest() { :; }
if ( publish_component bot ) >/dev/null 2>&1; then
  fail 'first partial build must fail'
fi
# Subshell cannot keep REGISTRY updates; replay the first-push side effect.
REGISTRY["${image}:latest"]="$manifest"
EXPORTS=1
[[ -z "${REGISTRY[${image}:${base}]:-}" ]] || fail 'failed first push must not record the SHA tag'
publish_component bot
[[ "${REGISTRY[${image}:${base}]}" == "${REGISTRY[${image}:latest]}" ]] || fail 'retry must repair the SHA tag'
[[ "$EXPORTS" -eq 2 ]] || fail "partial repair expected 2 exports, got $EXPORTS"
publish_component bot
[[ "$EXPORTS" -eq 2 ]] || fail 'matching latest and SHA tags must skip a third build'

# Retried job must republish its planned SHA even if latest now points at a newer commit.
reload_publisher
verify_published_image() { :; }
sign_published_digest() { :; }
EXPORTS=0
PLAN_JSON="$(plan_json main "" "$base" "$base" "")"
export PLAN_JSON
with_main
GITHUB_SHA="$base"
GITHUB_RUN_ATTEMPT=2
HEAD_SHA="$base"
GHCR_USERNAME=fixture
GHCR_TOKEN=fixture-token
GITHUB_TOKEN=""
export GITHUB_SHA GITHUB_RUN_ATTEMPT GHCR_USERNAME GHCR_TOKEN GITHUB_TOKEN
registry_inspect() {
  local ref="$1"
  shift
  case "$ref" in
    "${image}:latest"|"${image}:${newer}")
      if [[ "$*" == *--raw* ]]; then
        printf '%s' '{"schemaVersion":2}'
        return 0
      fi
      printf '%s' "$(oci_config "$newer")"
      return 0
      ;;
    *) return 1 ;;
  esac
}
run_git() {
  case "$1" in
    rev-parse) printf '%s\n' "$base" ;;
    status) printf '' ;;
    worktree)
      if [[ "$2" == add ]]; then
        mkdir -p "$4"
        cp "$fixture_git_root/Dockerfile.bot" "$4/Dockerfile.bot"
      elif [[ "$2" == remove ]]; then rm -rf "$4"; fi
      ;;
    *) git "$@" ;;
  esac
}
read_publication_plan() { printf '%s\n' "$PLAN_JSON"; }
redact_tokens() { :; }
run_docker() {
  if [[ "$1" == login ]]; then cat >/dev/null; return 0; fi
  if [[ "$1 $2" == 'buildx imagetools' ]]; then printf '{"digest":"sha256:%064d","size":10}\n' 0; return 0; fi
  if [[ "$1" == buildx ]]; then
    EXPORTS=$((EXPORTS + 1))
    return 0
  fi
  return 0
}
publish_component bot
[[ "$EXPORTS" -eq 1 ]] || fail "retried job must rebuild, got $EXPORTS exports"

popd >/dev/null

reload_publisher
FAIL_SIGN=0
JMODGUARD_UPDATE_SIGNING_KEY=""
JMODGUARD_UPDATE_SIGNING_PASSWORD="$SIGNING_PASSWORD_VALUE"
export JMODGUARD_UPDATE_SIGNING_KEY JMODGUARD_UPDATE_SIGNING_PASSWORD
if ( exercise_publish 1 ) >/dev/null 2>&1; then
  fail 'missing signing key must fail publication'
fi
load_trace
printf '%s\n' "${CALLS[@]}" | grep -q 'docker buildx imagetools create' \
  && fail 'missing signing key must not promote tags'

reload_publisher
JMODGUARD_UPDATE_SIGNING_KEY="$SIGNING_KEY_VALUE"
JMODGUARD_UPDATE_SIGNING_PASSWORD=""
export JMODGUARD_UPDATE_SIGNING_KEY JMODGUARD_UPDATE_SIGNING_PASSWORD
if ( exercise_publish 1 ) >/dev/null 2>&1; then
  fail 'missing signing password must fail publication'
fi
load_trace
printf '%s\n' "${CALLS[@]}" | grep -q 'docker buildx imagetools create' \
  && fail 'missing signing password must not promote tags'

FAIL_SIGN=1
JMODGUARD_UPDATE_SIGNING_KEY="$SIGNING_KEY_VALUE"
JMODGUARD_UPDATE_SIGNING_PASSWORD="$SIGNING_PASSWORD_VALUE"
export JMODGUARD_UPDATE_SIGNING_KEY JMODGUARD_UPDATE_SIGNING_PASSWORD
if ( exercise_publish 1 ) >/dev/null 2>&1; then
  fail 'failed signing must fail publication'
fi
load_trace
printf '%s\n' "${CALLS[@]}" | grep -q 'docker buildx imagetools create' \
  && fail 'failed signing must not promote tags'
FAIL_SIGN=0
unset JMODGUARD_UPDATE_SIGNING_KEY JMODGUARD_UPDATE_SIGNING_PASSWORD

assert_eq "$(redact_tokens 'a%b' $'c\nd' 'secret-key' 'secret-pass')" \
  $'::add-mask::a%25b\n::add-mask::c%0Ad\n::add-mask::secret-key\n::add-mask::secret-pass'
trace="$(redact_tokens 'a%b' $'c\nd' 'secret-key' 'secret-pass')"
printf '%s' "$trace" | grep -v '^::add-mask::' | grep -Eq 'secret-key|secret-pass' \
  && fail 'redactor output must not echo signing secrets outside masking'

reload_publisher
run_docker() {
  [[ "$*" == *'{{json .Image}}'* ]] \
    || fail 'image_published_at must read Buildx Image JSON'
  printf '%s' '{"config":{"Labels":{"io.jmodguard.published-at":"2026-10-27T17:00:00Z"}}}'
}
got="$(image_published_at "ghcr.io/jatmn/jmodguard-bot@sha256:$(printf 'a%.0s' {1..64})")"
assert_eq "$got" "2026-10-27T17:00:00Z"

reload_publisher
readarray -d '' -t legacy_sign_args < <(build_args bot "$HEAD_SHA" "" . stable legacy)
printf '%s\n' "${legacy_sign_args[@]}" | grep -q 'io.jmodguard.published-at=' \
  || fail 'legacy builds must carry a publication timestamp for signing'
printf '%s\n' "${legacy_sign_args[@]}" | grep -q 'io.jmodguard.launcher-protocol=1' \
  && fail 'legacy builds must not claim updater compatibility'

grep -q 'BEGIN PUBLIC KEY' "$root/deploy/update-signing.pub" \
  || fail 'deploy/update-signing.pub must contain the committed production cosign public key'
grep -q 'MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAENbnym' "$root/deploy/update-signing.pub" \
  && fail 'deploy signing pubkey must not be the offline test fixture'

reload_publisher
COSIGN_TRACE="$PUBLISH_TEST_TRACE_DIR/cosign-commands.log"
: >"$COSIGN_TRACE"
run_cosign() {
  printf 'cosign %s\n' "$*" >>"$COSIGN_TRACE"
  return 0
}
run_docker() {
  if [[ "$1 $2" == 'buildx imagetools' ]]; then
    if [[ "$*" == *'{{json .Image}}'* ]]; then
      printf '%s' '{"config":{"Labels":{"io.jmodguard.published-at":"2026-10-27T17:00:00Z"}}}'
      return 0
    fi
    printf '{"digest":"sha256:%064d","size":10}\n' 1
    return 0
  fi
  return 0
}
JMODGUARD_UPDATE_SIGNING_KEY="$SIGNING_KEY_VALUE"
JMODGUARD_UPDATE_SIGNING_PASSWORD="$SIGNING_PASSWORD_VALUE"
export JMODGUARD_UPDATE_SIGNING_KEY JMODGUARD_UPDATE_SIGNING_PASSWORD
sign_published_digest bot ghcr.io/jatmn/jmodguard-bot "sha256:$(printf 'a%.0s' {1..64})"
grep -q 'cosign sign' "$COSIGN_TRACE" || fail 'production signing must invoke cosign sign'
grep -q 'cosign verify' "$COSIGN_TRACE" || fail 'production signing must invoke cosign verify'
unset JMODGUARD_UPDATE_SIGNING_KEY JMODGUARD_UPDATE_SIGNING_PASSWORD

echo 'publish-image-test: ok'
