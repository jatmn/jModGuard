#!/usr/bin/env bash
# Point this checkout at .githooks so commits and pushes run ci-preflight.sh.
# GitHub Actions is not involved; CI remains Buildkite.
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
base_ref="origin/main"
if [ "${1:-}" = "--base" ]; then
  [ "$#" -eq 2 ] || {
    echo 'Usage: bash scripts/install-git-hooks.sh [--base origin/<base-branch>]' >&2
    exit 2
  }
  base_ref="$2"
elif [ "$#" -ne 0 ]; then
  echo 'Usage: bash scripts/install-git-hooks.sh [--base origin/<base-branch>]' >&2
  exit 2
fi

for hook in pre-commit pre-push; do
  [ -f "$root/.githooks/$hook" ] || {
    echo "install-git-hooks: missing .githooks/$hook" >&2
    exit 1
  }
done

git -C "$root" config --local core.hooksPath .githooks
git -C "$root" config --local jmodguard.preflight-base "$base_ref"
echo "Installed .githooks for this checkout (core.hooksPath) with preflight base $base_ref."
echo "Git will use only .githooks for this checkout (previous hook directories are not chained)."
echo "Each hook runs the full bash scripts/ci-preflight.sh Buildkite test job."
echo "This does not add GitHub Actions."
