#!/usr/bin/env bash
# Offline publication contract tests; never log in, push, or contact GitHub Actions.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

fail() {
  echo "publish-image-test: $*" >&2
  exit 1
}

ensure_cosign() {
  if command -v cosign >/dev/null; then
    return 0
  fi
  local cache="${TMPDIR:-/tmp}/jmodguard-cosign"
  mkdir -p "$cache"
  if [[ ! -x "$cache/cosign" ]]; then
    local version='v2.4.1'
    local url="https://github.com/sigstore/cosign/releases/download/${version}/cosign-linux-amd64"
    curl -fsSL "$url" -o "$cache/cosign" || fail "could not download cosign ${version}"
    chmod 700 "$cache/cosign"
  fi
  PATH="$cache:$PATH"
  export PATH
  command -v cosign >/dev/null || fail 'cosign is required for publish-image-test.sh'
}
ensure_cosign
TEST_SIGNING_DIR="$(mktemp -d "${TMPDIR:-/tmp}/publish-image-signing-XXXXXX")"
TEST_SIGNING_PASSWORD="$(head -c 32 /dev/urandom | base64 | tr -d '\n/=')"
cleanup_test_signing() {
  rm -rf "$TEST_SIGNING_DIR"
}
TEST_SIGNING_TRAP="$(trap -p EXIT || true)"
trap 'cleanup_test_signing' EXIT
COSIGN_PASSWORD="$TEST_SIGNING_PASSWORD" cosign generate-key-pair \
  --output-key-prefix "$TEST_SIGNING_DIR/update-signing"
SIGNING_KEY_VALUE="$(<"$TEST_SIGNING_DIR/update-signing.key")"
SIGNING_PASSWORD_VALUE="$TEST_SIGNING_PASSWORD"
TEST_SIGNING_PUBLIC_KEY="$TEST_SIGNING_DIR/update-signing.pub"
MOCK_BUILD_DIGEST="sha256:$(printf '0%.0s' {1..64})"

staging_ref() {
  publish_staging_ref "ghcr.io/jatmn/jmodguard-${1}" "${2:-$HEAD_SHA}"
}

assert_signing_secrets_not_in_environ() {
  signing_secrets_in_environ && fail 'signing secrets must not remain in the process environment'
  local line
  while IFS= read -r line; do
    [[ "$line" == JMODGUARD_UPDATE_SIGNING_KEY=* || "$line" == JMODGUARD_UPDATE_SIGNING_PASSWORD=* ]] \
      && fail "signing secret leaked in environ: ${line%%=*}"
  done <"/proc/self/environ"
}

# shellcheck disable=SC1091
JMODGUARD_PUBLISH_IMAGE_LIB=1 source "$root/scripts/publish-image.sh"
SIGNING_PUBLIC_KEY="$TEST_SIGNING_PUBLIC_KEY"
export SIGNING_PUBLIC_KEY

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
( GITHUB_EVENT_NAME=pull_request publish_component bot ) >/dev/null 2>&1 || true
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

readarray -d '' -t nightly_args < <(build_args bot "$HEAD_SHA" "nightly-20260915-${HEAD_SHA:0:12}" . nightly current "$(staging_ref bot)")
contains_arg "ghcr.io/jatmn/jmodguard-bot:latest" "${nightly_args[@]}" \
  && fail 'nightly must not move latest'
contains_arg "$(staging_ref bot)" "${nightly_args[@]}" \
  || fail 'nightly must tag the staging reference'
contains_arg "ghcr.io/jatmn/jmodguard-bot:nightly-20260915-${HEAD_SHA:0:12}" "${nightly_args[@]}" \
  && fail 'nightly must not push the unique label before verification and signing'
contains_arg "ghcr.io/jatmn/jmodguard-bot:nightly" "${nightly_args[@]}" \
  && fail 'Nightly pointer must move only after verification'

readarray -d '' -t stable_args < <(build_args bot "$HEAD_SHA" v1.2.3 . stable current "$(staging_ref bot)")
contains_arg "ghcr.io/jatmn/jmodguard-bot:latest" "${stable_args[@]}" \
  && fail 'stable must not move latest before verification'
contains_arg "ghcr.io/jatmn/jmodguard-bot:v1.2.3" "${stable_args[@]}" \
  && fail 'stable must not push the version tag before verification and signing'
contains_arg "ghcr.io/jatmn/jmodguard-bot:stable" "${stable_args[@]}" \
  && fail 'Stable pointer must move only after verification'

# Every component must describe the core schema in the selected build source.
# Migration 0016 is already active; advertising 15 admitted a bot-only migration
# and then stranded the old API behind the same compatibility check.
for component in bot api webui; do
  readarray -d '' -t schema_args < <(build_args "$component" "$HEAD_SHA" "" . "" current "$(staging_ref "$component")")
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
readarray -d '' -t launcher_args < <(build_args bot "$HEAD_SHA" "" . "" current "$(staging_ref bot)")
for field in launcher-revision=18 min-launcher-revision=7; do
  contains_arg "io.jmodguard.${field}" "${launcher_args[@]}" || fail "missing launcher requirement ${field}"
done
launcher_source="$(mktemp -d)"
mkdir -p "$launcher_source/src/db" "$launcher_source/migrations/guild"
cp src/db.rs "$launcher_source/src/db.rs"
cp migrations/guild/current_version.rs "$launcher_source/migrations/guild/current_version.rs"
sed -e 's/^pub const LAUNCHER_REVISION: u32 = 18;$/pub const LAUNCHER_REVISION: u32 = 19;/' \
    src/db/update_control.rs >"$launcher_source/src/db/update_control.rs"
readarray -d '' -t launcher_args < <(build_args api "$HEAD_SHA" "" "$launcher_source" "" current "$(staging_ref api)")
for field in launcher-revision=19 min-launcher-revision=7; do
  contains_arg "io.jmodguard.${field}" "${launcher_args[@]}" || fail "selected source lost ${field}"
done
rm -rf "$launcher_source"

# Publishing an older source must keep its schema, not the publisher's schema.
schema_source="$(mktemp -d)"
mkdir -p "$schema_source/src"
sed -e '/include_str!("..\/migrations\/001[6789]_/d' -e '/include_str!("..\/migrations\/0020_/d' src/db.rs >"$schema_source/src/db.rs"
readarray -d '' -t old_schema_args < <(build_args bot "$BASE_SHA" "" "$schema_source" "" current "$(staging_ref bot "$BASE_SHA")")
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
  readarray -d '' -t broken_launcher_args < <(build_args "$component" "$BASE_SHA" "" "$schema_source" "" current "$(staging_ref "$component" "$BASE_SHA")")
  contains_arg "io.jmodguard.result-schema=16" "${broken_launcher_args[@]}" \
    || fail 'historical schema16 labels must remain truthful'
  contains_arg "io.jmodguard.launcher-protocol=1" "${broken_launcher_args[@]}" \
    && fail 'historical schema15-reporting launcher must not be installable on schema16'
done
printf 'invalid source\n' >"$schema_source/src/db.rs"
if (build_args bot "$HEAD_SHA" "" "$schema_source" "" current "$(staging_ref bot)") >/dev/null 2>&1; then
  fail 'unknown schema must not produce installable image labels'
fi
rm -rf "$schema_source"

reload_publisher() {
  # shellcheck disable=SC1091
  JMODGUARD_PUBLISH_IMAGE_LIB=1 source "$root/scripts/publish-image.sh"
  eval "$(declare -f sign_published_digest | sed '1s/^sign_published_digest/_real_sign_published_digest/')"
  SIGNING_PUBLIC_KEY="$TEST_SIGNING_PUBLIC_KEY"
  export SIGNING_PUBLIC_KEY
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
FAIL_RUNTIME=0
FAIL_SIGN=0
PROMOTE_CREATE_ATTEMPTS=0
MOCK_PROMOTE_FAIL_ON=""
PUBLISH_SHA="$HEAD_SHA"
SOURCE_LAYOUT=current
TRACE_FILE=""
PUBLISH_TEST_TRACE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/publish-traces-XXXXXX")"

mock_publish_helpers() {
  CALLS=()
  LOGIN_INPUT=""
  REDACTOR_INPUT=""
  BUILD_LINE=""
  GITHUB_TOKEN_SEEN=""
  PROMOTE_CREATE_ATTEMPTS=0
  TRACE_FILE="$(mktemp "${PUBLISH_TEST_TRACE_DIR}/publish-trace-XXXXXX")"

  verify_published_image() {
    printf 'verify %s %s@%s %s\n' "$1" "$2" "$3" "${5:-current}" >>"$TRACE_FILE"
    [[ "$FAIL_VERIFY" == 0 ]] || publish_fail 'effective image verification failed'
    verify_image_runtime_configuration "$1" "${2}@${3}" "${5:-current}" \
      || return 1
  }
  verify_image_runtime_configuration() {
    printf 'verify runtime %s\n' "$1" >>"$TRACE_FILE"
    [[ "$FAIL_RUNTIME" == 0 ]] || return 1
  }
  # The browser bundle itself is tested by package-webui-test.mjs. Here prove
  # publication installs/builds in the selected source worktree before packaging.
  npm() {
    printf 'npm %s\n' "$*" >>"$TRACE_FILE"
    if [[ "$1" == run && "$2" == build ]]; then
      signing_secrets_in_environ && fail 'signing secrets must not remain in the process environment during WebUI build'
      [[ -z "$_JMG_SIGNING_KEY_FILE" || ! -f "$_JMG_SIGNING_KEY_FILE" ]] \
        || fail 'signing key file must not exist during WebUI build'
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
      if [[ "$2" == build ]]; then
        assert_signing_secrets_not_in_environ
        [[ -z "$_JMG_SIGNING_KEY_FILE" || ! -f "$_JMG_SIGNING_KEY_FILE" ]] \
          || fail 'signing key file must not exist during image build'
        local arg prev=""
        local wrote_metadata=0
        for arg in "$@"; do
          if [[ "$prev" == --metadata-file ]]; then
            printf '{"containerimage.digest":"%s"}\n' "$MOCK_BUILD_DIGEST" >"$arg"
            wrote_metadata=1
          fi
          prev="$arg"
        done
        [[ "$wrote_metadata" == 1 ]] || fail 'image build must record digest metadata'
      fi
      if [[ "$2" == imagetools ]]; then
        if [[ "$*" == *'{{json .Image}}'* ]]; then
          printf '%s' '{"config":{"Labels":{"io.jmodguard.published-at":"2026-10-27T12:00:00Z","org.opencontainers.image.revision":"abc"},"Entrypoint":["/bin/sh","/usr/local/bin/jmodguard-bootstrap"]}}'
          return 0
        fi
        printf '{"digest":"%s","size":10}\n' "$MOCK_BUILD_DIGEST"
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
  run_cosign() {
    printf 'cosign %s\n' "$*" >>"$TRACE_FILE"
    [[ "$FAIL_SIGN" == 0 ]] || publish_fail 'cosign failed: sign'
  }
  registry_promote_tag() {
    printf 'docker buildx imagetools create %s\n' "$*" >>"$TRACE_FILE"
    if [[ -n "$MOCK_PROMOTE_FAIL_ON" && "$MOCK_PROMOTE_FAIL_ON" == "$((PROMOTE_CREATE_ATTEMPTS + 1))" ]]; then
      PROMOTE_CREATE_ATTEMPTS=$((PROMOTE_CREATE_ATTEMPTS + 1))
      return 1
    fi
    PROMOTE_CREATE_ATTEMPTS=$((PROMOTE_CREATE_ATTEMPTS + 1))
    return 0
  }
  sign_published_digest() {
    printf 'sign %s %s %s\n' "$1" "$2" "$3" >>"$TRACE_FILE"
    SIGNING_PUBLIC_KEY="$TEST_SIGNING_PUBLIC_KEY"
    export SIGNING_PUBLIC_KEY
    _real_sign_published_digest "$1" "$2" "$3"
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
  export GHCR_USERNAME GHCR_TOKEN GITHUB_TOKEN
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
  JMODGUARD_PUBLISH_STATE_DIR="$(mktemp -d "${saved_tmp}/publish-state-XXXXXX")"
  export JMODGUARD_PUBLISH_STATE_DIR
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
  printf '%s\n' "$TRACE_FILE" >"$PUBLISH_TEST_TRACE_DIR/latest-trace-path"
  local publish_rc=0
  if [[ "$FAIL_BUILD" == 1 || "$FAIL_VERIFY" == 1 ]]; then
    ( publish_component "$component" ) >/dev/null 2>&1 || publish_rc=$?
    TRACE_FILE="$(<"$PUBLISH_TEST_TRACE_DIR/latest-trace-path")"
    load_trace
    [[ "$publish_rc" -ne 0 ]] || fail 'failed build must not succeed'
  else
    local publish_err
    publish_err="$(mktemp "${scratch}/publish-err-XXXXXX")"
    ( publish_component "$component" ) >"$publish_err" 2>&1 || publish_rc=$?
    if [[ "$publish_rc" -ne 0 ]]; then
      PUBLISH_LAST_ERROR="$(<"$publish_err")"
      TRACE_FILE="$(<"$PUBLISH_TEST_TRACE_DIR/latest-trace-path")"
      load_trace
      rm -rf "$scratch"
      TMPDIR="$saved_tmp"
      export TMPDIR
      SOURCE_LAYOUT=current
      return "$publish_rc"
    fi
    rm -f "$publish_err"
  fi
  TRACE_FILE="$(<"$PUBLISH_TEST_TRACE_DIR/latest-trace-path")"
  load_trace
  [[ -z "$(ls -A "$scratch" 2>/dev/null || true)" ]] || fail "scratch leftover: $(ls -A "$scratch")"
  rm -rf "$scratch" "$JMODGUARD_PUBLISH_STATE_DIR"
  TMPDIR="$saved_tmp"
  unset JMODGUARD_PUBLISH_STATE_DIR
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
( publish_component api ) >/dev/null 2>&1 || true
load_trace
printf '%s\n' "${CALLS[@]}" | grep -q 'plan get' \
  || fail 'skip without credentials must still read the plan'
printf '%s\n' "${CALLS[@]}" | grep -qE 'docker |secret get|mask tokens' \
  && fail "skip without credentials must not touch docker or secrets: ${CALLS[*]}"

exercise_publish 1 || fail "main publication exercise failed: ${PUBLISH_LAST_ERROR:-unknown}"
printf '%s\n' "$BUILD_LINE" | grep -q -- '--provenance=false' || fail 'build must disable provenance'
printf '%s\n' "$BUILD_LINE" | grep -q -- '--sbom=false' || fail 'build must disable sbom'
printf '%s\n' "$BUILD_LINE" | grep -q 'linux/amd64' || fail 'build must be linux/amd64'
printf '%s\n' "$BUILD_LINE" | grep -q -- '--metadata-file' || fail 'build must record output digest metadata'
printf '%s\n' "$BUILD_LINE" | grep -q "$(staging_ref api)" || fail 'build must tag the staging reference'
printf '%s\n' "$BUILD_LINE" | grep -q "ghcr.io/jatmn/jmodguard-api:${HEAD_SHA}" \
  && fail 'build must not push the source-SHA tag before verification and signing'
grep -Fq 'digest_from_build_metadata' "$root/scripts/publish-image.sh" \
  || fail 'publisher must read the build output digest from metadata'
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
  [[ "$BUILD_LINE" == *"$(staging_ref "$component" "$BASE_SHA")"* ]] \
    || fail "$component release must build the selected release SHA to staging"
  [[ "$BUILD_LINE" != *"--tag ghcr.io/jatmn/jmodguard-${component}:v1.2.3"* ]] \
    || fail "$component release must not push the version tag before signing"
  verify_line="$(printf '%s\n' "${CALLS[@]}" | grep -n "^verify $component " | cut -d: -f1)"
  sign_line="$(printf '%s\n' "${CALLS[@]}" | grep -n "^sign $component " | cut -d: -f1)"
  version_line="$(printf '%s\n' "${CALLS[@]}" | grep -n -- "--tag ghcr.io/jatmn/jmodguard-${component}:v1.2.3" | cut -d: -f1)"
  stable_line="$(printf '%s\n' "${CALLS[@]}" | grep -n -- "--tag ghcr.io/jatmn/jmodguard-${component}:stable" | cut -d: -f1)"
  [[ -n "$verify_line" && -n "$sign_line" && -n "$version_line" && -n "$stable_line" && "$verify_line" -lt "$sign_line" && "$sign_line" -lt "$version_line" && "$sign_line" -lt "$stable_line" ]] \
    || fail "$component release must sign before promoting version and Stable tags"

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
printf '%s\n' "$BUILD_LINE" | grep -q "$(staging_ref api "$BASE_SHA")" \
  || fail 'manual publication must build the planned SHA to staging even when compare would skip'

for component in bot api webui; do
  exercise_publish 0 0 manual '' '' "$component" 0 current "$BASE_SHA"
  printf '%s\n' "$BUILD_LINE" | grep -q "$(staging_ref "$component" "$BASE_SHA")" \
    || fail "manual SHA publication must still build ${component} to staging"
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
  if ( PUBLISHED_AT='2026-10-27T99:00:00Z' not_after_from_published_at "$PUBLISHED_AT" ) >/dev/null 2>&1; then
  fail 'invalid publication timestamps must not compute expiry'
fi
printf '%s\n' "$BUILD_LINE" | grep -Eq 'io\.jmodguard\.published-at=[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z' \
    || fail "historical ${component} must label a valid publication timestamp for signing"
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
    buildx)
      if [[ "$2" == imagetools && "$*" == *'{{json .Image}}'* ]]; then
        printf '%s' '{"config":{"Entrypoint":[]}}'
        return 0
      fi
      return 0
      ;;
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
verify_published_image bot historical-image "sha256:$(printf 'b%.0s' {1..64})" . legacy

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

git_fixture_scratch="$(mktemp -d "${TMPDIR:-/tmp}/publish-image-git-XXXXXX")"
cleanup_git() { rm -rf "$git_fixture_scratch"; }
trap cleanup_git EXIT
git init -b main "$git_fixture_scratch" >/dev/null
git -C "$git_fixture_scratch" config user.name Fixture
git -C "$git_fixture_scratch" config user.email fixture@example.invalid
mkdir -p "$git_fixture_scratch/src"
echo old >"$git_fixture_scratch/src/lib.rs"
printf 'FROM scratch\nCOPY role /usr/local/bin/jmodguard-bot\n' >"$git_fixture_scratch/Dockerfile.bot"
git -C "$git_fixture_scratch" add .
git -C "$git_fixture_scratch" commit -m base >/dev/null
base="$(git -C "$git_fixture_scratch" rev-parse HEAD)"
git -C "$git_fixture_scratch" tag -a 'release/v1+meta' -m release
git -C "$git_fixture_scratch" remote add origin "$git_fixture_scratch"
fixture_git_root="$git_fixture_scratch"

pushd "$git_fixture_scratch" >/dev/null
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
published_signature_still_valid() { return 0; }
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
published_signature_still_valid() { return 0; }
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
published_signature_still_valid() { return 0; }
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
rm -rf "${JMODGUARD_PUBLISH_STATE_DIR:-/tmp/jmodguard-publish-state}"
JMODGUARD_PUBLISH_STATE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/publish-state-partial-XXXXXX")"
export JMODGUARD_PUBLISH_STATE_DIR
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
JMODGUARD_UPDATE_SIGNING_KEY="$SIGNING_KEY_VALUE"
JMODGUARD_UPDATE_SIGNING_PASSWORD="$SIGNING_PASSWORD_VALUE"
export GITHUB_SHA GHCR_USERNAME GHCR_TOKEN GITHUB_TOKEN \
  JMODGUARD_UPDATE_SIGNING_KEY JMODGUARD_UPDATE_SIGNING_PASSWORD
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
record_buildx_tag() {
  local prev="" arg
  for arg in "$@"; do
    if [[ "$prev" == --tag ]]; then
      REGISTRY["$arg"]="$manifest"
    fi
    prev="$arg"
  done
}
record_imagetools_promote() {
  local prev="" arg
  for arg in "$@"; do
    if [[ "$prev" == --tag ]]; then
      REGISTRY["$arg"]="$manifest"
    fi
    prev="$arg"
  done
}
published_signature_still_valid() { return 0; }
run_docker() {
  if [[ "$1" == login ]]; then cat >/dev/null; return 0; fi
  if [[ "$1 $2 $3" == 'buildx imagetools create' ]]; then
    record_imagetools_promote "$@"
    return 0
  fi
  if [[ "$1 $2" == 'buildx imagetools' ]]; then
    if [[ "$*" == *'{{json .Image}}'* ]]; then
      printf '%s' '{"config":{"Labels":{"io.jmodguard.published-at":"2026-10-27T12:00:00Z","org.opencontainers.image.revision":"abc"},"Entrypoint":["/bin/sh","/usr/local/bin/jmodguard-bootstrap"]}}'
      return 0
    fi
    printf '{"digest":"sha256:%064d","size":10}\n' 0
    return 0
  fi
  if [[ "$1" == buildx && "$2" == build ]]; then
    EXPORTS=$((EXPORTS + 1))
    local prev="" arg
    for arg in "$@"; do
      if [[ "$prev" == --metadata-file ]]; then
        printf '{"containerimage.digest":"sha256:%064d"}\n' 0 >"$arg"
      fi
      prev="$arg"
    done
    record_buildx_tag "$@"
    if [[ "$EXPORTS" -eq 1 ]]; then
      publish_fail 'docker failed: buildx build'
    fi
    return 0
  fi
  return 0
}
verify_published_image() { :; }
verify_image_runtime_configuration() { :; }
sign_published_digest() { :; }
registry_promote_tag() {
  run_docker buildx imagetools create "$@" || return 1
}
if ( publish_component bot ) >/dev/null 2>&1; then
  fail 'first partial build must fail'
fi
# Subshell cannot keep REGISTRY updates; replay the first-push side effect.
REGISTRY["$(staging_ref bot "$base")"]="$manifest"
REGISTRY["${image}:latest"]="$manifest"
EXPORTS=1
[[ -z "${REGISTRY[${image}:${base}]:-}" ]] || fail 'failed first push must not record the SHA tag'
publish_component bot
[[ "${REGISTRY[${image}:${base}]:-}" == "${REGISTRY[${image}:latest]:-}" ]] || fail 'retry must repair the SHA tag'
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
JMODGUARD_UPDATE_SIGNING_KEY="$SIGNING_KEY_VALUE"
JMODGUARD_UPDATE_SIGNING_PASSWORD="$SIGNING_PASSWORD_VALUE"
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
  if [[ "$1 $2 $3" == 'buildx imagetools create' ]]; then
    record_imagetools_promote "$@"
    return 0
  fi
  if [[ "$1 $2" == 'buildx imagetools' ]]; then
    if [[ "$*" == *'{{json .Image}}'* ]]; then
      printf '%s' '{"config":{"Labels":{"io.jmodguard.published-at":"2026-10-27T12:00:00Z","org.opencontainers.image.revision":"abc"},"Entrypoint":["/bin/sh","/usr/local/bin/jmodguard-bootstrap"]}}'
      return 0
    fi
    printf '{"digest":"sha256:%064d","size":10}\n' 0
    return 0
  fi
  if [[ "$1" == buildx && "$2" == build ]]; then
    EXPORTS=$((EXPORTS + 1))
    local prev="" arg
    for arg in "$@"; do
      if [[ "$prev" == --metadata-file ]]; then
        printf '{"containerimage.digest":"sha256:%064d"}\n' 0 >"$arg"
      fi
      prev="$arg"
    done
    record_buildx_tag "$@"
    return 0
  fi
  return 0
}
verify_published_image() { :; }
verify_image_runtime_configuration() { :; }
sign_published_digest() { :; }
registry_promote_tag() {
  run_docker buildx imagetools create "$@" || return 1
}
publish_component bot
[[ "$EXPORTS" -eq 1 ]] || fail "retried job must rebuild, got $EXPORTS exports"

popd >/dev/null

reload_publisher
FAIL_SIGN=0
JMODGUARD_UPDATE_SIGNING_KEY=""
JMODGUARD_UPDATE_SIGNING_PASSWORD="$SIGNING_PASSWORD_VALUE"
if exercise_publish 1 >/dev/null 2>&1; then
  fail 'missing signing key must fail publication'
fi
printf '%s\n' "${CALLS[@]}" | grep -q 'docker buildx imagetools create' \
  && fail 'missing signing key must not promote tags'

reload_publisher
JMODGUARD_UPDATE_SIGNING_KEY="$SIGNING_KEY_VALUE"
JMODGUARD_UPDATE_SIGNING_PASSWORD=""
if exercise_publish 1 >/dev/null 2>&1; then
  fail 'missing signing password must fail publication'
fi
printf '%s\n' "${CALLS[@]}" | grep -q 'docker buildx imagetools create' \
  && fail 'missing signing password must not promote tags'

FAIL_SIGN=1
JMODGUARD_UPDATE_SIGNING_KEY="$SIGNING_KEY_VALUE"
JMODGUARD_UPDATE_SIGNING_PASSWORD="$SIGNING_PASSWORD_VALUE"
if exercise_publish 1 >/dev/null 2>&1; then
  fail 'failed signing must fail publication'
fi
printf '%s\n' "${CALLS[@]}" | grep -q 'docker buildx imagetools create' \
  && fail 'failed signing must not promote tags'
FAIL_SIGN=0
unset JMODGUARD_UPDATE_SIGNING_KEY JMODGUARD_UPDATE_SIGNING_PASSWORD

# Split build/sign phases must fail closed without state or promotion.
reload_publisher
mock_publish_helpers
FAIL_RUNTIME=1
FAIL_VERIFY=0
PROMOTE_CREATE_ATTEMPTS=0
MOCK_PROMOTE_FAIL_ON=""
phase_state_dir="$(mktemp -d "${TMPDIR:-/tmp}/publish-phase-state-XXXXXX")"
JMODGUARD_PUBLISH_STATE_DIR="$phase_state_dir"
export JMODGUARD_PUBLISH_STATE_DIR
JMODGUARD_PUBLISH_PHASE=build
export JMODGUARD_PUBLISH_PHASE
with_main
GHCR_USERNAME=publisher
GHCR_TOKEN=test-secret
GITHUB_TOKEN=""
PLAN_JSON="$(plan_json main "" "$HEAD_SHA" "$HEAD_SHA" "")"
export GHCR_USERNAME GHCR_TOKEN GITHUB_TOKEN PLAN_JSON
changed_since_publication() { return 0; }
if ( publish_component api ) >/dev/null 2>&1; then
  fail 'build phase must fail when runtime verification fails'
fi
[[ -f "$phase_state_dir/api.json" ]] && fail 'build phase must not write publish state after runtime verification failure'
load_trace
printf '%s\n' "${CALLS[@]}" | grep -q 'docker buildx imagetools create' \
  && fail 'build phase must not promote tags when verification fails'
rm -rf "$phase_state_dir"
unset JMODGUARD_PUBLISH_PHASE FAIL_RUNTIME

reload_publisher
mock_publish_helpers
FAIL_RUNTIME=0
FAIL_VERIFY=0
FAIL_SIGN=0
PROMOTE_CREATE_ATTEMPTS=0
phase_state_dir="$(mktemp -d "${TMPDIR:-/tmp}/publish-phase-state-XXXXXX")"
JMODGUARD_PUBLISH_STATE_DIR="$phase_state_dir"
export JMODGUARD_PUBLISH_STATE_DIR
JMODGUARD_PUBLISH_PHASE=build
export JMODGUARD_PUBLISH_PHASE
with_main
GHCR_USERNAME=publisher
GHCR_TOKEN=test-secret
GITHUB_TOKEN=""
PLAN_JSON="$(plan_json main "" "$HEAD_SHA" "$HEAD_SHA" "")"
export GHCR_USERNAME GHCR_TOKEN GITHUB_TOKEN PLAN_JSON
changed_since_publication() { return 0; }
( publish_component api ) >/dev/null 2>&1 || fail 'build phase must succeed before sign regression'
[[ -f "$phase_state_dir/api.json" ]] || fail 'build phase must persist publish state for signing'
: >"$TRACE_FILE"
JMODGUARD_PUBLISH_PHASE=sign
export JMODGUARD_PUBLISH_PHASE
FAIL_SIGN=1
if ( publish_component api ) >/dev/null 2>&1; then
  fail 'sign phase must fail when cosign sign fails'
fi
load_trace
printf '%s\n' "${CALLS[@]}" | grep -q 'docker buildx imagetools create' \
  && fail 'sign phase must not promote tags when signing fails'
rm -rf "$phase_state_dir"
unset JMODGUARD_PUBLISH_PHASE FAIL_SIGN

reload_publisher
mock_publish_helpers
handoff_dir="$(mktemp -d "${TMPDIR:-/tmp}/publish-handoff-XXXXXX")"
JMODGUARD_PUBLISH_STATE_DIR="$handoff_dir"
export JMODGUARD_PUBLISH_STATE_DIR
write_publish_handoff api false
JMODGUARD_PUBLISH_PHASE=sign
export JMODGUARD_PUBLISH_PHASE
with_main
output="$(publish_component api 2>&1)" || fail 'sign phase must succeed when handoff marks no verified image'
[[ "$output" == *'skipping promotion'* ]] || fail "expected sign skip message, got: $output"
[[ -f "$handoff_dir/api.json" ]] && fail 'sign skip must not require component state file'
rm -rf "$handoff_dir"
unset JMODGUARD_PUBLISH_PHASE JMODGUARD_PUBLISH_STATE_DIR

# Sign phase must derive digest and promotion targets from the plan and registry, not build state.
reload_publisher
mock_publish_helpers
FAIL_SIGN=0
trusted_sign_dir="$(mktemp -d "${TMPDIR:-/tmp}/publish-trusted-sign-XXXXXX")"
JMODGUARD_PUBLISH_STATE_DIR="$trusted_sign_dir"
export JMODGUARD_PUBLISH_STATE_DIR
FORGED_SHA="$(printf 'f%.0s' {1..40})"
FORGED_DIGEST="sha256:$(printf 'f%.0s' {1..64})"
TRUSTED_STAGING="$(staging_ref api)"
write_publish_state api "$(COMPONENT=api IMAGE=ghcr.io/jatmn/jmodguard-api SHA="$FORGED_SHA" LABEL="" MODE=main CHANNEL="" DIGEST="$FORGED_DIGEST" STAGING_REF="$TRUSTED_STAGING" node --input-type=commonjs - <<'JS'
process.stdout.write(JSON.stringify({
  component: process.env.COMPONENT,
  image: process.env.IMAGE,
  sha: process.env.SHA,
  label: process.env.LABEL,
  mode: process.env.MODE,
  channel: process.env.CHANNEL,
  published_digest: process.env.DIGEST,
  staging_ref: process.env.STAGING_REF,
}));
JS
)"
write_publish_handoff api true
JMODGUARD_PUBLISH_PHASE=sign
export JMODGUARD_PUBLISH_PHASE
with_main
GHCR_USERNAME=publisher
GHCR_TOKEN=test-secret
GITHUB_TOKEN=""
PLAN_JSON="$(plan_json main "" "$HEAD_SHA" "$HEAD_SHA" "")"
JMODGUARD_UPDATE_SIGNING_KEY="$SIGNING_KEY_VALUE"
JMODGUARD_UPDATE_SIGNING_PASSWORD="$SIGNING_PASSWORD_VALUE"
export GHCR_USERNAME GHCR_TOKEN GITHUB_TOKEN PLAN_JSON \
  JMODGUARD_UPDATE_SIGNING_KEY JMODGUARD_UPDATE_SIGNING_PASSWORD
if ( publish_component api ) >/dev/null 2>&1; then
  fail 'sign phase must reject build state that disagrees with the trusted publication plan'
fi
rm -f "$trusted_sign_dir/api.json"
: >"$TRACE_FILE"
( publish_component api ) >/dev/null 2>&1 || fail 'sign phase must succeed from trusted plan and registry without build state'
load_trace
printf '%s\n' "${CALLS[@]}" | grep -q "^sign api ghcr.io/jatmn/jmodguard-api ${MOCK_BUILD_DIGEST}$" \
  || fail 'sign phase must sign the registry digest for the trusted staging reference'
printf '%s\n' "${CALLS[@]}" | grep -q "@${MOCK_BUILD_DIGEST}" \
  || fail 'sign phase must promote the trusted registry digest, not forged build state'
printf '%s\n' "${CALLS[@]}" | grep -q -- "--tag ghcr.io/jatmn/jmodguard-api:${HEAD_SHA}" \
  || fail 'sign phase must promote tags derived from the publication plan SHA'
printf '%s\n' "${CALLS[@]}" | grep -q -- "--tag ghcr.io/jatmn/jmodguard-api:${FORGED_SHA}" \
  && fail 'sign phase must not promote tags from forged build state'
rm -rf "$trusted_sign_dir"
unset JMODGUARD_PUBLISH_PHASE JMODGUARD_PUBLISH_STATE_DIR

reload_publisher
PROD_PROMOTE_CREATE=0
ROLLBACK_PROMOTE_IMAGE=ghcr.io/jatmn/jmodguard-api
ROLLBACK_PROMOTE_DIGEST="sha256:$(printf 'c%.0s' {1..64})"
ROLLBACK_PROMOTE_PREV="sha256:$(printf 'b%.0s' {1..64})"
declare -A ROLLBACK_PROMOTE_TAG_DIGEST=()
docker() {
  local prev="" arg ref digest
  if [[ "$1 $2 $3" == 'buildx imagetools create' ]]; then
    for arg in "$@"; do
      if [[ "$prev" == --tag ]]; then
        ref="$arg"
      fi
      if [[ "$arg" == *@sha256:* ]]; then
        digest="${arg#*@}"
      fi
      prev="$arg"
    done
    [[ -n "$ref" && -n "$digest" ]] && ROLLBACK_PROMOTE_TAG_DIGEST["$ref"]="$digest"
    PROD_PROMOTE_CREATE=$((PROD_PROMOTE_CREATE + 1))
    [[ "$PROD_PROMOTE_CREATE" -lt 2 ]] || return 1
    return 0
  fi
  return 0
}
run_docker() {
  local prev="" arg ref digest
  if [[ "$1 $2 $3" == 'buildx imagetools create' ]]; then
    for arg in "$@"; do
      if [[ "$prev" == --tag ]]; then
        ref="$arg"
      fi
      if [[ "$arg" == *@sha256:* ]]; then
        digest="${arg#*@}"
      fi
      prev="$arg"
    done
    [[ -n "$ref" && -n "$digest" ]] && ROLLBACK_PROMOTE_TAG_DIGEST["$ref"]="$digest"
    return 0
  fi
  if [[ "$1 $2" == 'buildx imagetools' && "$3" == inspect && "$*" == *'{{json .Manifest}}'* ]]; then
    ref="$4"
    digest="${ROLLBACK_PROMOTE_TAG_DIGEST[$ref]:-$ROLLBACK_PROMOTE_DIGEST}"
    printf '{"digest":"%s","size":10}\n' "$digest"
    return 0
  fi
  if [[ "$1 $2" == 'buildx imagetools' && "$*" == *'{{json .Image}}'* ]]; then
    printf '%s' '{"config":{"Labels":{"io.jmodguard.published-at":"2026-10-27T12:00:00Z","org.opencontainers.image.revision":"abc"},"Entrypoint":["/bin/sh","/usr/local/bin/jmodguard-bootstrap"]}}'
    return 0
  fi
  return 0
}
ROLLBACK_PROMOTE_TAG_DIGEST["${ROLLBACK_PROMOTE_IMAGE}:${HEAD_SHA}"]="$ROLLBACK_PROMOTE_PREV"
PROMOTION_ROLLBACK=()
promote_image_tag "$ROLLBACK_PROMOTE_IMAGE" "${HEAD_SHA}" "$ROLLBACK_PROMOTE_DIGEST" \
  || fail 'first promotion must succeed before create failure'
ROLLBACK_PROMOTE_TAG_DIGEST["${ROLLBACK_PROMOTE_IMAGE}:${HEAD_SHA}"]="$ROLLBACK_PROMOTE_DIGEST"
[[ "${#PROMOTION_ROLLBACK[@]}" -eq 1 ]] || fail 'first promotion must record rollback state'
if promote_image_tag "$ROLLBACK_PROMOTE_IMAGE" latest "$ROLLBACK_PROMOTE_DIGEST"; then
  fail 'production registry_promote_tag must fail when imagetools create fails'
fi
rollback_promoted_tags
[[ "${ROLLBACK_PROMOTE_TAG_DIGEST[${ROLLBACK_PROMOTE_IMAGE}:${HEAD_SHA}]:-}" == "$ROLLBACK_PROMOTE_PREV" ]] \
  || fail 'rollback must restore the first promoted tag after production create failure'
unset -f docker

reload_publisher
JMODGUARD_UPDATE_SIGNING_KEY="$SIGNING_KEY_VALUE"
JMODGUARD_UPDATE_SIGNING_PASSWORD="$SIGNING_PASSWORD_VALUE"
capture_signing_secrets
assert_signing_secrets_not_in_environ
_JMG_SIGNING_KEY=""
_JMG_SIGNING_PASSWORD=""

reload_publisher
WRONG_SIGNING_DIR="$(mktemp -d "${TMPDIR:-/tmp}/wrong-signing-XXXXXX")"
COSIGN_PASSWORD="$TEST_SIGNING_PASSWORD" cosign generate-key-pair --output-key-prefix "$WRONG_SIGNING_DIR/wrong"
JMODGUARD_UPDATE_SIGNING_KEY="$(<"$WRONG_SIGNING_DIR/wrong.key")"
JMODGUARD_UPDATE_SIGNING_PASSWORD="$TEST_SIGNING_PASSWORD"
if exercise_publish 1 >/dev/null 2>&1; then
  fail 'mismatched signing key must fail before image build'
else
  :
fi
if printf '%s\n' "${CALLS[@]}" | grep -q 'docker buildx build'; then
  fail 'mismatched signing key must not build images'
fi
rm -rf "$WRONG_SIGNING_DIR"
unset JMODGUARD_UPDATE_SIGNING_KEY JMODGUARD_UPDATE_SIGNING_PASSWORD

reload_publisher
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
readarray -d '' -t legacy_sign_args < <(build_args bot "$HEAD_SHA" "" . stable legacy "$(staging_ref bot)")
printf '%s\n' "${legacy_sign_args[@]}" | grep -Eq 'io\.jmodguard\.published-at=[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z' \
  || fail 'legacy builds must carry a valid publication timestamp for signing'
printf '%s\n' "${legacy_sign_args[@]}" | grep -q 'io.jmodguard.launcher-protocol=1' \
  && fail 'legacy builds must not claim updater compatibility'

grep -q 'BEGIN PUBLIC KEY' "$root/deploy/update-signing.pub" \
  || fail 'deploy/update-signing.pub must contain the committed production cosign public key'
PROD_SIGNING_FINGERPRINT="$(openssl pkey -pubin -in "$root/deploy/update-signing.pub" -outform DER | openssl dgst -sha256 | awk '{print $2}')"
assert_eq "$PROD_SIGNING_FINGERPRINT" "d36f7ae0529b99c64f662c319855648309c7e0d6f92895b777193bae52136d17" \
  || fail 'deploy/update-signing.pub fingerprint changed unexpectedly'

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
PROD_SIGN_DIGEST="sha256:$(printf 'a%.0s' {1..64})"
PROD_SIGN_IMAGE=ghcr.io/jatmn/jmodguard-bot
PROD_SIGN_SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/prod-sign-XXXXXX")"
JMODGUARD_UPDATE_SIGNING_KEY="$SIGNING_KEY_VALUE"
JMODGUARD_UPDATE_SIGNING_PASSWORD="$SIGNING_PASSWORD_VALUE"
capture_signing_secrets
persist_signing_secrets_to_files "$PROD_SIGN_SCRATCH"
assert_signing_secrets_not_in_environ
PROD_SIGN_KEY_FILE="${PROD_SIGN_SCRATCH}/signing.key"
sign_published_digest bot "$PROD_SIGN_IMAGE" "$PROD_SIGN_DIGEST"
grep -Fq 'cosign sign --yes' "$COSIGN_TRACE" || fail 'production signing must pass --yes to cosign sign'
grep -Fq "${PROD_SIGN_IMAGE}@${PROD_SIGN_DIGEST}" "$COSIGN_TRACE" || fail 'production signing must target the verified digest reference'
grep -Fq "cosign sign --yes --key ${PROD_SIGN_KEY_FILE}" "$COSIGN_TRACE" \
  || fail 'production signing must use a key file'
rm -rf "$PROD_SIGN_SCRATCH"
grep -Fq 'env://JMODGUARD_UPDATE_SIGNING_KEY' "$COSIGN_TRACE" \
  && fail 'production signing must not pass the private key through the environment'
grep -Fq 'io.jmodguard.not-after=' "$COSIGN_TRACE" || fail 'production signing must attach the expiry annotation'
grep -Fq -- '--annotation' "$COSIGN_TRACE" && fail 'cosign sign must use -a/--annotations, not --annotation'
grep -Fq ' -a io.jmodguard.not-after=' "$COSIGN_TRACE" \
  || grep -Fq -- '-a io.jmodguard.not-after=' "$COSIGN_TRACE" \
  || fail 'cosign sign must pass -a for the expiry annotation'
grep -Fq 'cosign verify' "$COSIGN_TRACE" || fail 'production signing must invoke cosign verify'
grep -Fq -- "--key ${SIGNING_PUBLIC_KEY}" "$COSIGN_TRACE" \
  || grep -Fq -- "--key $root/deploy/update-signing.pub" "$COSIGN_TRACE" \
  || fail 'production verification must use the committed cosign public key'
grep -Fq "${PROD_SIGN_IMAGE}@${PROD_SIGN_DIGEST}" "$COSIGN_TRACE" || fail 'production verification must target the verified digest reference'
_JMG_SIGNING_KEY=""
_JMG_SIGNING_PASSWORD=""

reload_publisher
runtime_image_json() {
  local component="$1" layout="${2:-current}"
  if [[ "$layout" == legacy ]]; then
    printf '%s' '{"config":{"Labels":{"org.opencontainers.image.revision":"abc"},"Entrypoint":[]}}'
  elif [[ "$component" == webui ]]; then
    printf '%s' '{"config":{"Labels":{"org.opencontainers.image.revision":"abc"},"Entrypoint":[]}}'
  else
    printf '%s' '{"config":{"Labels":{"org.opencontainers.image.revision":"abc"},"Entrypoint":["/bin/sh","/usr/local/bin/jmodguard-bootstrap"]}}'
  fi
}
run_docker() {
  if [[ "$1 $2" == 'buildx imagetools' && "$*" == *'{{json .Image}}'* ]]; then
    printf '%s' "$(runtime_image_json "$RUNTIME_TEST_COMPONENT" "$RUNTIME_TEST_LAYOUT")"
    return 0
  fi
  return 0
}
for RUNTIME_TEST_COMPONENT in bot api; do
  RUNTIME_TEST_LAYOUT=current
  verify_image_runtime_configuration "$RUNTIME_TEST_COMPONENT" 'ghcr.io/jatmn/jmodguard-bot@sha256:deadbeef' current \
    || fail "${RUNTIME_TEST_COMPONENT} current layout entrypoint must match Dockerfile"
done
RUNTIME_TEST_COMPONENT=webui
RUNTIME_TEST_LAYOUT=current
verify_image_runtime_configuration webui 'ghcr.io/jatmn/jmodguard-webui@sha256:deadbeef' current \
  || fail 'webui scratch image must have no entrypoint'
RUNTIME_TEST_COMPONENT=bot
RUNTIME_TEST_LAYOUT=legacy
verify_image_runtime_configuration bot 'ghcr.io/jatmn/jmodguard-bot@sha256:deadbeef' legacy \
  || fail 'legacy service images must have no entrypoint'
RUNTIME_TEST_LAYOUT=current
run_docker() {
  if [[ "$1 $2" == 'buildx imagetools' && "$*" == *'{{json .Image}}'* ]]; then
    printf '%s' '{"config":{"Labels":{"org.opencontainers.image.revision":"abc"},"Entrypoint":["/wrong"]}}'
    return 0
  fi
  return 0
}
if verify_image_runtime_configuration bot 'ghcr.io/jatmn/jmodguard-bot@sha256:deadbeef' current 2>/dev/null; then
  fail 'mismatched entrypoint must fail runtime configuration'
fi

reload_publisher
EARLY_SIGNING_DIR="$(mktemp -d "${TMPDIR:-/tmp}/early-signing-XXXXXX")"
umask 077
printf '%s' "$SIGNING_KEY_VALUE" >"$EARLY_SIGNING_DIR/key"
printf '%s' "$SIGNING_PASSWORD_VALUE" >"$EARLY_SIGNING_DIR/password"
export JMODGUARD_SIGNING_SECRETS_DIR="$EARLY_SIGNING_DIR"
JMODGUARD_UPDATE_SIGNING_KEY="$SIGNING_KEY_VALUE"
JMODGUARD_UPDATE_SIGNING_PASSWORD="$SIGNING_PASSWORD_VALUE"
if exercise_publish 1 >/dev/null 2>&1; then
  fail 'pre-staged signing secrets on disk must fail before image build'
fi
printf '%s\n' "${PUBLISH_LAST_ERROR:-}" | grep -q 'staged on disk' \
  || fail 'pre-staged signing secrets must fail with a staged-on-disk error'
rm -rf "$EARLY_SIGNING_DIR"
unset JMODGUARD_SIGNING_SECRETS_DIR

reload_publisher
ROLLBACK_IMAGE=ghcr.io/jatmn/jmodguard-bot
ROLLBACK_DIGEST="sha256:$(printf 'b%.0s' {1..64})"
ROLLBACK_PREV="sha256:$(printf 'a%.0s' {1..64})"
declare -A ROLLBACK_TAG_DIGEST=()
ROLLBACK_PROMOTES=0
ROLLBACK_ROLLBACKS=0
run_docker() {
  local prev="" arg ref digest
  if [[ "$1 $2 $3" == 'buildx imagetools create' ]]; then
    for arg in "$@"; do
      if [[ "$prev" == --tag ]]; then
        ref="$arg"
      fi
      if [[ "$arg" == *@sha256:* ]]; then
        digest="${arg#*@}"
      fi
      prev="$arg"
    done
    ROLLBACK_TAG_DIGEST["$ref"]="$digest"
    if [[ "$ref" == "${ROLLBACK_IMAGE}:latest" ]]; then
      ROLLBACK_PROMOTES=$((ROLLBACK_PROMOTES + 1))
      ROLLBACK_TAG_DIGEST["$ref"]="$ROLLBACK_PREV"
      return 0
    fi
    ROLLBACK_PROMOTES=$((ROLLBACK_PROMOTES + 1))
    return 0
  fi
  if [[ "$1 $2" == 'buildx imagetools' ]]; then
    if [[ "$3" == inspect && "$*" == *'{{json .Manifest}}'* ]]; then
      ref="$4"
      digest="${ROLLBACK_TAG_DIGEST[$ref]:-$ROLLBACK_DIGEST}"
      printf '{"digest":"%s","size":10}\n' "$digest"
      return 0
    fi
    if [[ "$*" == *'{{json .Image}}'* ]]; then
      printf '%s' '{"config":{"Labels":{"io.jmodguard.published-at":"2026-10-27T12:00:00Z","org.opencontainers.image.revision":"abc"},"Entrypoint":["/bin/sh","/usr/local/bin/jmodguard-bootstrap"]}}'
      return 0
    fi
  fi
  return 0
}
registry_promote_tag() {
  run_docker buildx imagetools create "$@" || return 1
}
ROLLBACK_TAG_DIGEST["${ROLLBACK_IMAGE}:${HEAD_SHA}"]="$ROLLBACK_PREV"
PROMOTION_ROLLBACK=()
promote_image_tag "$ROLLBACK_IMAGE" "${HEAD_SHA}" "$ROLLBACK_DIGEST" \
  || fail 'first promotion must succeed'
ROLLBACK_TAG_DIGEST["${ROLLBACK_IMAGE}:${HEAD_SHA}"]="$ROLLBACK_DIGEST"
[[ "${#PROMOTION_ROLLBACK[@]}" -eq 1 ]] || fail 'first promotion must record rollback state'
if promote_image_tag "$ROLLBACK_IMAGE" latest "$ROLLBACK_DIGEST"; then
  fail 'second promotion must fail when digest verification mismatches'
fi
rollback_promoted_tags
[[ "${ROLLBACK_TAG_DIGEST[${ROLLBACK_IMAGE}:${HEAD_SHA}]:-}" == "$ROLLBACK_PREV" ]] \
  || fail 'rollback must restore the first promoted tag to its previous digest'
[[ "$ROLLBACK_PROMOTES" -ge 2 ]] || fail 'rollback test must exercise two promotions'

reload_publisher
SIGNING_PUBLIC_KEY="$TEST_SIGNING_PUBLIC_KEY"
export SIGNING_PUBLIC_KEY
signature_skip_image='ghcr.io/jatmn/jmodguard-api'
inspect_digest() {
  [[ "$1" == "${signature_skip_image}:latest" ]] && printf '%s\t' "$MOCK_BUILD_DIGEST"
}
image_published_at() { printf '%s' '2026-10-01T00:00:00Z'; }
not_after_from_published_at() { printf '%s' '2027-12-31T00:00:00Z'; }
published_signature_still_valid "$signature_skip_image" \
  && fail 'unsigned registry image must fail signature revalidation with cosign available'
missing_cosign_path="$(mktemp -d "${TMPDIR:-/tmp}/publish-no-cosign-XXXXXX")"
if (
  export SIGNING_PUBLIC_KEY="$TEST_SIGNING_PUBLIC_KEY"
  PATH="$missing_cosign_path"
  JMODGUARD_PUBLISH_IMAGE_LIB=1 source "$root/scripts/publish-image.sh"
  published_signature_still_valid "$signature_skip_image"
); then
  fail 'missing cosign must fail closed instead of skipping signature revalidation'
fi
rm -rf "$missing_cosign_path"

cleanup_test_signing
trap - EXIT
eval "${TEST_SIGNING_TRAP#trap -- }" 2>/dev/null || true

echo 'publish-image-test: ok'
