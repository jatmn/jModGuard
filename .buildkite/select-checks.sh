#!/usr/bin/env bash
# Choose Buildkite check steps from the diff, then upload only those steps.
# Unclassified paths and diffs that cannot be proven run every check.
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
cd "$root"

usage() {
  cat <<'EOF'
Usage: bash .buildkite/select-checks.sh plan [--files PATH]...
       bash .buildkite/select-checks.sh upload

plan prints one selected step group per line. upload rewrites
.buildkite/pipeline.steps.yml with if: true or if: false and, on a
Buildkite agent, uploads that pipeline. A missing diff selects every group.
Selection reasons go to stderr; stdout remains the plan or generated YAML.
EOF
}

mode=""
files=()
while [ "$#" -gt 0 ]; do
  case "$1" in
    plan|upload) mode="$1"; shift ;;
    --files)
      shift
      [ "$#" -ge 1 ] || { usage >&2; exit 2; }
      while [ "$#" -gt 0 ] && [ "${1#--}" = "$1" ]; do
        files+=("$1")
        shift
      done
      ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
done
[ -n "$mode" ] || { usage >&2; exit 2; }

groups=(
  contracts frontend fmt dylint api bot conductor runtime updater core
  integration boundaries publish-tests bootstrap pullfrog supply-chain
  remote-database
)
selected=""
full=0

add() {
  case " ${selected} " in
    *" $1 "*) ;;
    *) selected="${selected} $1" ;;
  esac
}

all_groups() {
  echo "select-checks: all groups: $*" >&2
  full=1
  selected=""
  local group
  for group in "${groups[@]}"; do
    add "$group"
  done
}

all_rust() {
  local group
  for group in fmt dylint api bot conductor runtime updater core integration boundaries supply-chain remote-database publish-tests bootstrap; do
    add "$group"
  done
}

# Root data is consumed by bot, API, updater and integration tests. Runtime and
# conductor deliberately do not import it; conductor embeds migration SQL below.
core_consumers() {
  local group
  for group in fmt dylint api bot updater core integration boundaries remote-database; do
    add "$group"
  done
}

# jmodguard-runtime is a direct dependency of jmodguard (core) and jmodguard-conductor;
# bot/API consume it through the root crate. Exercise those groups on runtime edits.
runtime_consumers() {
  add core
  add conductor
  add integration
}

# jmodguard-updater is a library dependency of jmodguard-api and a release
# binary in both the bot and API images (jmodguard-launcher).
updater_consumers() {
  add api
  add bot
  add boundaries
}

classify() {
  local path="$1"
  local top="${path%%/*}"
  case "$path" in
    .buildkite/pipeline.yml|.buildkite/pipeline.steps.yml|.buildkite/select-checks.sh|.buildkite/select-checks-test.sh|.buildkite/with-cargo-auth.sh|scripts/ci-preflight.sh)
      all_groups "CI control changed: $path"
      return
      ;;
    .buildkite/publish-image.sh|.buildkite/publish-image-test.sh)
      add publish-tests
      return
      ;;
    .buildkite/*)
      all_groups "CI control changed: $path"
      return
      ;;
    Cargo.lock|rust-toolchain.toml|deny.toml|Cargo.toml)
      all_rust
      return
      ;;
    dylint.toml|scripts/dylint.sh|scripts/dylint-test.sh)
      add dylint
      return
      ;;
    crates/api/Cargo.toml)
      add fmt; add dylint; add api; add boundaries; add integration; add supply-chain
      return
      ;;
    crates/bot/Cargo.toml)
      add fmt; add dylint; add bot; add boundaries; add integration; add supply-chain
      return
      ;;
    crates/conductor/Cargo.toml)
      add fmt; add dylint; add conductor; add boundaries; add supply-chain
      return
      ;;
    crates/runtime/Cargo.toml)
      add fmt; add dylint; add runtime; add boundaries; add supply-chain
      runtime_consumers
      return
      ;;
    crates/updater/Cargo.toml)
      add fmt; add dylint; add updater; add supply-chain
      updater_consumers
      return
      ;;
    crates/integration-tests/Cargo.toml)
      add fmt; add dylint; add integration; add remote-database; add supply-chain
      return
      ;;
    Dockerfile.webui)
      add frontend
      add publish-tests
      return
      ;;
    Dockerfile.bot)
      add fmt; add bot; add updater; add bootstrap; add publish-tests
      return
      ;;
    crates/conductor/frontend/*)
      add frontend
      add fmt
      add conductor
      add boundaries
      return
      ;;
    Dockerfile.api)
      add api; add fmt; add updater; add boundaries; add integration; add bootstrap; add publish-tests
      return
      ;;
    Dockerfile.conductor)
      add conductor; add fmt; add boundaries; add bootstrap; add publish-tests
      return
      ;;
    scripts/bootstrap-container.sh|scripts/bootstrap-container-test.sh)
      add bootstrap
      return
      ;;
    scripts/check-service-boundaries.sh)
      add boundaries
      return
      ;;
    scripts/public-install-acceptance-test.sh|scripts/dependabot-config-test.sh|.githooks/*|scripts/install-git-hooks.sh)
      add contracts
      return
      ;;
    scripts/pullfrog-command-check.mjs|scripts/pullfrog-command-check.test.mjs|.github/workflows/*|.github/dependabot.yml)
      add pullfrog
      add contracts
      return
      ;;
    scripts/frontend-state-test.mjs|scripts/frontend-contrast-test.mjs|scripts/frontend-warmup-test.mjs|scripts/publish-frontend-test.mjs|scripts/publish-frontend-bunny.sh|scripts/frontend_publication.sh|scripts/package-webui.mjs|scripts/package-webui-test.mjs|scripts/administration-state-test.mjs|scripts/require-host-eslint-node.sh|tools/eslint/*|tools/eslint-web-selftest.sh)
      add frontend
      return
      ;;
    crates/integration-tests/tests/remote_database_smoke.rs|crates/integration-tests/tests/guild_store_smoke.rs)
      add fmt
      add dylint
      add remote-database
      return
      ;;
  esac
  case "$top" in
    vendor)
      all_rust
      ;;
    src)
      core_consumers
      ;;
    migrations)
      core_consumers
      add conductor
      ;;
    frontend)
      # Default API embed-ui compiles these files with include_str!.
      add frontend
      add api
      ;;
    docs)
      add contracts
      ;;
    crates)
      local crate="${path#crates/}"
      crate="${crate%%/*}"
      case "$crate" in
        api) add fmt; add dylint; add api; add boundaries; add integration ;;
        bot) add fmt; add dylint; add bot; add boundaries; add integration ;;
        conductor) add fmt; add dylint; add conductor; add boundaries ;;
        runtime) add fmt; add dylint; add runtime; add boundaries; runtime_consumers ;;
        updater) add fmt; add dylint; add updater; updater_consumers ;;
        integration-tests) add fmt; add dylint; add integration ;;
        *) all_groups "unclassified path: $path" ;;
      esac
      ;;
    *)
      case "$path" in
        *.md|LICENSE|AGENTS.md|README.md|development.md|config.example.toml) add contracts ;;
        *) all_groups "unclassified path: $path" ;;
      esac
      ;;
  esac
}

prove_diff() {
  if [ "${#files[@]}" -gt 0 ]; then
    echo "select-checks: explicit file list" >&2
    printf '%s\n' "${files[@]}"
    return 0
  fi
  if [ "${BUILDKITE_SOURCE:-}" = "schedule" ] || [ -n "${BUILDKITE_TAG:-}" ] || [ -n "${JMODGUARD_PUBLISH_SOURCE:-}" ]; then
    echo "select-checks: scheduled, tag, or manual publication build" >&2
    return 1
  fi
  if [ "${BUILDKITE_PULL_REQUEST:-false}" != "false" ]; then
    local base="${BUILDKITE_PULL_REQUEST_BASE_BRANCH:-main}"
    git fetch --no-tags origin "$base" || return 1
    echo "select-checks: PR diff origin/${base}...HEAD" >&2
    git diff --name-only "origin/${base}...HEAD" || return 1
    return 0
  fi
  local branch="${BUILDKITE_BRANCH:-main}"
  local remote="origin/${branch}"
  local before="" after
  after="$(git rev-parse "${BUILDKITE_COMMIT:-HEAD}")"
  if git rev-parse --verify --quiet "$remote" >/dev/null; then
    before="$(git rev-parse "$remote")"
  fi
  git fetch --no-tags origin "$branch" || return 1
  if [ -n "$before" ] && [ "$before" != "$after" ]; then
    case "$before" in
      0000000000000000000000000000000000000000) return 1 ;;
    esac
    echo "select-checks: branch diff $before..$after" >&2
    git diff --name-only "$before" "$after" || return 1
    return 0
  fi
  if [ "${BUILDKITE_SOURCE:-}" = "webhook" ] \
      && [ "$branch" = "main" ] \
      && [ -z "${BUILDKITE_TAG:-}" ]; then
    echo "select-checks: main push range unavailable" >&2
    return 1
  fi
  if ! git rev-parse --verify --quiet "${after}^1" >/dev/null; then
    return 1
  fi
  echo "select-checks: first-parent diff ${after}^1..${after}" >&2
  git diff --name-only "${after}^1" "$after"
}

add contracts
if ! names="$(prove_diff)"; then
  all_groups "full validation requested or diff unavailable"
elif [ -z "${names}" ]; then
  all_groups "empty diff"
else
  while IFS= read -r path; do
    [ -n "$path" ] || continue
    classify "$path"
    [ "$full" -eq 1 ] && break
  done <<<"$names"
fi

# cargo deny / cargo audit stay on every plan that compiles or publishes Rust.
ensure_supply_chain() {
  local group
  for group in fmt dylint api bot conductor runtime updater core integration boundaries remote-database publish-tests bootstrap; do
    case " ${selected} " in
      *" ${group} "*) add supply-chain; return ;;
    esac
  done
}
ensure_supply_chain
echo "select-checks: selected:${selected}" >&2

if [ "$mode" = "plan" ]; then
  for group in "${groups[@]}"; do
    case " ${selected} " in
      *" ${group} "*) printf '%s\n' "$group" ;;
    esac
  done
  exit 0
fi

steps="$root/.buildkite/pipeline.steps.yml"
generated="$(mktemp)"
awk -v selected=" ${selected} " '
  /^  # group: / { group = $3; next }
  {
    line = $0
    if (line ~ /if: __SELECTED__/) {
      if (index(selected, " " group " ") > 0) print "    if: \"true\""
      else print "    if: \"false\""
      next
    }
    print line
  }
' "$steps" >"$generated"

if [ "${BUILDKITE:-}" = "true" ]; then
  buildkite-agent pipeline upload "$generated"
else
  cat "$generated"
fi
rm -f "$generated"
