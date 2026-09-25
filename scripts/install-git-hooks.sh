#!/usr/bin/env bash
# Point this worktree at .githooks so commits and pushes run ci-preflight.sh.
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

if [ -z "$base_ref" ]; then
  echo 'install-git-hooks: --base must not be empty' >&2
  exit 1
fi

if ! git -C "$root" rev-parse --verify "${base_ref}^{commit}" >/dev/null 2>&1; then
  echo "install-git-hooks: base ref does not resolve to a commit: $base_ref" >&2
  exit 1
fi

for hook in pre-commit pre-push; do
  hook_path="$root/.githooks/$hook"
  if [ ! -f "$hook_path" ]; then
    echo "install-git-hooks: missing .githooks/$hook" >&2
    exit 1
  fi
  if [ ! -x "$hook_path" ]; then
    chmod +x "$hook_path" 2>/dev/null || true
  fi
  if [ ! -x "$hook_path" ]; then
    echo "install-git-hooks: .githooks/$hook must be executable (git skips non-executable hooks)" >&2
    exit 1
  fi
done

git -C "$root" config --local extensions.worktreeConfig true
git -C "$root" config --worktree core.hooksPath .githooks
git -C "$root" config --worktree jmodguard.preflight-base "$base_ref"
echo "Installed .githooks for this worktree (core.hooksPath) with preflight base $base_ref."
echo "Git will use only .githooks here (previous hook directories are not chained)."
echo "Each hook runs the full bash scripts/ci-preflight.sh suite."
echo "This does not add GitHub Actions."
