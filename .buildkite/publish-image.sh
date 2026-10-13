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
  # COPY crates/ supplies Cargo workspace layout, not a production dependency
  # on every service. Keep every manifest (resolution), but only linked source.
  local -a inputs=(
    "Dockerfile.${component}" "Dockerfile.${component}.dockerignore" .dockerignore
    Cargo.toml Cargo.lock LICENSE.md rust-toolchain.toml ':(glob)crates/*/Cargo.toml'
    src migrations vendor crates/bot crates/api crates/runtime crates/updater
    .buildkite/publish-image.sh scripts/bootstrap-container.sh
    ':(exclude,glob)crates/*/tests/**'
    ':(exclude,glob)crates/*/benches/**'
    ':(exclude,glob)crates/*/examples/**'
  )
  if [[ "$component" == "webui" ]]; then
    inputs=(Dockerfile.webui Dockerfile.webui.dockerignore .dockerignore LICENSE.md
      frontend tools/webui scripts/package-webui.mjs .buildkite/publish-image.sh src/db.rs src/db/update_control.rs migrations/guild/current_version.rs)
  fi
  if ! names="$(skip_source_diff "$previous" "$sha" -- "${inputs[@]}")"; then
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
  local layout="${6:-current}"
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
      guild_schema="$(cat "$context/migrations/guild/current_version.rs")"
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
  fi
  args+=(--build-arg "SOURCE_SHA=${sha}" --tag "${image}:${sha}")
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
  local component="$1" image="$2" context="$3" layout="${4:-current}" scratch cid entry
  scratch="$(scratch_dir image-verify)"
  if [[ "$component" == "webui" ]]; then
    entry=/webui/index.html
  elif [[ "$layout" == "legacy" ]]; then
    entry="/usr/local/bin/jmodguard-${component}"
  else
    entry=/usr/local/bin/jmodguard-bootstrap
  fi
  run_docker pull --platform linux/amd64 "$image" >/dev/null
  cid="$(run_docker create --entrypoint "$entry" "$image")"
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
  run_docker rm "$cid" >/dev/null
  rm -rf "$scratch"
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

publish_component() {
  local component="$1"
  local selection mode ref head sha label image username token source_token scratch context channel layout published_digest published_size promoted_digest
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
  JMODGUARD_PUBLISH_CLEANUP_CONTEXT="$context"
  JMODGUARD_PUBLISH_CLEANUP_SCRATCH="$scratch"
  cleanup_publish() {
    run_git worktree remove --force "$JMODGUARD_PUBLISH_CLEANUP_CONTEXT" >/dev/null 2>&1 || true
    rm -rf "$JMODGUARD_PUBLISH_CLEANUP_SCRATCH"
  }
  trap cleanup_publish EXIT
  export DOCKER_CONFIG="${scratch}/docker"
  mkdir -m 700 "$DOCKER_CONFIG"
  printf '%s' "$token" | run_docker login ghcr.io --username "$username" --password-stdin >/dev/null
  run_git worktree add --detach "$context" "$sha" >/dev/null
  layout="$(source_image_layout "$component" "$context")"
  if [[ "$component" == "webui" && "$layout" == "legacy" ]]; then
    echo 'webui: selected source predates the independent WebUI package; no image to publish'
    trap - EXIT
    cleanup_publish
    return 0
  fi
  channel="$(channel_for "$mode" "$label" || true)"
  if [[ "$component" == "webui" ]]; then
    if [[ -f "$context/tools/webui/package-lock.json" ]]; then
      npm ci --ignore-scripts --prefix "$context/tools/webui"
      npm run build --prefix "$context/tools/webui"
    fi
    node "$context/scripts/package-webui.mjs" "$context" "$sha" >/dev/null
  fi
  readarray -d '' -t build_cmd < <(build_args "$component" "$sha" "$label" "$context" "$channel" "$layout")
  [[ "${#build_cmd[@]}" -gt 1 ]] || publish_fail 'cannot construct image build arguments'
  run_docker "${build_cmd[@]:1}"
  IFS=$'\t' read -r published_digest published_size <<<"$(inspect_digest "${image}:${sha}")"
  verify_published_image "$component" "${image}:${sha}" "$context" "$layout"
  if [[ "$channel" != "nightly" ]]; then
    run_docker buildx imagetools create --prefer-index=false --tag "${image}:latest" "${image}@${published_digest}" >/dev/null
    IFS=$'\t' read -r promoted_digest published_size <<<"$(inspect_digest "${image}:latest")"
    [[ "$promoted_digest" == "$published_digest" ]] || publish_fail 'latest pointer differs from verified image digest'
  fi
  if [[ "$channel" == "stable" || "$channel" == "nightly" ]]; then
    run_docker buildx imagetools create --prefer-index=false --tag "${image}:${channel}" "${image}@${published_digest}" >/dev/null
    IFS=$'\t' read -r promoted_digest published_size <<<"$(inspect_digest "${image}:${channel}")"
    [[ "$promoted_digest" == "$published_digest" ]] || publish_fail 'release channel pointer differs from verified image digest'
  fi
  echo "Published ${image}:${sha}"
  trap - EXIT
  cleanup_publish
}

publish_image_main() {
  local component="${1:-}"
  case "$component" in
    prepare) prepare ;;
    bot|api|webui) publish_component "$component" ;;
    *)
      echo "usage: bash .buildkite/publish-image.sh prepare|bot|api|webui" >&2
      exit 2
      ;;
  esac
}

if [[ "${JMODGUARD_PUBLISH_IMAGE_LIB:-}" == 1 ]]; then
  return 0 2>/dev/null || exit 0
fi
publish_image_main "${1:-}"
