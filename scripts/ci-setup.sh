#!/usr/bin/env bash
# Configure an ephemeral hosted runner; local preflight keeps its own environment.
set -euo pipefail
[[ "${GITHUB_ACTIONS:-}" == true && "${RUNNER_ENVIRONMENT:-}" == github-hosted ]] || {
  echo 'ci-setup requires a GitHub-hosted runner' >&2; exit 1;
}
case "${1:-false}" in true|false) ;; *) exit 2 ;; esac
mkdir -p "$RUNNER_TEMP/jmodguard" "$HOME/.cache/jmodguard-ci/dylint-drivers"
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
if [[ "${NEED_NEXTEST:-false}" == true ]]; then
  # Prebuilt, checksum-pinned runner: avoid compiling tools or launching Cargo per test.
  nextest_archive="$RUNNER_TEMP/jmodguard/nextest.tar.gz"
  curl --fail --silent --show-error --location --retry 3 \
    --connect-timeout 10 --max-time 60 --retry-max-time 120 \
    https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-0.9.148/cargo-nextest-0.9.148-x86_64-unknown-linux-gnu.tar.gz \
    --output "$nextest_archive"
  echo "223ba936714cc861a9cdedad5a8eb664dfdd69d6fbdf6cea6aa8184f523290dc  $nextest_archive" | sha256sum --check
  mkdir -p "$RUNNER_TEMP/jmodguard/bin"
  tar -xzf "$nextest_archive" -C "$RUNNER_TEMP/jmodguard/bin" cargo-nextest
  "$RUNNER_TEMP/jmodguard/bin/cargo-nextest" --version
  echo "$RUNNER_TEMP/jmodguard/bin" >>"$GITHUB_PATH"
fi
