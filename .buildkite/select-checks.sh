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
  contracts frontend fmt api bot conductor runtime updater core
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
  full=1
  selected=""
  local group
  for group in "${groups[@]}"; do
    add "$group"
  done
}

all_rust() {
  local group
  for group in fmt api bot conductor runtime updater core integration boundaries supply-chain remote-database publish-tests bootstrap; do
    add "$group"
  done
}

classify() {
  local path="$1"
  local top="${path%%/*}"
  case "$path" in
    .buildkite/pipeline.yml|.buildkite/pipeline.steps.yml|.buildkite/select-checks.sh|.buildkite/select-checks-test.sh|.buildkite/with-cargo-auth.sh|scripts/ci-preflight.sh)
      all_groups
      return
      ;;
    .buildkite/publish-image.sh|.buildkite/publish-image-test.sh)
      add publish-tests
      return
      ;;
    .buildkite/*)
      all_groups
      return
      ;;
    Cargo.lock|rust-toolchain.toml|deny.toml|Cargo.toml)
      all_rust
      return
      ;;
    crates/api/Cargo.toml)
      add fmt; add api; add boundaries; add integration; add supply-chain
      return
      ;;
    crates/bot/Cargo.toml)
      add fmt; add bot; add boundaries; add integration; add supply-chain
      return
      ;;
    crates/conductor/Cargo.toml)
      add fmt; add conductor; add boundaries; add supply-chain
      return
      ;;
    crates/runtime/Cargo.toml)
      add fmt; add runtime; add boundaries; add supply-chain
      return
      ;;
    crates/updater/Cargo.toml)
      add fmt; add updater; add supply-chain
      return
      ;;
    crates/integration-tests/Cargo.toml)
      add fmt; add integration; add remote-database; add supply-chain
      return
      ;;
    Dockerfile.webui)
      add frontend
      add publish-tests
      return
      ;;
    Dockerfile.bot)
      add bot; add bootstrap; add publish-tests
      return
      ;;
    Dockerfile.api)
      add api; add fmt; add boundaries; add integration; add bootstrap; add publish-tests
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
    scripts/frontend-state-test.mjs|scripts/frontend-contrast-test.mjs|scripts/frontend-warmup-test.mjs|scripts/publish-frontend-test.mjs|scripts/publish-frontend-bunny.sh|scripts/frontend_publication.sh|scripts/package-webui.mjs|scripts/package-webui-test.mjs|scripts/administration-state-test.mjs)
      add frontend
      return
      ;;
    crates/integration-tests/tests/remote_database_smoke.rs|crates/integration-tests/tests/guild_store_smoke.rs)
      add fmt
      add remote-database
      return
      ;;
  esac
  case "$top" in
    vendor|src|migrations)
      all_rust
      ;;
    frontend)
      add frontend
      ;;
    docs)
      add contracts
      ;;
    crates)
      local crate="${path#crates/}"
      crate="${crate%%/*}"
      case "$crate" in
        api) add fmt; add api; add boundaries; add integration ;;
        bot) add fmt; add bot; add boundaries; add integration ;;
        conductor) add fmt; add conductor; add boundaries ;;
        runtime) add fmt; add runtime; add boundaries ;;
        updater) add fmt; add updater ;;
        integration-tests) add fmt; add integration ;;
        *) all_groups ;;
      esac
      ;;
    *)
      case "$path" in
        *.md|LICENSE|AGENTS.md|README.md|development.md) add contracts ;;
        *) all_groups ;;
      esac
      ;;
  esac
}

prove_diff() {
  if [ "${#files[@]}" -gt 0 ]; then
    printf '%s\n' "${files[@]}"
    return 0
  fi
  if [ "${BUILDKITE_SOURCE:-}" = "schedule" ] || [ -n "${BUILDKITE_TAG:-}" ] || [ -n "${JMODGUARD_PUBLISH_SOURCE:-}" ]; then
    return 1
  fi
  if [ "${BUILDKITE_PULL_REQUEST:-false}" != "false" ]; then
    local base="${BUILDKITE_PULL_REQUEST_BASE_BRANCH:-main}"
    git fetch --no-tags origin "$base" || return 1
    git diff --name-only "origin/${base}...HEAD" || return 1
    return 0
  fi
  if ! git rev-parse --verify --quiet 'HEAD^1' >/dev/null; then
    return 1
  fi
  git diff --name-only 'HEAD^1' HEAD
}

add contracts
if ! names="$(prove_diff)"; then
  all_groups
elif [ -z "${names}" ]; then
  all_groups
else
  while IFS= read -r path; do
    [ -n "$path" ] || continue
    classify "$path"
    [ "$full" -eq 1 ] && break
  done <<<"$names"
fi

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
