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
contains_arg "ghcr.io/jatmn/jmodguard-bot:nightly" "${nightly_args[@]}" \
  && fail 'Nightly pointer must move only after verification'

readarray -d '' -t stable_args < <(build_args bot "$HEAD_SHA" v1.2.3 . stable)
contains_arg "ghcr.io/jatmn/jmodguard-bot:latest" "${stable_args[@]}" \
  || fail 'stable must move latest'
contains_arg "ghcr.io/jatmn/jmodguard-bot:v1.2.3" "${stable_args[@]}" \
  || fail 'stable must tag the release'
contains_arg "ghcr.io/jatmn/jmodguard-bot:stable" "${stable_args[@]}" \
  && fail 'Stable pointer must move only after verification'

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

reload_publisher() {
  # shellcheck disable=SC1091
  JMODGUARD_PUBLISH_IMAGE_LIB=1 source "$root/.buildkite/publish-image.sh"
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
PUBLISH_SHA="$HEAD_SHA"
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
    printf 'verify %s %s\n' "$1" "$2" >>"$TRACE_FILE"
  }

  run_git() {
    printf 'git %s\n' "$*" >>"$TRACE_FILE"
    case "$1" in
      rev-parse) printf '%s\n' "$HEAD_SHA" ;;
      status) printf '' ;;
      worktree)
        if [[ "$2" == add ]]; then
          mkdir -p "$4"
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
  run_bk() {
    printf 'buildkite-agent %s\n' "$*" >>"$TRACE_FILE"
    case "$1 $2" in
      'meta-data get') printf '%s\n' "$PLAN_JSON" ;;
      'redactor add') cat >"${TRACE_FILE}.redactor"; return 0 ;;
      'secret get') fail 'skip path must not read secrets' ;;
      *) fail "unexpected buildkite $*" ;;
    esac
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
  local mode="${3:-main}"
  local source_token="${4:-}"
  local plan="${5:-}"
  local ref="" sha="$HEAD_SHA"
  with_main
  GHCR_USERNAME=publisher
  GHCR_TOKEN=test-secret
  GITHUB_TOKEN="$source_token"
  export GHCR_USERNAME GHCR_TOKEN GITHUB_TOKEN
  if [[ "$mode" == manual ]]; then
    ref='refs/tags/v1'
    sha="$BASE_SHA"
    BUILDKITE_SOURCE=ui
    JMODGUARD_PUBLISH_SOURCE="$ref"
    export BUILDKITE_SOURCE JMODGUARD_PUBLISH_SOURCE
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
    [[ "$mode" == manual ]] || fail 'resolve_source only for manual'
    assert_eq "$1" manual
    assert_eq "$2" "$BASE_SHA"
    assert_eq "$3" "$HEAD_SHA"
    printf '%s\t\n' "$BASE_SHA"
  }
  if [[ "$FAIL_BUILD" == 1 ]]; then
    if ( publish_component api ) >/dev/null 2>&1; then
      fail 'failed build must not succeed'
    fi
  else
    publish_component api
  fi
  load_trace
  [[ -z "$(ls -A "$scratch" 2>/dev/null || true)" ]] || fail "scratch leftover: $(ls -A "$scratch")"
  rm -rf "$scratch"
  TMPDIR="$saved_tmp"
  export TMPDIR
}

reload_publisher
exercise_publish 0
printf '%s\n' "${CALLS[@]}" | grep -q 'docker buildx build' && fail 'unchanged main must not build'
printf '%s\n' "${CALLS[@]}" | grep -q 'git worktree' && fail 'unchanged main must not use a worktree'
printf '%s\n' "${CALLS[@]}" | grep -q 'secret get' && fail 'unchanged main must not read secrets'
printf '%s\n' "${CALLS[@]}" | grep -q 'docker login' && fail 'unchanged main must not log in'
printf '%s\n' "${CALLS[@]}" | grep -q 'redactor add' && fail 'unchanged main must not redact'

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
printf '%s\n' "${CALLS[@]}" | grep -q 'buildkite-agent meta-data get' \
  || fail 'skip without credentials must still read the plan'
printf '%s\n' "${CALLS[@]}" | grep -qE 'docker |secret get|redactor' \
  && fail "skip without credentials must not touch docker or secrets: ${CALLS[*]}"

exercise_publish 1
printf '%s\n' "$BUILD_LINE" | grep -q -- '--provenance=false' || fail 'build must disable provenance'
printf '%s\n' "$BUILD_LINE" | grep -q -- '--sbom=false' || fail 'build must disable sbom'
printf '%s\n' "$BUILD_LINE" | grep -q 'linux/amd64' || fail 'build must be linux/amd64'
printf '%s\n' "$BUILD_LINE" | grep -q "ghcr.io/jatmn/jmodguard-api:${HEAD_SHA}" || fail 'build must tag the SHA'
printf '%s\n' "$BUILD_LINE" | grep -q 'ghcr.io/jatmn/jmodguard-api:latest' || fail 'main build must move latest'
[[ "$GITHUB_TOKEN_SEEN" == test-secret ]] || fail "build secret must fall back to GHCR token, got '$GITHUB_TOKEN_SEEN'"
printf '%s\n' "${CALLS[@]}" | grep -q 'git worktree remove' || fail 'publish must remove the worktree'
printf '%s\n' "${CALLS[@]}" | grep -q 'test-secret' && fail 'secret must not appear on argv'
[[ "$LOGIN_INPUT" == test-secret ]] || fail 'login must read the token from stdin'
[[ "$(REDACTOR_INPUT="$REDACTOR_INPUT" node --input-type=commonjs - <<'JS'
const data = JSON.parse(process.env.REDACTOR_INPUT);
process.stdout.write(data.GITHUB_TOKEN || "");
JS
)" == test-secret ]] || fail 'redactor must receive the source token'

exercise_publish 1 1
printf '%s\n' "${CALLS[@]}" | grep -q 'git worktree remove' || fail 'failed build must still remove the worktree'

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
run_bk() {
  case "$1 $2" in
    'meta-data get') printf '%s\n' "$PLAN_JSON" ;;
    'redactor add') cat >/dev/null; return 0 ;;
    *) fail "unexpected buildkite $*" ;;
  esac
}
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
  run_bk() { printf '%s\n' "$PLAN_JSON"; }
  if ( read_plan main "" "$HEAD_SHA" ) >/dev/null 2>&1; then
    fail "invalid plan must be rejected: $plan"
  fi
done

reload_publisher
digest="$(printf 'a%.0s' {1..64})"
run_docker() { printf '%s' "{\"digest\":\"sha256:${digest}\",\"size\":9}"; }
got="$(inspect_digest "ghcr.io/jatmn/jmodguard-bot:${HEAD_SHA}")"
assert_eq "$got" "$(printf 'sha256:%s\t%s' "$digest" 9)"

reload_publisher
run_git() {
  case "$1" in
    status|rev-parse)
      if [[ "$1" == status ]]; then printf ''; else printf '%s\n' "$HEAD_SHA"; fi
      ;;
    fetch) return 0 ;;
    *) fail "unexpected git $*" ;;
  esac
}
run_docker() { fail 'manual SHA manifest must not use docker'; }
run_bk() { fail 'manual SHA manifest must not use buildkite'; }
read_plan() { printf '%s\t\n' "$HEAD_SHA"; }
resolve_source() { printf '%s\t\n' "$HEAD_SHA"; }
with_main
BUILDKITE_SOURCE=ui
JMODGUARD_PUBLISH_SOURCE="$HEAD_SHA"
GHCR_USERNAME=jatmn
export BUILDKITE_SOURCE JMODGUARD_PUBLISH_SOURCE GHCR_USERNAME
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

for path in \
  crates/bot/src/main.rs \
  crates/api/src/web/api.rs \
  crates/runtime/src/health.rs \
  migrations/guild/0001_identity.sql \
  migrations/0012_guild_moves.sql
do
  mkdir -p "$(dirname "$path")"
  echo changed >"$path"
  git add .
  git commit -m "$path" >/dev/null
  head="$(git rev-parse HEAD)"
  previous="$(git rev-parse HEAD~1)"
  INSPECT_REVISION="$previous"
  registry_inspect() {
    local ref="$1"
    shift
    if [[ "$ref" == image:latest && "$*" == *--format* ]]; then
      printf '%s' "$(oci_config "$INSPECT_REVISION")"
      return 0
    fi
    return 1
  }
  if ! changed_since_publication bot "$head" image; then
    fail "bot must rebuild after $path"
  fi
  if ! changed_since_publication api "$head" image; then
    fail "api must rebuild after $path"
  fi
done

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
BUILDKITE_COMMIT="$base"
HEAD_SHA="$base"
GHCR_USERNAME=fixture
GHCR_TOKEN=fixture-token
GITHUB_TOKEN=""
export BUILDKITE_COMMIT GHCR_USERNAME GHCR_TOKEN GITHUB_TOKEN
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
      if [[ "$2" == add ]]; then mkdir -p "$4"
      elif [[ "$2" == remove ]]; then rm -rf "$4"; fi
      ;;
    *) git "$@" ;;
  esac
}
run_bk() {
  case "$1 $2" in
    'meta-data get') printf '%s\n' "$PLAN_JSON" ;;
    'redactor add') cat >/dev/null; return 0 ;;
    *) fail "unexpected buildkite $*" ;;
  esac
}
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
EXPORTS=0
PLAN_JSON="$(plan_json main "" "$base" "$base" "")"
export PLAN_JSON
with_main
BUILDKITE_COMMIT="$base"
BUILDKITE_RETRY_COUNT=1
HEAD_SHA="$base"
GHCR_USERNAME=fixture
GHCR_TOKEN=fixture-token
GITHUB_TOKEN=""
export BUILDKITE_COMMIT BUILDKITE_RETRY_COUNT GHCR_USERNAME GHCR_TOKEN GITHUB_TOKEN
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
      if [[ "$2" == add ]]; then mkdir -p "$4"
      elif [[ "$2" == remove ]]; then rm -rf "$4"; fi
      ;;
    *) git "$@" ;;
  esac
}
run_bk() {
  case "$1 $2" in
    'meta-data get') printf '%s\n' "$PLAN_JSON" ;;
    'redactor add') cat >/dev/null; return 0 ;;
    *) fail "unexpected buildkite $*" ;;
  esac
}
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

echo 'publish-image-test: ok'
