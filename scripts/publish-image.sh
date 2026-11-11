#!/usr/bin/env bash
# Publish a verified GHCR image from a trusted GitHub Actions run.
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
run_cosign() { cosign "$@" || publish_fail "cosign failed: $*"; }

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SIGNING_PUBLIC_KEY="${REPO_ROOT}/deploy/update-signing.pub"
IMAGE_SIGNING_RE='^ghcr\.io/jatmn/jmodguard-(bot|api|webui)$'
_JMG_SIGNING_KEY=""
_JMG_SIGNING_PASSWORD=""
_JMG_SIGNING_KEY_FILE=""
_JMG_SIGNING_PASSWORD_FILE=""

load_signing_secrets_from_staging() {
  local dir="${JMODGUARD_SIGNING_SECRETS_DIR:-}"
  [[ -n "$dir" ]] || return 0
  [[ -f "$dir/key" && -f "$dir/password" ]] \
    || publish_fail 'signing secrets staging directory is incomplete'
  _JMG_SIGNING_KEY="$(<"$dir/key")"
  _JMG_SIGNING_PASSWORD="$(<"$dir/password")"
}

assert_signing_secrets_not_staged_on_disk() {
  local dir="${JMODGUARD_SIGNING_SECRETS_DIR:-}"
  [[ -z "$dir" ]] && return 0
  if [[ -f "$dir/key" || -f "$dir/password" ]]; then
    publish_fail 'signing secrets must not be staged on disk before image build completes'
  fi
}

capture_signing_secrets() {
  if [[ -n "${JMODGUARD_UPDATE_SIGNING_KEY:-}" ]]; then
    _JMG_SIGNING_KEY="$JMODGUARD_UPDATE_SIGNING_KEY"
    _JMG_SIGNING_KEY_FILE=""
    _JMG_SIGNING_PASSWORD_FILE=""
  fi
  if [[ -n "${JMODGUARD_UPDATE_SIGNING_PASSWORD:-}" ]]; then
    _JMG_SIGNING_PASSWORD="$JMODGUARD_UPDATE_SIGNING_PASSWORD"
    _JMG_SIGNING_KEY_FILE=""
    _JMG_SIGNING_PASSWORD_FILE=""
  fi
  unset JMODGUARD_UPDATE_SIGNING_KEY JMODGUARD_UPDATE_SIGNING_PASSWORD
}

ensure_signing_secrets_captured() {
  if [[ -n "$_JMG_SIGNING_KEY" && -n "$_JMG_SIGNING_PASSWORD" ]]; then
    return 0
  fi
  load_signing_secrets_from_staging
}

signing_secrets_in_environ() {
  [[ -n "${JMODGUARD_UPDATE_SIGNING_KEY:-}" || -n "${JMODGUARD_UPDATE_SIGNING_PASSWORD:-}" ]]
}

persist_signing_secrets_to_files() {
  local dir="$1"
  _JMG_SIGNING_KEY_FILE="${dir}/signing.key"
  _JMG_SIGNING_PASSWORD_FILE="${dir}/signing.password"
  printf '%s' "$_JMG_SIGNING_KEY" >"$_JMG_SIGNING_KEY_FILE"
  printf '%s' "$_JMG_SIGNING_PASSWORD" >"$_JMG_SIGNING_PASSWORD_FILE"
  chmod 600 "$_JMG_SIGNING_KEY_FILE" "$_JMG_SIGNING_PASSWORD_FILE"
  _JMG_SIGNING_KEY=""
  _JMG_SIGNING_PASSWORD=""
}

load_signing_secrets_from_files() {
  [[ -f "$_JMG_SIGNING_KEY_FILE" && -f "$_JMG_SIGNING_PASSWORD_FILE" ]] \
    || publish_fail 'signing secret files are missing'
  _JMG_SIGNING_KEY="$(<"$_JMG_SIGNING_KEY_FILE")"
  _JMG_SIGNING_PASSWORD="$(<"$_JMG_SIGNING_PASSWORD_FILE")"
}

publish_staging_ref() {
  local image="$1" sha="$2"
  local run_id="${GITHUB_RUN_ID:-0}"
  printf '%s:_publish-%s-%s' "$image" "${sha:0:12}" "$run_id"
}
read_publication_plan() {
  if [[ -n "${JMODGUARD_PUBLICATION_PLAN:-}" ]]; then
    printf '%s\n' "$JMODGUARD_PUBLICATION_PLAN"
  else
    cat "${JMODGUARD_PUBLICATION_PLAN_FILE:?publication plan path is required}"
  fi
}

publication_plan_exists() {
  [[ -n "${JMODGUARD_PUBLICATION_PLAN:-}" || -f "${JMODGUARD_PUBLICATION_PLAN_FILE:-}" ]]
}

write_publication_plan() {
  local file="${JMODGUARD_PUBLICATION_PLAN_FILE:?publication plan path is required}"
  printf '%s\n' "$1" >"$file"
}

publish_request() {
  [[ "${GITHUB_ACTIONS:-}" == true ]] || publish_fail 'Image publication must run in GitHub Actions'
  [[ "${GITHUB_REPOSITORY:-}" == jatmn/jModGuard ]] || { echo skip; return; }
  local event="${GITHUB_EVENT_NAME:-}" ref="${GITHUB_REF:-}"
  local manual="${JMODGUARD_PUBLISH_SOURCE:-}"
  case "$event" in
    pull_request|pull_request_target|merge_group) echo skip; return ;;
    workflow_dispatch)
      [[ "$ref" == refs/heads/main ]] || publish_fail 'Manual publication requires main'
      [[ "${GITHUB_ACTOR:-}" == jatmn && "${GITHUB_TRIGGERING_ACTOR:-}" == jatmn && "${GITHUB_ACTOR_ID:-}" == 12479882 ]] \
        || publish_fail 'Manual publication requires the repository owner'
      [[ -n "$manual" ]] || { echo skip; return; }
      [[ "$manual" =~ $SHA_RE || "$manual" == refs/tags/* ]] \
        || publish_fail 'JMODGUARD_PUBLISH_SOURCE must be a full lowercase SHA or refs/tags/<tag>'
      printf 'manual\t%s\n' "$manual" ;;
    push)
      if [[ "$ref" == refs/heads/main ]]; then
        if [[ -n "${JMODGUARD_RELEASE_TAG:-}" ]]; then
          stable_tag "$JMODGUARD_RELEASE_TAG" || publish_fail 'Invalid Release Please tag'
          [[ "${JMODGUARD_RELEASE_SHA:-}" =~ $SHA_RE ]] || publish_fail 'Invalid Release Please SHA'
          printf 'release\trefs/tags/%s\n' "$JMODGUARD_RELEASE_TAG"
        else
          printf 'main\t\n'
        fi
      elif [[ "$ref" == refs/tags/* ]]; then
        [[ "${GITHUB_ACTOR:-}" == jatmn && "${GITHUB_TRIGGERING_ACTOR:-}" == jatmn && "${GITHUB_ACTOR_ID:-}" == 12479882 ]] \
          || publish_fail 'Tag publication requires the repository owner'
        printf 'tag\t%s\n' "$ref"
      else
        echo skip
      fi ;;
    schedule)
      [[ "$ref" == refs/heads/main ]] && printf 'nightly\t\n' || echo skip ;;
    *) echo skip ;;
  esac
}

stable_tag() {
  [[ "${1:-}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]
}

# Status 1 means no Stable/Nightly channel; callers intentionally accept it.
channel_for() {
  local mode="$1"
  local tag="${2:-}"
  tag="${tag#refs/tags/}"
  if [[ "$mode" == "nightly" ]]; then
    echo nightly
    return 0
  fi
  if [[ "$mode" == "tag" || "$mode" == "manual" || "$mode" == "release" ]] && stable_tag "$tag"; then
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
      publish_fail 'Tag moved after the workflow checkout; create a new build'
    fi
    if [[ "$mode" == "release" && "$sha" != "${JMODGUARD_RELEASE_SHA:-}" ]]; then
      publish_fail 'Tag differs from the Release Please source SHA'
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
  # COPY crates/ supplies Cargo workspace layout, not a production dependency
  # on every service. Keep every manifest (resolution), but only linked source.
  local -a inputs=(
    "Dockerfile.${component}" "Dockerfile.${component}.dockerignore" .dockerignore LICENSE.md
    Cargo.toml Cargo.lock rust-toolchain.toml ':(glob)crates/*/Cargo.toml'
    src migrations vendor "crates/${component}" crates/runtime crates/updater
    scripts/publish-image.sh scripts/bootstrap-container.sh
    ':(exclude,glob)crates/*/tests/**'
    ':(exclude,glob)crates/*/benches/**'
    ':(exclude,glob)crates/*/examples/**'
  )
  if [[ "$component" == "webui" ]]; then
    inputs=(Dockerfile.webui Dockerfile.webui.dockerignore .dockerignore LICENSE.md
      frontend tools/webui scripts/package-webui.mjs scripts/publish-image.sh src/db.rs src/db/update_control.rs migrations/guild/current_version.txt)
  fi
  if ! names="$(skip_source_diff "$previous" "$sha" -- "${inputs[@]}")"; then
    return 0
  fi
  if [[ -n "$names" ]]; then
    return 0
  fi
  latest="$(registry_inspect "${image}:latest" --raw)" || return 0
  immutable="$(registry_inspect "${image}:${previous}" --raw)" || return 0
  if [[ -z "${latest//[[:space:]]/}" || "$latest" != "$immutable" ]]; then
    return 0
  fi
  if ! published_signature_still_valid "$image"; then
    return 0
  fi
  return 1
}

published_signature_still_valid() {
  local image="$1" digest published_at not_after
  command -v cosign >/dev/null || return 1
  [[ -f "$SIGNING_PUBLIC_KEY" ]] || return 1
  if ! digest="$(inspect_digest "${image}:latest" 2>/dev/null | cut -f1)"; then
    return 1
  fi
  if ! cosign verify \
      --key "$SIGNING_PUBLIC_KEY" \
      --insecure-ignore-tlog=true \
      "${image}@${digest}" >/dev/null 2>&1; then
    return 1
  fi
  if ! published_at="$(image_published_at "${image}@${digest}" 2>/dev/null)"; then
    return 1
  fi
  if ! not_after="$(not_after_from_published_at "$published_at" 2>/dev/null)"; then
    return 1
  fi
  NOT_AFTER="$not_after" node --input-type=commonjs - <<'JS' || return 1
const deadline = new Date(process.env.NOT_AFTER || "");
if (Number.isNaN(deadline.getTime())) process.exit(2);
if (Date.now() >= deadline.getTime()) process.exit(3);
JS
  return 0
}

build_args() {
  local component="$1"
  local sha="$2"
  local label="$3"
  local context="$4"
  local channel="${5:-}"
  local layout="${6:-current}"
  local staging_ref="${7:?staging image reference is required}"
  local image="ghcr.io/jatmn/jmodguard-${component}"
  local release_channel="${channel:-development}"
  local published_at
  published_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
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
  )
  # Old manual-source images can be dashboard rollback images, but they have
  # no launcher and must not appear compatible with the owner updater.
  if [[ "$layout" != "legacy" ]]; then
    # Startup requires the selected source's completed core migrations. Read
    # that source, including for historical publication, rather than labeling
    # a migrating binary with the publisher's former schema version.
    local schema
    schema="$(node - "$context/src/db.rs" <<'JS'
const fs = require("node:fs");
const source = fs.readFileSync(process.argv[2], "utf8");
const migrations = source.match(/const MIGRATIONS: &\[&str\] = &\[([\s\S]*?)\];/);
const count = migrations?.[1].match(/include_str!\("\.\.\/migrations\/\d{4}_[^"\n]+\.sql"\)/g)?.length;
if (!count || count < 15) throw new Error("selected source has no supported updater schema");
process.stdout.write(String(count));
JS
)" || publish_fail 'cannot determine selected source core schema'
    args+=(
      --label "io.jmodguard.component=${component}"
      --label "io.jmodguard.channel=${release_channel}"
      --label "io.jmodguard.release-id=${label:-$sha}"
      --label "io.jmodguard.published-at=${published_at}"
      --label "io.jmodguard.min-schema=${schema}"
      --label "io.jmodguard.max-schema=${schema}"
      --label "io.jmodguard.result-schema=${schema}"
      --label "io.jmodguard.min-api=1"
      --label "io.jmodguard.max-api=1"
      --label "io.jmodguard.base-abi=$([[ "$component" == webui ]] && echo static-webui-v1 || echo debian-bookworm-amd64)"
    )
    if [[ -f "$context/src/db/update_control.rs" ]]; then
      local guild_schema control_protocol launcher_revision min_launcher_revision
      read -r control_protocol launcher_revision min_launcher_revision < <(node - "$context/src/db/update_control.rs" <<'JS'
const fs = require("node:fs");
const source = fs.readFileSync(process.argv[2], "utf8");
function number(name, fallback) {
  const match = source.match(new RegExp(`pub const ${name}: u32 = ([0-9]+);`));
  if (!match && fallback === undefined) throw new Error(`missing ${name}`);
  return match ? Number(match[1]) : fallback;
}
const control = number("UPDATE_CONTROL_PROTOCOL");
const revision = number("LAUNCHER_REVISION", 0);
const minimum = number("REQUIRED_LAUNCHER_REVISION", 0);
if (control >= 3 && (!minimum || revision < minimum)) throw new Error("invalid launcher capability contract");
console.log(control, revision, minimum);
JS
) || publish_fail 'cannot determine selected source launcher contract'
      if [[ "$component" != webui && "$control_protocol" -ge 3 ]]; then
        args+=(--label "io.jmodguard.launcher-revision=${launcher_revision}"
          --label "io.jmodguard.min-launcher-revision=${min_launcher_revision}")
      fi
      guild_schema="$(cat "$context/migrations/guild/current_version.txt")"
      [[ "$guild_schema" =~ ^[1-9][0-9]*$ ]] || publish_fail 'invalid selected guild schema'
      args+=(
        --label "io.jmodguard.update-control=${control_protocol}"
        --label "io.jmodguard.upgrade-min-core=15"
        --label "io.jmodguard.upgrade-min-guild=1"
        --label "io.jmodguard.guild-schema=${guild_schema}"
      )
    fi
    # Historical schema16 images still contain the schema15-reporting launcher.
    # They remain dashboard artifacts, but must not reinstall that launcher on
    # a repaired deployment. Without protocol metadata discovery rejects them.
    if [[ "$component" != webui && "$schema" != 15 ]] \
        && grep -Eq '(min|max)_schema: 15,' "$context/crates/updater/src/main.rs"; then
      echo 'Selected source has stale launcher schema observations; owner updates unavailable' >&2
    else
      args+=(--label "io.jmodguard.launcher-protocol=1")
    fi
  else
    # Dashboard rollback images are signed but must not advertise updater metadata.
    args+=(--label "io.jmodguard.published-at=${published_at}")
  fi
  args+=(--build-arg "SOURCE_SHA=${sha}" --tag "$staging_ref")
  args+=("$context")
  printf '%s\0' "${args[@]}"
}

digest_from_build_metadata() {
  local metadata_file="$1"
  METADATA_FILE="$metadata_file" node --input-type=commonjs - <<'JS'
const fs = require("node:fs");
let data;
try {
  data = JSON.parse(fs.readFileSync(process.env.METADATA_FILE, "utf8"));
} catch {
  process.exit(2);
}
const digest = data["containerimage.digest"] || data.containerimage?.digest || "";
if (!/^sha256:[0-9a-f]{64}$/.test(digest)) process.exit(3);
process.stdout.write(digest);
JS
}

digest_matches_ref() {
  local ref="$1" expected_digest="$2"
  local resolved
  IFS=$'\t' read -r resolved _ <<<"$(inspect_digest "$ref")"
  [[ "$resolved" == "$expected_digest" ]]
}

assert_digest_matches_ref() {
  local ref="$1" expected_digest="$2" resolved
  if digest_matches_ref "$ref" "$expected_digest"; then
    return 0
  fi
  IFS=$'\t' read -r resolved _ <<<"$(inspect_digest "$ref")"
  publish_fail "registry reference ${ref} points at ${resolved}, expected ${expected_digest}"
}

verify_image_runtime_configuration() {
  local component="$1" ref="$2" layout="${3:-current}"
  local raw expected
  raw="$(run_docker buildx imagetools inspect "$ref" --format '{{json .Image}}')"
  if [[ "$layout" == "legacy" ]]; then
    expected='[]'
  elif [[ "$component" == "webui" ]]; then
    expected='[]'
  else
    expected='["/bin/sh","/usr/local/bin/jmodguard-bootstrap"]'
  fi
  IMAGE_JSON="$raw" EXPECTED_ENTRYPOINT="$expected" LAYOUT="$layout" node --input-type=commonjs - <<'JS'
const data = JSON.parse(process.env.IMAGE_JSON || "{}");
const labels = (data.config && data.config.Labels) || (data.Config && data.Config.Labels) || {};
const entrypoint = (data.config && data.config.Entrypoint) || (data.Config && data.Config.Entrypoint) || [];
const expected = JSON.parse(process.env.EXPECTED_ENTRYPOINT || "[]");
if (JSON.stringify(entrypoint) !== JSON.stringify(expected)) process.exit(2);
if (process.env.LAYOUT !== "legacy" && !labels["org.opencontainers.image.revision"]) process.exit(3);
JS
}

declare -g -a PROMOTION_QUEUE=()
declare -g -a PROMOTION_DONE=()
declare -g -a PROMOTION_SCRATCHES=()
declare -g -a PROMOTION_ROLLBACK=()

registry_promote_tag() {
  docker buildx imagetools create "$@" || return 1
}

registry_user_versions_path() {
  printf '/users/jatmn/packages/container/%s/versions' "$1"
}

registry_fetch_versions() {
  local package="$1" token="$2"
  # gh api --paginate prints one JSON array per page. Combine them before matching.
  # Bound the entire scan; timeout exits nonzero on expiry, and pipefail propagates
  # that failure even if jq successfully combines the partial pages.
  GH_PROMPT_DISABLED=1 GH_TOKEN="$token" timeout --kill-after=5s 60s gh api --paginate \
    "$(registry_user_versions_path "$package")?per_page=100" \
    | jq -se '
        if any(.[]; type != "array") then
          error("package version list page is not a JSON array")
        else
          [ .[][] ]
        end
      '
}

registry_delete_version() {
  local package="$1" token="$2" version_id="$3"
  GH_PROMPT_DISABLED=1 GH_TOKEN="$token" gh api --method DELETE \
    "$(registry_user_versions_path "$package")/${version_id}" >/dev/null
}

registry_classify_tag_version() {
  local tag="$1"
  jq -r --arg tag "$tag" '
    [ .[] | select((.metadata.container.tags // []) | index($tag) != null) ]
    | if length == 1 then
        if (.[0].metadata.container.tags | length) == 1 then
          "delete \(.[0].id)"
        else
          "untag \(.[0].id)"
        end
      elif length == 0 then
        "none"
      else
        "many \(length)"
      end
  '
}

registry_disposable_version_id() {
  local tag="$1" keep_id="$2"
  jq -r --arg tag "$tag" --argjson keep "$keep_id" '
    (map(select(.id == $keep)) | first) as $kept
    | if $kept == null then "kept-missing"
      elif (($kept.metadata.container.tags // []) | index($tag) != null) then "tag-still-present"
      else
        [ .[] | select(.id != $keep and ((.metadata.container.tags // []) == [$tag])) ]
        | if length == 1 then "delete \(.[0].id)"
          elif length == 0 then "none"
          else "many \(length)"
          end
      end
  '
}

registry_preserved_digest() {
  local version_id="$1"
  jq -er --argjson id "$version_id" '
    ([.[] | select(.id == $id)] | first | .name // empty)
    | select(test("^sha256:[0-9a-f]{64}$"))
  '
}

registry_move_tag_onto_empty_image() {
  local image_tag="$1" dir
  dir="$(mktemp -d)" || return 1
  printf 'FROM scratch\n' >"${dir}/Dockerfile"
  if ! docker buildx build --builder default --platform linux/amd64 \
    --provenance=false --sbom=false --push --progress plain \
    --file "${dir}/Dockerfile" --tag "$image_tag" "$dir"; then
    rm -rf "$dir"
    return 1
  fi
  rm -rf "$dir"
}

registry_delete_detached_tag() {
  local package="$1" token="$2" tag="$3" keep_id="$4" digest="$5"
  local payload decision attempt disposable_id image_tag
  decision="unreadable"
  for attempt in 1 2 3; do
    if ! payload="$(registry_fetch_versions "$package" "$token")"; then
      decision="list"
    elif ! decision="$(printf '%s\n' "$payload" | registry_disposable_version_id "$tag" "$keep_id")"; then
      decision="unreadable"
    fi
    case "$decision" in
      delete\ [0-9]*)
        disposable_id="${decision#delete }"
        if [[ "$disposable_id" =~ ^[0-9]+$ ]]; then
          if registry_delete_version "$package" "$token" "$disposable_id"; then
            return 0
          fi
        else
          decision="unreadable"
        fi
        ;;
    esac
    if [[ "$attempt" != 3 ]]; then
      sleep 2
    fi
  done
  case "$decision" in
    none)
      echo "registry tag ${tag} matched no disposable package version" >&2
      ;;
    many\ *)
      echo "registry tag ${tag} matched ${decision#many } disposable package versions" >&2
      ;;
    kept-missing)
      echo "registry tag ${tag} preserved package version ${keep_id} was not listed" >&2
      ;;
    tag-still-present)
      echo "registry tag ${tag} is still on preserved package version ${keep_id}" >&2
      ;;
    list)
      echo "registry tag ${tag} package version list failed" >&2
      ;;
    delete\ [0-9]*)
      echo "registry tag ${tag} disposable package version ${decision#delete } could not be deleted" >&2
      ;;
    *)
      echo "registry tag ${tag} disposable package version match was unreadable" >&2
      ;;
  esac
  if [[ ! "$digest" =~ ^sha256:[0-9a-f]{64}$ ]]; then
    echo "registry tag ${tag} cleanup failed and preserved version ${keep_id} has no digest to restore" >&2
    return 1
  fi
  image_tag="ghcr.io/jatmn/${package}:${tag}"
  registry_restore_preserved_tag "$image_tag" "$digest" || return 1
  return 1
}

registry_restore_preserved_tag() {
  local image_tag="$1" digest="$2"
  if ! registry_promote_tag --prefer-index=false --tag "$image_tag" "${image_tag%:*}@${digest}" >/dev/null; then
    echo "registry tag ${image_tag##*:} could not be restored to ${digest}" >&2
    return 1
  fi
  echo "registry tag ${image_tag##*:} restored to ${digest}" >&2
}

registry_delete_tag() {
  local image_tag="$1" package tag token payload decision action version_id digest attempt
  [[ "$image_tag" =~ ^ghcr\.io/jatmn/([a-z0-9._-]+):([A-Za-z0-9_][A-Za-z0-9_.-]{0,127})$ ]] || {
    echo 'registry tag deletion received an invalid image tag reference' >&2
    return 1
  }
  package="${BASH_REMATCH[1]}"
  tag="${BASH_REMATCH[2]}"
  token="${GHCR_TOKEN:-${GITHUB_TOKEN:-}}"
  [[ -n "$token" ]] || {
    echo "registry tag ${tag} deletion requires GHCR_TOKEN or GITHUB_TOKEN" >&2
    return 1
  }
  payload="$(registry_fetch_versions "$package" "$token")" || {
    echo "registry tag ${tag} package version list failed" >&2
    return 1
  }
  decision="$(printf '%s\n' "$payload" | registry_classify_tag_version "$tag")" || {
    echo "registry tag ${tag} package version match was unreadable" >&2
    return 1
  }
  case "$decision" in
    none)
      echo "registry tag ${tag} matched no package version" >&2
      return 1
      ;;
    many\ *)
      echo "registry tag ${tag} matched ${decision#many } package versions" >&2
      return 1
      ;;
    delete\ [0-9]*|untag\ [0-9]*)
      ;;
    *)
      echo "registry tag ${tag} package version match was unreadable" >&2
      return 1
      ;;
  esac
  action="${decision%% *}"
  version_id="${decision#* }"
  [[ "$version_id" =~ ^[0-9]+$ ]] || {
    echo "registry tag ${tag} package version ID was invalid" >&2
    return 1
  }
  case "$action" in
    delete)
      for attempt in 1 2 3; do
        if registry_delete_version "$package" "$token" "$version_id"; then
          return 0
        fi
        if [[ "$attempt" != 3 ]]; then
          sleep 2
        fi
      done
      echo "registry tag ${tag} package version ${version_id} could not be deleted" >&2
      return 1
      ;;
    untag)
      digest="$(printf '%s\n' "$payload" | registry_preserved_digest "$version_id")" || {
        echo "registry tag ${tag} preserved version ${version_id} has no digest to restore" >&2
        return 1
      }
      if ! registry_move_tag_onto_empty_image "$image_tag"; then
        # A failed push can still have been accepted by the registry.
        registry_restore_preserved_tag "$image_tag" "$digest" || return 1
        return 1
      fi
      registry_delete_detached_tag "$package" "$token" "$tag" "$version_id" "$digest"
      ;;
    *)
      return 1
      ;;
  esac
}

# Status 0 prints the tag's current digest. Status 2 means the tag is
# confirmed absent. Any other status means the digest is unknown: recording
# an empty previous digest would make rollback delete an existing tag.
promotion_previous_digest() {
  local ref="$1" previous="" rc=0
  set +e
  previous="$(inspect_digest "$ref" 2>/dev/null | cut -f1)"
  rc=$?
  set -e
  if [[ "$rc" -eq 0 && "$previous" =~ ^sha256:[0-9a-f]{64}$ ]]; then
    printf '%s\n' "$previous"
    return 0
  fi
  if [[ "$rc" -eq 2 ]]; then
    return 2
  fi
  return 1
}

promote_image_tag() {
  local image="$1" tag="$2" digest="$3" previous="" rc=0
  set +e
  previous="$(promotion_previous_digest "${image}:${tag}")"
  rc=$?
  set -e
  if [[ "$rc" -eq 2 ]]; then
    previous=""
  elif [[ "$rc" -ne 0 ]]; then
    return 1
  fi
  PROMOTION_ROLLBACK+=("${image}:${tag}|${previous}|${digest}")
  registry_promote_tag --prefer-index=false --tag "${image}:${tag}" "${image}@${digest}" >/dev/null \
    || return 1
  digest_matches_ref "${image}:${tag}" "$digest" || return 1
}

rollback_promoted_tags() {
  local entry image_tag previous expected current rc image i failed=0
  [[ -v PROMOTION_ROLLBACK ]] || return 0
  [[ "${#PROMOTION_ROLLBACK[@]}" -eq 0 ]] && return 0
  for ((i=${#PROMOTION_ROLLBACK[@]}-1; i>=0; i--)); do
    entry="${PROMOTION_ROLLBACK[i]}"
    IFS='|' read -r image_tag previous expected <<<"$entry"
    rc=0
    current="$(promotion_previous_digest "$image_tag")" || rc=$?
    if [[ "$rc" -eq 2 && -z "$previous" ]]; then
      continue
    elif [[ "$rc" -ne 0 ]]; then
      echo "rollback cannot inspect ${image_tag}; leaving it unchanged" >&2
      failed=1
      continue
    elif [[ "$current" == "$previous" ]]; then
      continue
    elif [[ "$current" != "$expected" ]]; then
      echo "rollback skipped ${image_tag}: changed since promotion" >&2
      failed=1
      continue
    fi
    if [[ -n "$previous" ]]; then
      image="${image_tag%:*}"
      if ! registry_promote_tag --prefer-index=false --tag "$image_tag" "${image}@${previous}" >/dev/null; then
        echo "rollback failed to restore ${image_tag} to ${previous}" >&2
        failed=1
      fi
    else
      if ! registry_delete_tag "$image_tag" >/dev/null; then
        echo "rollback failed to delete ${image_tag}" >&2
        failed=1
      fi
    fi
  done
  PROMOTION_ROLLBACK=()
  return "$failed"
}

promotion_tag_list() {
  local sha="$1" label="$2" channel="$3"
  printf '%s\n' "$sha"
  if [[ -n "$label" && "$label" != "latest" && "$label" != "$sha" ]]; then
    printf '%s\n' "$label"
  fi
  if [[ "$channel" != "nightly" ]]; then
    printf '%s\n' latest
  fi
  if [[ "$channel" == "stable" || "$channel" == "nightly" ]]; then
    printf '%s\n' "$channel"
  fi
}

resolve_promotion_target() {
  local image="$1" tag="$2" digest="$3" previous="" rc=0
  set +e
  previous="$(promotion_previous_digest "${image}:${tag}")"
  rc=$?
  set -e
  if [[ "$rc" -eq 2 ]]; then
    previous=""
  elif [[ "$rc" -ne 0 ]]; then
    publish_fail "cannot inspect ${image}:${tag} before promotion"
  fi
  PROMOTION_QUEUE+=("${image}|${tag}|${digest}|${previous}")
}

cleanup_promotion_scratch() {
  local dir
  [[ "${#PROMOTION_SCRATCHES[@]}" -eq 0 ]] && return 0
  for dir in "${PROMOTION_SCRATCHES[@]}"; do
    rm -rf "$dir"
  done
  PROMOTION_SCRATCHES=()
}

abort_promotion() {
  local target="$1" rollback_status=0
  rollback_promoted_tags || rollback_status=$?
  cleanup_promotion_scratch
  if [[ "$rollback_status" -ne 0 ]]; then
    publish_fail "failed to promote ${target}; rollback failed"
  fi
  publish_fail "failed to promote ${target}"
}

promotion_exit() {
  local status=$?
  trap - EXIT
  trap '' INT TERM
  if ! rollback_promoted_tags; then
    echo 'promotion interrupted; rollback failed' >&2
    [[ "$status" -ne 0 ]] || status=1
  fi
  cleanup_promotion_scratch
  exit "$status"
}

commit_queued_promotions() {
  local entry image tag digest previous
  [[ "${#PROMOTION_QUEUE[@]}" -eq 0 ]] && return 0
  PROMOTION_ROLLBACK=()
  trap promotion_exit EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  for entry in "${PROMOTION_QUEUE[@]}"; do
    IFS='|' read -r image tag digest previous <<<"$entry"
    PROMOTION_ROLLBACK+=("${image}:${tag}|${previous}|${digest}")
    registry_promote_tag --prefer-index=false --tag "${image}:${tag}" "${image}@${digest}" >/dev/null \
      || abort_promotion "${image}:${tag}"
    digest_matches_ref "${image}:${tag}" "$digest" \
      || abort_promotion "${image}:${tag}"
  done
  PROMOTION_ROLLBACK=()
  PROMOTION_QUEUE=()
  trap - INT TERM
}

finish_deferred_promotions() {
  local entry component dir published rest
  commit_queued_promotions
  if [[ "${#PROMOTION_DONE[@]}" -gt 0 ]]; then
    for entry in "${PROMOTION_DONE[@]}"; do
      component="${entry%%$'\t'*}"
      rest="${entry#*$'\t'}"
      dir="${rest%%$'\t'*}"
      published="${rest#*$'\t'}"
      if [[ -n "$dir" ]]; then
        JMODGUARD_PUBLISH_STATE_DIR="$dir" clear_publish_state "$component"
      fi
      echo "Published ${published}"
    done
  fi
  PROMOTION_DONE=()
  cleanup_promotion_scratch
  trap - EXIT
}

checked_request() {
  local selection mode ref head
  selection="$(publish_request)" || return $?
  if [[ "$selection" == "skip" ]]; then
    echo skip
    return 0
  fi
  mode="${selection%%$'\t'*}"
  ref="${selection#*$'\t'}"
  head="$(run_git rev-parse HEAD)"
  [[ "$head" =~ $SHA_RE && "${GITHUB_SHA:-}" == "$head" ]] \
    || publish_fail 'workflow commit does not match the checked-out full SHA'
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
  planned="$(read_publication_plan)"
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
  if [[ "$mode" == "release" ]]; then
    [[ "$sha" == "${JMODGUARD_RELEASE_SHA:-}" && "$label" == "${ref#refs/tags/}" ]] \
      || publish_fail 'Publication plan differs from the Release Please source'
  elif [[ "$mode" != "manual" && "$sha" != "$head" ]]; then
    publish_fail 'Publication plan differs from the workflow commit'
  fi
  printf '%s\t%s\n' "$sha" "$label"
}

prepare() {
  local selection mode ref head exists_rc sha label plan resolved
  selection="$(checked_request)" || return $?
  if [[ "$selection" == "skip" ]]; then
    echo 'Test-only build; no image publication'
    return 0
  fi
  mode="${selection%%$'\t'*}"
  local rest="${selection#*$'\t'}"
  ref="${rest%%$'\t'*}"
  head="${rest#*$'\t'}"
  set +e
  publication_plan_exists
  exists_rc=$?
  set -e
  if [[ "$exists_rc" -eq 0 ]]; then
    read_plan "$mode" "$ref" "$head" >/dev/null
    echo 'Reusing the original immutable publication plan'
    return 0
  fi
  if [[ "$exists_rc" -ne 1 ]]; then
    publish_fail 'Could not check existing publication plan'
  fi
  if [[ "$mode" == "nightly" ]]; then
    resolved="$(resolve_source main "$ref" "$head")" || return $?
    IFS=$'\t' read -r sha label <<<"$resolved"
    label="$(nightly_label "$sha")"
  else
    resolved="$(resolve_source "$mode" "$ref" "$head")" || return $?
    IFS=$'\t' read -r sha label <<<"$resolved"
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
  write_publication_plan "$plan"
}

registry_manifest_absent() {
  local errfile="$1"
  grep -Eq ': not found$|manifest unknown' "$errfile" || return 1
  if grep -Eq 'failed to do request|unauthorized|authentication required|denied:|no such host|timeout|connection refused|connection reset' "$errfile"; then
    return 1
  fi
  return 0
}

inspect_digest() {
  local raw="" err rc=0
  err="$(mktemp)" || return 1
  set +e
  raw="$(run_docker buildx imagetools inspect "$1" --format '{{json .Manifest}}' 2>"$err")"
  rc=$?
  set -e
  if [[ "$rc" -ne 0 ]]; then
    if registry_manifest_absent "$err"; then
      rm -f "$err"
      return 2
    fi
    cat "$err" >&2
    rm -f "$err"
    return 1
  fi
  rm -f "$err"
  set +e
  MANIFEST_JSON="$raw" node --input-type=commonjs - <<'JS'
const data = JSON.parse(process.env.MANIFEST_JSON || "");
const digest = data.digest || (data.Descriptor && data.Descriptor.digest) || "";
const sizeRaw = data.size ?? (data.Descriptor && data.Descriptor.size);
const size = Number(sizeRaw);
if (!/^sha256:[0-9a-f]{64}$/.test(digest)) process.exit(2);
if (!Number.isInteger(size) || size <= 0) process.exit(2);
process.stdout.write(`${digest}\t${size}\n`);
JS
  rc=$?
  set -e
  [[ "$rc" -eq 0 ]] || return 1
}

source_image_layout() {
  local component="$1" context="$2" dockerfile="${2}/Dockerfile.${1}"
  if [[ "$component" == "webui" ]]; then
    if [[ ! -e "$context/scripts/package-webui.mjs" && ! -e "$dockerfile" ]]; then
      echo legacy
    elif [[ -f "$context/scripts/package-webui.mjs" && -f "$dockerfile" ]]; then
      echo current
    else
      publish_fail 'selected source has an incomplete WebUI package'
    fi
  elif [[ "$component" == "bot" || "$component" == "api" ]]; then
    [[ -f "$dockerfile" ]] || publish_fail 'selected source has no service Dockerfile'
    if [[ ! -e "$context/scripts/bootstrap-container.sh" && ! -e "$context/crates/updater/src/main.rs" ]] \
        && ! grep -Fq 'jmodguard-launcher' "$dockerfile" \
        && ! grep -Fq 'files.sha256' "$dockerfile"; then
      echo legacy
    elif [[ -f "$context/scripts/bootstrap-container.sh" && -f "$context/crates/updater/src/main.rs" ]] \
        && grep -Fq 'jmodguard-launcher' "$dockerfile" \
        && grep -Fq 'files.sha256' "$dockerfile"; then
      echo current
    else
      publish_fail 'selected source has an incomplete service image layout'
    fi
  else
    echo current
  fi
}

verify_published_image() {
  local component="$1" image="$2" digest="$3" context="$4" layout="${5:-current}" scratch cid entry ref
  ref="${image}@${digest}"
  scratch="$(scratch_dir image-verify)"
  if [[ "$component" == "webui" ]]; then
    entry=/webui/index.html
  elif [[ "$layout" == "legacy" ]]; then
    entry="/usr/local/bin/jmodguard-${component}"
  else
    entry=/usr/local/bin/jmodguard-bootstrap
  fi
  run_docker pull --platform linux/amd64 "$ref" >/dev/null
  cid="$(run_docker create --entrypoint "$entry" "$ref")"
  if [[ "$component" == "webui" ]]; then
    run_docker cp "$cid:/webui/." "$scratch/" >/dev/null
    diff -r "$context/dist/webui" "$scratch" >/dev/null || publish_fail 'published WebUI files differ from the packaged assets'
  else
    mkdir -p "$scratch/bin" "$scratch/share/jmodguard"
    run_docker cp "$cid:/usr/local/bin/jmodguard-${component}" "$scratch/bin/jmodguard-${component}" >/dev/null
    [[ -s "$scratch/bin/jmodguard-${component}" ]] || publish_fail 'published service executable is empty'
    if [[ "$layout" != "legacy" ]]; then
      run_docker cp "$cid:/usr/local/bin/jmodguard-launcher" "$scratch/bin/jmodguard-launcher" >/dev/null
      run_docker cp "$cid:/usr/local/share/jmodguard/files.sha256" "$scratch/share/jmodguard/files.sha256" >/dev/null
      (cd "$scratch" && sha256sum -c share/jmodguard/files.sha256 >/dev/null) || publish_fail 'effective service binaries do not match image inventory'
    fi
  fi
  verify_image_runtime_configuration "$component" "$ref" "$layout" \
    || publish_fail 'published image failed runtime configuration verification'
  run_docker rm "$cid" >/dev/null
  rm -rf "$scratch"
}

redact_tokens() {
  GHCR_TOKEN="${1:-}" GITHUB_TOKEN_VALUE="${2:-}" SIGNING_KEY="${3:-}" SIGNING_PASSWORD="${4:-}" \
    node --input-type=commonjs - <<'JS'
for (const value of [
  process.env.GHCR_TOKEN,
  process.env.GITHUB_TOKEN_VALUE,
  process.env.SIGNING_KEY,
  process.env.SIGNING_PASSWORD,
]) {
  if (!value) continue;
  const escaped = value.replaceAll("%", "%25").replaceAll("\r", "%0D").replaceAll("\n", "%0A");
  process.stdout.write(`::add-mask::${escaped}\n`);
}
JS
}

require_signing_secrets() {
  if [[ -z "$_JMG_SIGNING_KEY" && -n "$_JMG_SIGNING_KEY_FILE" ]]; then
    load_signing_secrets_from_files
  fi
  [[ -n "$_JMG_SIGNING_KEY" ]] \
    || publish_fail 'JMODGUARD_UPDATE_SIGNING_KEY is empty; image signing is required before tags move'
  [[ -n "$_JMG_SIGNING_PASSWORD" ]] \
    || publish_fail 'JMODGUARD_UPDATE_SIGNING_PASSWORD is empty; image signing is required before tags move'
  [[ -f "$SIGNING_PUBLIC_KEY" ]] \
    || publish_fail 'committed signing public key is missing'
}

validate_signing_key_pair() {
  local scratch extracted_pub pass key_file
  [[ -n "$_JMG_SIGNING_KEY" && -n "$_JMG_SIGNING_PASSWORD" ]] \
    || publish_fail 'signing secrets are missing'
  scratch="$(mktemp -d "${TMPDIR:-.tmp}/signing-validate-XXXXXX")"
  key_file="$scratch/signing.key"
  extracted_pub="$scratch/extracted.pub"
  printf '%s' "$_JMG_SIGNING_KEY" >"$key_file"
  pass="$_JMG_SIGNING_PASSWORD"
  (
    export COSIGN_PASSWORD="$pass"
    cosign public-key --key "$key_file" --outfile "$extracted_pub"
  ) || publish_fail 'signing private key is malformed'
  diff -q "$extracted_pub" "$SIGNING_PUBLIC_KEY" >/dev/null \
    || publish_fail 'signing private key does not match deploy/update-signing.pub'
  rm -rf "$scratch"
}

image_published_at() {
  local ref="$1"
  local raw
  raw="$(run_docker buildx imagetools inspect "$ref" --format '{{json .Image}}')"
  IMAGE_JSON="$raw" node --input-type=commonjs - <<'JS'
const data = JSON.parse(process.env.IMAGE_JSON || "{}");
const labels = (data.config && data.config.Labels) || (data.Config && data.Config.Labels) || {};
const published = labels["io.jmodguard.published-at"] || "";
if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$/.test(published)) process.exit(2);
const parsed = new Date(published);
if (Number.isNaN(parsed.getTime()) || parsed.toISOString().replace(/\.\d{3}Z$/, "Z") !== published) {
  process.exit(2);
}
process.stdout.write(published);
JS
}

not_after_from_published_at() {
  PUBLISHED_AT="$1" node --input-type=commonjs - <<'JS'
const published = process.env.PUBLISHED_AT || "";
const date = new Date(published);
if (Number.isNaN(date.getTime())) process.exit(2);
date.setUTCDate(date.getUTCDate() + 366);
process.stdout.write(date.toISOString().replace(/\.\d{3}Z$/, "Z"));
JS
}

sign_published_digest() {
  local component="$1" image="$2" digest="$3"
  local published_at not_after pass
  [[ "$image" =~ $IMAGE_SIGNING_RE ]] || publish_fail 'invalid image signing reference'
  [[ -f "$_JMG_SIGNING_KEY_FILE" && -f "$_JMG_SIGNING_PASSWORD_FILE" ]] \
    || publish_fail 'signing secret files are missing'
  published_at="$(image_published_at "${image}@${digest}")" \
    || publish_fail 'published image lacks io.jmodguard.published-at'
  not_after="$(not_after_from_published_at "$published_at")" \
    || publish_fail 'cannot compute image signature expiry'
  pass="$(<"$_JMG_SIGNING_PASSWORD_FILE")"
  (
    export COSIGN_PASSWORD="$pass"
    run_cosign sign \
      --yes \
      --key "$_JMG_SIGNING_KEY_FILE" \
      --tlog-upload=false \
      -a "io.jmodguard.not-after=${not_after}" \
      "${image}@${digest}"
  ) || publish_fail 'cosign sign failed'
  run_cosign verify \
    --key "$SIGNING_PUBLIC_KEY" \
    --insecure-ignore-tlog=true \
    "${image}@${digest}" \
    || publish_fail 'cosign verify failed'
}

scratch_dir() {
  local prefix="$1"
  mkdir -p "${TMPDIR:-.tmp}"
  mktemp -d "${TMPDIR:-.tmp}/${prefix}-XXXXXX"
}

publish_state_dir() {
  local base="${JMODGUARD_PUBLISH_STATE_DIR:-${RUNNER_TEMP:-${TMPDIR:-/tmp}/jmodguard-publish-state}}"
  mkdir -p "$base"
  printf '%s' "$base"
}

publish_state_file() {
  local component="$1"
  printf '%s/%s.json' "$(publish_state_dir)" "$component"
}

write_publish_state() {
  local component="$1" payload="$2"
  printf '%s\n' "$payload" >"$(publish_state_file "$component")"
}

read_publish_state() {
  local component="$1" file
  file="$(publish_state_file "$component")"
  [[ -f "$file" ]] || return 1
  cat "$file"
}

clear_publish_state() {
  local component="$1"
  rm -f "$(publish_state_file "$component")"
}

publish_handoff_file() {
  printf '%s/handoff.json' "$(publish_state_dir)"
}

write_publish_handoff() {
  local component="$1" sign="$2"
  COMPONENT="$component" SIGN="$sign" node --input-type=commonjs - <<'JS' >"$(publish_handoff_file)"
const sign = process.env.SIGN === "true";
process.stdout.write(JSON.stringify({ component: process.env.COMPONENT, sign }));
JS
}

read_publish_handoff() {
  local file
  file="$(publish_handoff_file)"
  [[ -f "$file" ]] || return 1
  cat "$file"
}

assert_publish_state_matches_trusted() {
  local component="$1" state="$2" image="$3" sha="$4" label="$5" mode="$6" channel="$7" staging_ref="$8" published_digest="$9"
  COMPONENT="$component" STATE_JSON="$state" IMAGE="$image" SHA="$sha" LABEL="$label" MODE="$mode" CHANNEL="$channel" \
    STAGING_REF="$staging_ref" PUBLISHED_DIGEST="$published_digest" node --input-type=commonjs - <<'JS' \
    || publish_fail 'publish state does not match trusted publication targets'
const state = JSON.parse(process.env.STATE_JSON);
const component = process.env.COMPONENT;
const trusted = {
  component,
  image: process.env.IMAGE,
  sha: process.env.SHA,
  label: process.env.LABEL || "",
  mode: process.env.MODE,
  channel: process.env.CHANNEL || "",
  staging_ref: process.env.STAGING_REF,
  published_digest: process.env.PUBLISHED_DIGEST,
};
for (const key of Object.keys(trusted)) {
  if (state[key] !== trusted[key]) process.exit(1);
}
JS
}

publish_component_build() {
  local component="$1"
  local selection mode ref head sha label image username token source_token scratch context channel layout published_digest resolved
  local staging_ref metadata_file
  selection="$(checked_request)" || return $?
  if [[ "$selection" == "skip" ]]; then
    echo 'Test-only build; no image publication'
    clear_publish_state "$component"
    write_publish_handoff "$component" false
    return 0
  fi
  mode="${selection%%$'\t'*}"
  local rest="${selection#*$'\t'}"
  ref="${rest%%$'\t'*}"
  head="${rest#*$'\t'}"
  resolved="$(read_plan "$mode" "$ref" "$head")" || return $?
  IFS=$'\t' read -r sha label <<<"$resolved"
  if [[ "$mode" == "manual" || "$mode" == "release" ]]; then
    resolve_source manual "$sha" "$head" >/dev/null
  fi
  image="ghcr.io/jatmn/jmodguard-${component}"
  if [[ "$mode" == "main" && "${GITHUB_RUN_ATTEMPT:-1}" == "1" ]] \
      && ! changed_since_publication "$component" "$sha" "$image"; then
    echo "${component}: image inputs match the last publication; no rebuild"
    clear_publish_state "$component"
    write_publish_handoff "$component" false
    return 0
  fi
  username="${GHCR_USERNAME:-}"
  [[ -n "$username" ]] || publish_fail 'Set GHCR_USERNAME to the GitHub account owning the GHCR credential'
  token="${GHCR_TOKEN:-}"
  [[ -n "$token" ]] || publish_fail 'GHCR_TOKEN is empty; provide the workflow package-write token'
  source_token="${GITHUB_TOKEN:-}"
  if [[ -z "$source_token" || "$source_token" == '$GITHUB_TOKEN' ]]; then
    source_token="$token"
  fi
  export GITHUB_TOKEN="$source_token"
  scratch="$(scratch_dir publish)"
  context="${scratch}/source"
  JMODGUARD_PUBLISH_CLEANUP_CONTEXT="$context"
  JMODGUARD_PUBLISH_CLEANUP_SCRATCH="$scratch"
  cleanup_publish() {
    # run_git exits on failure; contain that exit so scratch cleanup still runs.
    if ! (run_git worktree remove --force "$JMODGUARD_PUBLISH_CLEANUP_CONTEXT" >/dev/null); then
      echo 'Image publication cleanup could not remove the source worktree' >&2
    fi
    rm -rf "$JMODGUARD_PUBLISH_CLEANUP_SCRATCH"
  }
  trap cleanup_publish EXIT
  redact_tokens "$token" "$source_token" "" ""
  export DOCKER_CONFIG="${scratch}/docker"
  mkdir -m 700 "$DOCKER_CONFIG"
  printf '%s' "$token" | run_docker login ghcr.io --username "$username" --password-stdin >/dev/null
  run_git worktree add --detach "$context" "$sha" >/dev/null
  layout="$(source_image_layout "$component" "$context")"
  if [[ "$component" == "webui" && "$layout" == "legacy" ]]; then
    echo 'webui: selected source predates the independent WebUI package; no image to publish'
    clear_publish_state "$component"
    write_publish_handoff "$component" false
    trap - EXIT
    cleanup_publish
    return 0
  fi
  channel="$(channel_for "$mode" "$label" || true)"
  assert_signing_secrets_not_staged_on_disk
  signing_secrets_in_environ && publish_fail 'signing secrets must not be present during image build'
  run_selected_source_build() {
    signing_secrets_in_environ && publish_fail 'signing secrets must not be present during selected-source build'
    assert_signing_secrets_not_staged_on_disk
    "$@"
  }
  if [[ "$component" == "webui" ]]; then
    if [[ -f "$context/tools/webui/package-lock.json" ]]; then
      run_selected_source_build npm ci --ignore-scripts --prefix "$context/tools/webui"
      run_selected_source_build npm run build --prefix "$context/tools/webui"
    fi
    run_selected_source_build node "$context/scripts/package-webui.mjs" "$context" "$sha" >/dev/null
  fi
  staging_ref="$(publish_staging_ref "$image" "$sha")"
  metadata_file="${scratch}/build-metadata.json"
  local -a build_cmd=()
  readarray -d '' -t build_cmd < <(build_args "$component" "$sha" "$label" "$context" "$channel" "$layout" "$staging_ref")
  [[ "${#build_cmd[@]}" -gt 1 ]] || publish_fail 'cannot construct image build arguments'
  build_cmd+=("--metadata-file" "$metadata_file")
  run_selected_source_build run_docker "${build_cmd[@]:1}"
  published_digest="$(digest_from_build_metadata "$metadata_file")" \
    || publish_fail 'build metadata did not report an image digest'
  assert_digest_matches_ref "$staging_ref" "$published_digest"
  verify_published_image "$component" "$image" "$published_digest" "$context" "$layout" \
    || publish_fail 'published image failed content verification'
  write_publish_state "$component" "$(COMPONENT="$component" IMAGE="$image" SHA="$sha" LABEL="$label" MODE="$mode" CHANNEL="$channel" DIGEST="$published_digest" STAGING_REF="$staging_ref" node --input-type=commonjs - <<'JS'
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
  write_publish_handoff "$component" true
  trap - EXIT
  cleanup_publish
}

publish_component_sign() {
  local component="$1"
  local selection mode ref head resolved sha label image channel published_digest staging_ref
  local state handoff sign_flag handoff_sign_expected=""
  local username token source_token scratch tag
  selection="$(checked_request)" || return $?
  if [[ "$selection" == "skip" ]]; then
    echo 'Test-only build; no image publication'
    return 0
  fi
  mode="${selection%%$'\t'*}"
  local rest="${selection#*$'\t'}"
  ref="${rest%%$'\t'*}"
  head="${rest#*$'\t'}"
  if handoff="$(read_publish_handoff 2>/dev/null)"; then
    sign_flag="$(HANDOFF_JSON="$handoff" node --input-type=commonjs -e 'process.stdout.write(String(JSON.parse(process.env.HANDOFF_JSON).sign))')"
    if [[ "$(HANDOFF_JSON="$handoff" node --input-type=commonjs -e 'process.stdout.write(JSON.parse(process.env.HANDOFF_JSON).component)')" != "$component" ]]; then
      publish_fail 'publish handoff component mismatch'
    fi
    handoff_sign_expected="$sign_flag"
    if [[ "$sign_flag" != "true" ]]; then
      echo "${component}: build produced no verified image; skipping promotion"
      return 0
    fi
  fi
  resolved="$(read_plan "$mode" "$ref" "$head")" || return $?
  IFS=$'\t' read -r sha label <<<"$resolved"
  if [[ "$mode" == "manual" || "$mode" == "release" ]]; then
    resolve_source manual "$sha" "$head" >/dev/null
  fi
  image="ghcr.io/jatmn/jmodguard-${component}"
  staging_ref="$(publish_staging_ref "$image" "$sha")"
  channel="$(channel_for "$mode" "$label" || true)"
  if ! published_digest="$(inspect_digest "$staging_ref" 2>/dev/null | cut -f1)"; then
    if [[ "$handoff_sign_expected" == "true" ]]; then
      publish_fail 'verified staging image is missing from the registry'
    fi
    echo "${component}: no verified image to sign; skipping promotion"
    return 0
  fi
  if state="$(read_publish_state "$component" 2>/dev/null)"; then
    assert_publish_state_matches_trusted "$component" "$state" "$image" "$sha" "$label" "$mode" "$channel" "$staging_ref" "$published_digest"
  fi
  if [[ -z "$_JMG_SIGNING_KEY" || -z "$_JMG_SIGNING_PASSWORD" ]]; then
    capture_signing_secrets
  fi
  username="${GHCR_USERNAME:-}"
  [[ -n "$username" ]] || publish_fail 'Set GHCR_USERNAME to the GitHub account owning the GHCR credential'
  token="${GHCR_TOKEN:-}"
  [[ -n "$token" ]] || publish_fail 'GHCR_TOKEN is empty; provide the workflow package-write token'
  source_token="${GITHUB_TOKEN:-}"
  if [[ -z "$source_token" || "$source_token" == '$GITHUB_TOKEN' ]]; then
    source_token="$token"
  fi
  require_signing_secrets
  scratch="$(scratch_dir publish-sign)"
  PROMOTION_SCRATCHES+=("$scratch")
  trap cleanup_promotion_scratch EXIT
  validate_signing_key_pair
  redact_tokens "$token" "$source_token" "$_JMG_SIGNING_KEY" "$_JMG_SIGNING_PASSWORD"
  export DOCKER_CONFIG="${scratch}/docker"
  mkdir -m 700 "$DOCKER_CONFIG"
  printf '%s' "$token" | run_docker login ghcr.io --username "$username" --password-stdin >/dev/null
  ensure_signing_secrets_captured
  persist_signing_secrets_to_files "$scratch"
  signing_secrets_in_environ && publish_fail 'signing secrets must not remain in the process environment'
  assert_digest_matches_ref "$staging_ref" "$published_digest" \
    || publish_fail 'staging reference does not match the verified build digest'
  sign_published_digest "$component" "$image" "$published_digest" \
    || publish_fail 'published image failed signing'
  while IFS= read -r tag; do
    [[ -n "$tag" ]] || continue
    resolve_promotion_target "$image" "$tag" "$published_digest"
  done < <(promotion_tag_list "$sha" "$label" "$channel")
  if [[ "${JMODGUARD_DEFER_PROMOTION:-}" == 1 ]]; then
    PROMOTION_DONE+=("${component}"$'\t'"${JMODGUARD_PUBLISH_STATE_DIR:-}"$'\t'"${image}:${sha}")
    return 0
  fi
  commit_queued_promotions
  clear_publish_state "$component"
  echo "Published ${image}:${sha}"
  trap - EXIT
  cleanup_promotion_scratch
}

publish_component_phase() {
  local phase_fn="$1" component="$2" rc=0
  "$phase_fn" "$component"
  rc=$?
  return "$rc"
}

publish_component() {
  local component="$1"
  local phase="${JMODGUARD_PUBLISH_PHASE:-all}"
  local phase_rc=0
  if [[ "$phase" == all ]]; then
    if [[ -n "${JMODGUARD_UPDATE_SIGNING_KEY:-}" || -n "${JMODGUARD_UPDATE_SIGNING_PASSWORD:-}" ]]; then
      capture_signing_secrets
      validate_signing_key_pair
    fi
  fi
  case "$phase" in
    build)
      publish_component_phase publish_component_build "$component"
      phase_rc=$?
      return "$phase_rc"
      ;;
    sign)
      publish_component_phase publish_component_sign "$component"
      phase_rc=$?
      return "$phase_rc"
      ;;
    all)
      publish_component_phase publish_component_build "$component"
      phase_rc=$?
      [[ "$phase_rc" -eq 0 ]] || return "$phase_rc"
      if [[ -f "$(publish_state_file "$component")" ]]; then
        publish_component_phase publish_component_sign "$component"
        phase_rc=$?
        return "$phase_rc"
      fi
      ;;
    *)
      publish_fail "unknown JMODGUARD_PUBLISH_PHASE: ${phase}"
      ;;
  esac
}

publish_image_main() {
  local component="${1:-}"
  case "$component" in
    prepare) prepare ;;
    bot|api|webui) publish_component "$component" ;;
    *)
      echo "usage: bash scripts/publish-image.sh prepare|bot|api|webui" >&2
      exit 2
      ;;
  esac
}

if [[ "${JMODGUARD_PUBLISH_IMAGE_LIB:-}" == 1 ]]; then
  return 0 2>/dev/null || exit 0
fi
publish_image_main "${1:-}"
