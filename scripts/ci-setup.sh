#!/usr/bin/env bash
# Configure an ephemeral hosted runner; local preflight keeps its own environment.
set -euo pipefail
[[ "${GITHUB_ACTIONS:-}" == true && "${RUNNER_ENVIRONMENT:-}" == github-hosted ]] || {
  echo 'ci-setup requires a GitHub-hosted runner' >&2; exit 1;
}
case "${1:-false}" in true|false) ;; *) exit 2 ;; esac
mkdir -p "$RUNNER_TEMP/jmodguard" "$HOME/.cache/jmodguard-ci"
{
  echo "TMPDIR=$RUNNER_TEMP/jmodguard"
  echo "JMODGUARD_CI_PREFIX=$HOME/.cache/jmodguard-ci"
  echo "JMODGUARD_PREFLIGHT_CARGO_TOOL_ROOT=$HOME/.cache/jmodguard-ci/tools"
  echo "DYLINT_CARGO_HOME=$HOME/.cache/jmodguard-ci/dylint-cargo"
  echo "DYLINT_DRIVER_PATH=$HOME/.cache/jmodguard-ci/dylint-drivers"
} >>"$GITHUB_ENV"
if [[ "$1" == true ]]; then
  # Rust service/linker outputs need room; these Android SDKs are unused here.
  sudo rm -rf /usr/local/lib/android
  toolchain="$(awk -F '\"' '/^channel = / {print $2; exit}' rust-toolchain.toml)"
  [[ -n "$toolchain" ]] || exit 1
  rustup toolchain install "$toolchain" --profile minimal --component clippy,rustfmt
fi
