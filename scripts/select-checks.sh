#!/usr/bin/env bash
# Select affected checks for GitHub Actions or local inspection.
# Unclassified paths and diffs that cannot be proven run every check.
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
cd "$root"

usage() {
  cat <<'EOF'
Usage: bash scripts/select-checks.sh plan [--files PATH]...
       bash scripts/select-checks.sh matrix [--files PATH]...

plan prints selected check groups. matrix emits GitHub Actions job entries.
PRs compare the event base/head with merge-base semantics. Pushes compare
all commits in the event before/after range. Missing history selects all checks.
Selection reasons go to stderr; stdout is machine-readable.
EOF
}

mode=""
files=()
while [ "$#" -gt 0 ]; do
  case "$1" in
    plan|matrix) mode="$1"; shift ;;
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
  contracts frontend fmt dylint api bot runtime updater core
  integration boundaries publish-tests bootstrap pullfrog supply-chain
  remote-database
)
selected=""
full=0
dylint_full=0

add() {
  case " ${selected} " in
    *" $1 "*) ;;
    *) selected="${selected} $1" ;;
  esac
}

all_groups() {
  echo "select-checks: all groups: $*" >&2
  full=1
  dylint_full=1
  selected=""
  local group
  for group in "${groups[@]}"; do
    add "$group"
  done
}

all_rust() {
  dylint_full=1
  local group
  for group in fmt dylint api bot runtime updater core integration boundaries supply-chain remote-database publish-tests bootstrap; do
    add "$group"
  done
}

# Root data is consumed by bot, API, updater and integration tests. Runtime
# deliberately does not import it.
# publish-tests and bootstrap stay out too. Every all_rust group this function
# does not add must stay listed in select-checks-test.sh core_omits.
core_consumers() {
  local group
  for group in fmt dylint api bot updater core integration boundaries remote-database; do
    add "$group"
  done
}

# jmodguard-runtime is a direct dependency of jmodguard (core);
# bot/API consume it through the root crate. Exercise those groups on runtime edits.
runtime_consumers() {
  add core
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
    .github/workflows/ci.yml|.github/workflows/publish.yml|scripts/select-checks.sh|scripts/select-checks-test.sh|scripts/ci-workflow-test.mjs|scripts/ci-preflight.sh|scripts/ci-setup.sh)
      all_groups "CI control changed: $path"
      return
      ;;
    scripts/publish-image.sh|scripts/publish-image-test.sh|scripts/prepare-publication.sh|scripts/prepare-publication-test.sh)
      add publish-tests
      return
      ;;
    LICENSE|LICENSE.md)
      add frontend; add publish-tests
      return
      ;;
    Cargo.lock|rust-toolchain.toml|deny.toml|Cargo.toml)
      all_rust
      return
      ;;
    dylint.toml|scripts/dylint.sh|scripts/dylint-test.sh|scripts/ci-agent-setup.sh)
      dylint_full=1
      add dylint
      return
      ;;
    scripts/ci-cargo-log.sh|scripts/ci-ensure-cargo-tool.sh|scripts/ci-preflight-test.sh|scripts/ci-run-timed-cargo-test.sh|scripts/ci-timed-cargo-test.sh)
      add supply-chain
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
    tools/webui/*)
      add frontend; add api; add publish-tests
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
    Dockerfile.api)
      add api; add fmt; add updater; add boundaries; add integration; add bootstrap; add publish-tests
      return
      ;;
    scripts/bootstrap-container.sh|scripts/bootstrap-container-test.sh)
      add bootstrap
      return
      ;;
    scripts/ci-remote-database.sh)
      add remote-database
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
  local base="${JMODGUARD_DIFF_BASE:-}" head="${JMODGUARD_DIFF_HEAD:-}"
  case "${GITHUB_EVENT_NAME:-}" in
    schedule|workflow_dispatch) return 1 ;;
    push) [ "${GITHUB_REF_TYPE:-}" != tag ] || return 1 ;;
  esac
  [[ "$base" =~ ^[0-9a-f]{40}$ && "$head" =~ ^[0-9a-f]{40}$ ]] || return 1
  git cat-file -e "${base}^{commit}" 2>/dev/null || return 1
  git cat-file -e "${head}^{commit}" 2>/dev/null || return 1
  case "${GITHUB_EVENT_NAME:-}" in
    pull_request)
      base="$(git merge-base "$base" "$head")" || return 1
      echo "select-checks: full PR diff $base..$head" >&2 ;;
    push|merge_group)
      echo "select-checks: complete event diff $base..$head" >&2 ;;
    *) return 1 ;;
  esac
  # Rename detection would hide the old path; treat a rename as delete + add.
  # NUL separators also preserve unusual filenames without skipping a surface.
  git diff --no-renames --name-only -z "$base" "$head"
}

add contracts
names="$(mktemp)"
trap 'rm -f "$names"' EXIT
if [ "${#files[@]}" -gt 0 ]; then
  printf '%s\0' "${files[@]}" >"$names"
elif ! prove_diff >"$names"; then
  all_groups "full validation requested or diff unavailable"
fi
if [ "$full" -eq 0 ]; then
  if [ ! -s "$names" ]; then
    # A proven empty diff needs only the always-reported contracts gate.
    echo 'select-checks: proven empty diff; contracts only' >&2
  else
    while IFS= read -r -d '' path; do
      classify "$path"
      [ "$full" -eq 1 ] && break
    done <"$names"
  fi
fi

# cargo deny / cargo audit stay on every plan that compiles or publishes Rust.
ensure_supply_chain() {
  local group
  for group in fmt dylint api bot runtime updater core integration boundaries remote-database publish-tests bootstrap; do
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

# Reuse the selected check targets; Cargo also compiles their dependencies.
# Remote DB has both integration-test binaries and one root-library test.
# Empty scope means the full workspace for global/tooling/fallback plans.
dylint_packages=""
if [ "$dylint_full" -eq 0 ]; then
  lint_targets="$selected"
  # API unit tests call bot code. Runtime consumers have binary/unit-test
  # targets that compiling them only as dependencies would not check.
  case " $selected " in *" bot "*) lint_targets="$lint_targets api" ;; esac
  case " $selected " in *" runtime "*) lint_targets="$lint_targets api bot updater" ;; esac
  for group in api bot runtime updater core integration; do
    wanted=0
    case " ${lint_targets} " in *" $group "*) wanted=1 ;; esac
    case "$group" in
      core|integration)
        case " ${selected} " in *" remote-database "*) wanted=1 ;; esac ;;
    esac
    if [ "$wanted" -eq 1 ]; then
      package="jmodguard-${group}"
      case "$group" in
        core) package=jmodguard ;;
        integration) package=jmodguard-integration-tests ;;
      esac
      dylint_packages="${dylint_packages:+$dylint_packages }$package"
    fi
  done
fi

# Related Rust checks share one runner/target directory instead of compiling
# the same dependency graph independently for every service.
SELECTED="$selected" LINT_PACKAGES="$dylint_packages" node --input-type=module - <<'JS'
const selected = new Set(process.env.SELECTED.trim().split(/\s+/));
const lanes = {
  quick: ["contracts", "fmt", "publish-tests", "bootstrap", "pullfrog"],
  frontend: ["frontend"],
  rust: ["api", "bot", "runtime", "updater", "core", "integration", "boundaries"],
  dylint: ["dylint"],
  "supply-chain": ["supply-chain"],
  "remote-database": ["remote-database"],
};
const include = Object.entries(lanes).flatMap(([lane, candidates]) => {
  const groups = candidates.filter(group => selected.has(group));
  if (!groups.length) return [];
  const rust = groups.some(group => !["contracts", "frontend", "publish-tests", "bootstrap", "pullfrog"].includes(group));
  return [{ lane, groups: groups.join(" "), rust,
    timeout: lane === "dylint" || lane === "rust" ? 90 : lane === "remote-database" ? 45 : 30,
    dylint_packages: lane === "dylint" ? process.env.LINT_PACKAGES : "" }];
});
console.log(JSON.stringify({ include }));
JS
