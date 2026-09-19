#!/usr/bin/env bash
# Publish one GHCR image from a trusted Buildkite build, without a GHA runner.
set -euo pipefail

REPOSITORY='https://github.com/jatmn/jModGuard'
SHA_RE='^[0-9a-f]{40}$'
DOCKER_TAG_RE='^[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}$'

publish_fail() {
  echo "Image publication failed: $*" >&2
  exit 1
}

run_git() { git "$@" || publish_fail "git failed: $*"; }
run_docker() { docker "$@" || publish_fail "docker failed: $*"; }
run_bk() { buildkite-agent "$@" || publish_fail "buildkite-agent failed: $*"; }

bk_meta_exists() {
  buildkite-agent meta-data exists image-publication >/dev/null
}

publish_request() {
  if [[ "${BUILDKITE:-}" != "true" ]]; then
    publish_fail 'Image publication must run in Buildkite'
  fi
  if [[ "${BUILDKITE_PULL_REQUEST:-false}" != "false" ]]; then
    echo skip
    return 0
  fi
  local source="${BUILDKITE_SOURCE:-}"
  local branch="${BUILDKITE_BRANCH:-}"
  local tag="${BUILDKITE_TAG:-}"
  local manual="${JMODGUARD_PUBLISH_SOURCE:-}"
  if [[ "$source" == "ui" || "$source" == "api" ]]; then
    if [[ -z "$manual" ]]; then
      echo skip
      return 0
    fi
    if [[ "$branch" != "main" || -n "$tag" ]]; then
      publish_fail 'Manual publication requires a UI/API build on main'
    fi
    if [[ ! "$manual" =~ $SHA_RE && "$manual" != refs/tags/* ]]; then
      publish_fail 'JMODGUARD_PUBLISH_SOURCE must be a full lowercase SHA or refs/tags/<tag>'
    fi
    printf 'manual\t%s\n' "$manual"
    return 0
  fi
  if [[ -n "$tag" && "$source" == "webhook" ]]; then
    printf 'tag\t%s\n' "refs/tags/${tag}"
    return 0
  fi
  if [[ "$source" == "schedule" && "$branch" == "main" && -z "$tag" ]]; then
    printf 'nightly\t\n'
    return 0
  fi
  if [[ "$source" == "webhook" && "$branch" == "main" && -z "$tag" ]]; then
    if [[ "${BUILDKITE_GITHUB_EVENT:-push}" != "push" ]]; then
      echo skip
      return 0
    fi
    printf 'main\t\n'
    return 0
  fi
  echo skip
}

stable_tag() {
  [[ "${1:-}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]
}

channel_for() {
  local mode="$1"
  local tag="${2:-}"
  tag="${tag#refs/tags/}"
  if [[ "$mode" == "nightly" ]]; then
    echo nightly
    return 0
  fi
  if [[ "$mode" == "tag" || "$mode" == "manual" ]] && stable_tag "$tag"; then
    echo stable
    return 0
  fi
  return 1
}

nightly_label() {
  local sha="$1"
  [[ "$sha" =~ $SHA_RE ]] || publish_fail 'nightly source must be a full commit SHA'
  printf 'nightly-%s-%s\n' "$(date -u +%Y%m%d)" "${sha:0:12}"
}

resolve_source() {
  local mode="$1"
  local ref="$2"
  local head="$3"
  local sha label
  if [[ "$mode" == "main" ]]; then
    printf '%s\t\n' "$head"
    return 0
  fi
  if [[ "$ref" == refs/tags/* ]]; then
    run_git check-ref-format "$ref" >/dev/null
    run_git fetch --no-tags origin "$ref" >/dev/null
    sha="$(run_git rev-parse 'FETCH_HEAD^{commit}')"
    label="${ref#refs/tags/}"
    label="${label//\//-}"
    label="${label//+/-}"
    if [[ ! "$label" =~ $DOCKER_TAG_RE ]]; then
      echo 'Tag cannot be used as a Docker tag; publishing latest and SHA only' >&2
      label=""
    fi
    if [[ "$mode" == "tag" && "$sha" != "$head" ]]; then
      publish_fail 'Tag moved after the Buildkite checkout; create a new build'
    fi
  else
    [[ "$ref" =~ $SHA_RE ]] || publish_fail 'Expected an immutable full commit SHA'
    run_git fetch --no-tags origin "$ref" >/dev/null
    sha="$(run_git rev-parse 'FETCH_HEAD^{commit}')"
    if [[ "$sha" != "$ref" ]]; then
      publish_fail 'Fetched commit differs from requested SHA'
    fi
    label=""
  fi
  [[ "$sha" =~ $SHA_RE ]] || publish_fail 'Source did not resolve to a full commit SHA'
  printf '%s\t%s\n' "$sha" "$label"
}

oci_revision() {
  IMAGE_JSON="${1:-}" node --input-type=commonjs - <<'JS'
const data = (() => {
  try {
    return JSON.parse(process.env.IMAGE_JSON || "");
  } catch {
    process.exit(1);
  }
})();
const labels = (data && data.config && data.config.Labels) || {};
const revision = labels["org.opencontainers.image.revision"] || "";
const source = labels["org.opencontainers.image.source"] || "";
process.stdout.write(`${revision}\n${source}\n`);
JS
}

# Skip-path helpers must not publish_fail. Absent history, registry failures,
# unknown git objects and foreign manifests require a rebuild.
registry_inspect() {
  docker buildx imagetools inspect "$@"
}

skip_source_diff() {
  git diff --name-only "$@"
}

changed_since_publication() {
  local component="$1"
  local sha="$2"
  local image="$3"
  local config previous source latest immutable names rev
  if ! config="$(registry_inspect "${image}:latest" --format '{{json .Image}}')"; then
    return 0
  fi
  if ! rev="$(oci_revision "$config")"; then
    return 0
  fi
  IFS= read -r previous <<<"${rev%%$'\n'*}"
  IFS= read -r source <<<"${rev#*$'\n'}"
  [[ "$source" == "$REPOSITORY" && "$previous" =~ $SHA_RE ]] || return 0
  if ! names="$(skip_source_diff "$previous" "$sha" -- \
      "Dockerfile.${component}" Cargo.toml Cargo.lock rust-toolchain.toml src crates migrations vendor .buildkite)"; then
    return 0
  fi
  if [[ -n "$names" ]]; then
    return 0
  fi
  latest="$(registry_inspect "${image}:latest" --raw)" || return 0
  immutable="$(registry_inspect "${image}:${previous}" --raw)" || return 0
  [[ -z "${latest//[[:space:]]/}" || "$latest" != "$immutable" ]]
}

build_args() {
  local component="$1"
  local sha="$2"
  local label="$3"
  local context="$4"
  local channel="${5:-}"
  local image="ghcr.io/jatmn/jmodguard-${component}"
  local -a args=(
    docker buildx build --builder default
    --file "${context}/Dockerfile.${component}"
    --platform linux/amd64 --provenance=false --sbom=false
    --push --progress plain
    --cache-from "type=registry,ref=${image}:latest"
    --cache-to type=inline
    --secret id=github_token,env=GITHUB_TOKEN
    --label "org.opencontainers.image.source=${REPOSITORY}"
    --label "org.opencontainers.image.revision=${sha}"
    --label "org.opencontainers.image.version=${label:-$sha}"
    --tag "${image}:${sha}"
  )
  if [[ "$channel" != "nightly" ]]; then
    args+=(--tag "${image}:latest")
  fi
  if [[ -n "$label" && "$label" != "latest" && "$label" != "$sha" ]]; then
    args+=(--tag "${image}:${label}")
  fi
  args+=("$context")
  printf '%s\0' "${args[@]}"
}

checked_request() {
  local selection mode ref head
  selection="$(publish_request)"
  if [[ "$selection" == "skip" ]]; then
    echo skip
    return 0
  fi
  mode="${selection%%$'\t'*}"
  ref="${selection#*$'\t'}"
  head="$(run_git rev-parse HEAD)"
  [[ "$head" =~ $SHA_RE && "${BUILDKITE_COMMIT:-}" == "$head" ]] \
    || publish_fail 'Buildkite commit does not match the checked-out full SHA'
  if [[ -n "$(run_git status --porcelain --untracked-files=no)" ]]; then
    publish_fail 'Refusing to publish a checkout with tracked modifications'
  fi
  printf '%s\t%s\t%s\n' "$mode" "$ref" "$head"
}

read_plan() {
  local mode="$1"
  local ref="$2"
  local head="$3"
  local planned check sha label
  planned="$(run_bk meta-data get image-publication)"
  check="$(PLAN_JSON="$planned" PLAN_MODE="$mode" PLAN_REF="$ref" PLAN_HEAD="$head" node --input-type=commonjs - <<'JS'
const planned = JSON.parse(process.env.PLAN_JSON);
const mode = process.env.PLAN_MODE;
const ref = process.env.PLAN_REF;
const head = process.env.PLAN_HEAD;
const keys = ["mode", "ref", "commit", "sha", "tag"];
if (typeof planned !== "object" || planned === null || Array.isArray(planned)) process.exit(2);
for (const key of keys) {
  if (typeof planned[key] !== "string") process.exit(2);
}
if (planned.mode !== mode || planned.ref !== ref || planned.commit !== head) process.exit(2);
if (!/^[0-9a-f]{40}$/.test(planned.sha)) process.exit(2);
if (planned.tag && !/^[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}$/.test(planned.tag)) process.exit(2);
process.stdout.write(`${planned.sha}\t${planned.tag}\n`);
JS
)" || publish_fail 'Invalid image publication plan'
  sha="${check%%$'\t'*}"
  label="${check#*$'\t'}"
  if [[ "$mode" != "manual" && "$sha" != "$head" ]]; then
    publish_fail 'Publication plan differs from the Buildkite commit'
  fi
  printf '%s\t%s\n' "$sha" "$label"
}

prepare() {
  local selection mode ref head exists_rc sha label plan
  selection="$(checked_request)"
  if [[ "$selection" == "skip" ]]; then
    echo 'Test-only build; no image publication'
    return 0
  fi
  mode="${selection%%$'\t'*}"
  local rest="${selection#*$'\t'}"
  ref="${rest%%$'\t'*}"
  head="${rest#*$'\t'}"
  set +e
  bk_meta_exists
  exists_rc=$?
  set -e
  if [[ "$exists_rc" -eq 0 ]]; then
    read_plan "$mode" "$ref" "$head" >/dev/null
    echo 'Reusing the original immutable publication plan'
    return 0
  fi
  if [[ "$exists_rc" -ne 100 ]]; then
    publish_fail 'Could not check existing publication plan'
  fi
  if [[ "$mode" == "nightly" ]]; then
    IFS=$'\t' read -r sha label <<<"$(resolve_source main "$ref" "$head")"
    label="$(nightly_label "$sha")"
  else
    IFS=$'\t' read -r sha label <<<"$(resolve_source "$mode" "$ref" "$head")"
  fi
  plan="$(MODE="$mode" REF="$ref" HEAD_SHA="$head" PLAN_SHA="$sha" PLAN_TAG="$label" node --input-type=commonjs - <<'JS'
process.stdout.write(JSON.stringify({
  mode: process.env.MODE,
  ref: process.env.REF,
  commit: process.env.HEAD_SHA,
  sha: process.env.PLAN_SHA,
  tag: process.env.PLAN_TAG,
}));
JS
)"
  run_bk meta-data set image-publication "$plan"
}

inspect_digest() {
  local raw
  raw="$(run_docker buildx imagetools inspect "$1" --format '{{json .Manifest}}')"
  MANIFEST_JSON="$raw" node --input-type=commonjs - <<'JS'
const data = JSON.parse(process.env.MANIFEST_JSON || "");
const digest = data.digest || (data.Descriptor && data.Descriptor.digest) || "";
const sizeRaw = data.size ?? (data.Descriptor && data.Descriptor.size);
const size = Number(sizeRaw);
if (!/^sha256:[0-9a-f]{64}$/.test(digest)) process.exit(2);
if (!Number.isInteger(size) || size <= 0) process.exit(2);
process.stdout.write(`${digest}\t${size}\n`);
JS
}

release_publish_bin() {
  if [[ -n "${JMODGUARD_RELEASE_PUBLISH_BIN:-}" ]]; then
    printf '%s\n' "$JMODGUARD_RELEASE_PUBLISH_BIN"
    return 0
  fi
  cargo build --locked -p jmodguard-conductor --bin jmodguard-release-publish >&2
  printf '%s\n' "${CARGO_TARGET_DIR:-target}/debug/jmodguard-release-publish"
}

redact_tokens() {
  local token="$1"
  local source_token="$2"
  GHCR_TOKEN="$token" GITHUB_TOKEN_VALUE="$source_token" node --input-type=commonjs - <<'JS' | run_bk redactor add --format json >/dev/null
process.stdout.write(JSON.stringify({
  GHCR_TOKEN: process.env.GHCR_TOKEN,
  GITHUB_TOKEN: process.env.GITHUB_TOKEN_VALUE,
}));
JS
}

scratch_dir() {
  local prefix="$1"
  mkdir -p "${TMPDIR:-.tmp}"
  mktemp -d "${TMPDIR:-.tmp}/${prefix}-XXXXXX"
}

publish_manifest() {
  local selection mode ref head sha label channel release_id key_id secret
  local scratch context out bin components name digest size image path
  selection="$(checked_request)"
  if [[ "$selection" == "skip" ]]; then
    echo 'Test-only build; no release manifest'
    return 0
  fi
  mode="${selection%%$'\t'*}"
  local rest="${selection#*$'\t'}"
  ref="${rest%%$'\t'*}"
  head="${rest#*$'\t'}"
  if [[ "$mode" == "main" ]]; then
    echo 'Main SHA publication is not an installable Stable/Nightly release'
    return 0
  fi
  IFS=$'\t' read -r sha label <<<"$(read_plan "$mode" "$ref" "$head")"
  channel="$(channel_for "$mode" "$label" || true)"
  release_id=""
  if [[ "$channel" == "nightly" ]]; then
    release_id="$label"
  elif [[ "$channel" == "stable" ]] && stable_tag "$label"; then
    release_id="$label"
  fi
  if [[ -z "$release_id" ]]; then
    echo 'Publication source has no installable Stable/Nightly release id; images only (no signed manifest for discovery)'
    return 0
  fi
  key_id="${JMODGUARD_RELEASE_KEY_ID:-}"
  secret="${JMODGUARD_RELEASE_SIGNING_KEY:-}"
  if [[ -z "$secret" ]]; then
    secret="$(run_bk secret get JMODGUARD_RELEASE_SIGNING_KEY)"
  fi
  if [[ -z "$key_id" || -z "$secret" ]]; then
    publish_fail 'complete manifests require JMODGUARD_RELEASE_KEY_ID and signing key'
  fi
  scratch="$(scratch_dir manifest)"
  context="${scratch}/source"
  components="${scratch}/components.json"
  cleanup_manifest() {
    run_git worktree remove --force "$context" >/dev/null 2>&1 || true
    rm -rf "$scratch"
  }
  trap cleanup_manifest EXIT
  run_git worktree add --detach "$context" "$sha" >/dev/null
  bin="$(release_publish_bin)"
  "$bin" frontend-inventory "$context/frontend" >"${scratch}/frontend.json"
  COMPONENTS="$components" FRONTEND_JSON="${scratch}/frontend.json" node --input-type=commonjs - <<'JS'
const fs = require("fs");
const components = { frontend: JSON.parse(fs.readFileSync(process.env.FRONTEND_JSON, "utf8")) };
fs.writeFileSync(process.env.COMPONENTS, JSON.stringify(components));
JS
  for name in bot api conductor; do
    image="ghcr.io/jatmn/jmodguard-${name}:${sha}"
    IFS=$'\t' read -r digest size <<<"$(inspect_digest "$image")"
    NAME="$name" DIGEST="$digest" SIZE="$size" IMAGE="ghcr.io/jatmn/jmodguard-${name}" \
      COMPONENTS="$components" node --input-type=commonjs - <<'JS'
const fs = require("fs");
const path = process.env.COMPONENTS;
const data = JSON.parse(fs.readFileSync(path, "utf8"));
data[process.env.NAME] = {
  digest: process.env.DIGEST,
  platform: "linux/amd64",
  size: Number(process.env.SIZE),
  image: process.env.IMAGE,
};
fs.writeFileSync(path, JSON.stringify(data));
JS
  done
  out="${JMODGUARD_RELEASE_OUT:-dist/releases}"
  export JMODGUARD_RELEASE_SIGNING_KEY="$secret"
  path="$("$bin" finalize \
    --channel "$channel" \
    --release-id "$release_id" \
    --source-commit "$sha" \
    --published-at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    --components-file "$components" \
    --key-id "$key_id" \
    --out "$out")"
  echo "Published installable manifest ${path}"
  trap - EXIT
  cleanup_manifest
}

publish_component() {
  local component="$1"
  if [[ "$component" == "manifest" ]]; then
    publish_manifest
    return 0
  fi
  local selection mode ref head sha label image username token source_token scratch context channel
  local -a build_cmd
  selection="$(checked_request)"
  if [[ "$selection" == "skip" ]]; then
    echo 'Test-only build; no image publication'
    return 0
  fi
  mode="${selection%%$'\t'*}"
  local rest="${selection#*$'\t'}"
  ref="${rest%%$'\t'*}"
  head="${rest#*$'\t'}"
  if [[ "$component" == "conductor" && "$mode" == "main" ]]; then
    echo 'conductor: installable control images are published on Stable/Nightly only'
    return 0
  fi
  IFS=$'\t' read -r sha label <<<"$(read_plan "$mode" "$ref" "$head")"
  if [[ "$mode" == "manual" ]]; then
    resolve_source manual "$sha" "$head" >/dev/null
  fi
  image="ghcr.io/jatmn/jmodguard-${component}"
  if [[ "$mode" == "main" && "${BUILDKITE_RETRY_COUNT:-0}" == "0" ]] \
      && ! changed_since_publication "$component" "$sha" "$image"; then
    echo "${component}: image inputs match the last publication; no rebuild"
    return 0
  fi
  username="${GHCR_USERNAME:-}"
  [[ -n "$username" ]] || publish_fail 'Set GHCR_USERNAME to the GitHub account owning the GHCR credential'
  token="${GHCR_TOKEN:-}"
  if [[ -z "$token" ]]; then
    token="$(run_bk secret get GHCR_TOKEN)"
  fi
  [[ -n "$token" ]] || publish_fail 'Buildkite secret GHCR_TOKEN is empty'
  source_token="${GITHUB_TOKEN:-}"
  if [[ -z "$source_token" || "$source_token" == '$GITHUB_TOKEN' ]]; then
    source_token="$token"
  fi
  redact_tokens "$token" "$source_token"
  export GITHUB_TOKEN="$source_token"
  scratch="$(scratch_dir publish)"
  context="${scratch}/source"
  cleanup_publish() {
    run_git worktree remove --force "$context" >/dev/null 2>&1 || true
    rm -rf "$scratch"
  }
  trap cleanup_publish EXIT
  export DOCKER_CONFIG="${scratch}/docker"
  mkdir -m 700 "$DOCKER_CONFIG"
  printf '%s' "$token" | run_docker login ghcr.io --username "$username" --password-stdin >/dev/null
  run_git worktree add --detach "$context" "$sha" >/dev/null
  channel="$(channel_for "$mode" "$label" || true)"
  readarray -d '' -t build_cmd < <(build_args "$component" "$sha" "$label" "$context" "$channel")
  run_docker "${build_cmd[@]:1}"
  echo "Published ${image}:${sha}"
  trap - EXIT
  cleanup_publish
}

publish_image_main() {
  local component="${1:-}"
  case "$component" in
    prepare) prepare ;;
    bot|api|conductor|manifest) publish_component "$component" ;;
    *)
      echo "usage: bash .buildkite/publish-image.sh prepare|bot|api|conductor|manifest" >&2
      exit 2
      ;;
  esac
}

if [[ "${JMODGUARD_PUBLISH_IMAGE_LIB:-}" == 1 ]]; then
  return 0 2>/dev/null || exit 0
fi
publish_image_main "${1:-}"
