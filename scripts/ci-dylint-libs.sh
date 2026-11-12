#!/usr/bin/env bash
# Publish the pinned dylint layout that scripts/dylint.sh consumes into
# $JMODGUARD_CI_PREFIX, so later runs restored from the CI tool cache skip
# the per-run tool resolution and the lint-library metadata build:
#   $JMODGUARD_CI_PREFIX/dylint-<tag>/       cargo-dylint and dylint-link
#   $JMODGUARD_CI_PREFIX/dylint-libs/<KEY>/  the six prebuilt lint libraries
# The tools come from the install root dylint.sh used ($DYLINT_CARGO_HOME on
# hosted runners); the libraries come from the metadata build output under
# $CARGO_TARGET_DIR/dylint/libraries. Run after scripts/dylint.sh; safe to
# repeat. Absent sources or a failed key lookup leave the cache untouched.
# dylint.sh itself never mutates this layout (contract in dylint-test.sh);
# this script is the only writer.
# shellcheck disable=SC2154  # version, CARGO_HOME, CARGO_TARGET_DIR and the
# key/library helpers come from the sourced prefix of scripts/dylint.sh.
set -euo pipefail

root="$(git rev-parse --show-toplevel 2>/dev/null || true)"
if [ -z "$root" ]; then
  root="$(cd "$(dirname "$0")/.." && pwd)"
fi
cd "$root"

# Load dylint.sh's helpers and the environment it used (tag version,
# CARGO_HOME, CARGO_TARGET_DIR, library names, key helpers) without running
# the dylint driver: everything before dylint.sh's own pinned_root lookup.
# dylint-test.sh cuts the same file at `mapfile -t library_tags`; both cuts
# must stay before dylint.sh starts resolving its pinned tools and libraries.
dylint_setup="$(mktemp)"
trap 'rm -f "$dylint_setup"' EXIT
sed '/^pinned_root=/,$d' scripts/dylint.sh >"$dylint_setup"
# shellcheck source=/dev/null
source "$dylint_setup"

publish_pinned_dylint_tools() {
  local pinned="$JMODGUARD_CI_PREFIX/dylint-${version}"
  local got_cd got_dl

  if [ -f "$pinned/.crates.toml" ] && [ -x "$pinned/bin/cargo-dylint" ] && [ -x "$pinned/bin/dylint-link" ]; then
    if got_cd="$(installed_version_root cargo-dylint "$pinned")" \
      && got_dl="$(installed_version_root dylint-link "$pinned")" \
      && [ "$got_cd" = "$version" ] && [ "$got_dl" = "$version" ]; then
      echo "ci-dylint-libs: pinned tools already published at $pinned"
      return 0
    fi
  fi

  # dylint.sh installs into (or finds the tools under) its own CARGO_HOME.
  if [ ! -f "$CARGO_HOME/.crates.toml" ] \
    || [ ! -x "$CARGO_HOME/bin/cargo-dylint" ] \
    || [ ! -x "$CARGO_HOME/bin/dylint-link" ]; then
    echo "ci-dylint-libs: no dylint tools under $CARGO_HOME; pinned tools not published"
    return 0
  fi
  if ! got_cd="$(installed_version_root cargo-dylint "$CARGO_HOME")" \
    || ! got_dl="$(installed_version_root dylint-link "$CARGO_HOME")" \
    || [ "$got_cd" != "$version" ] || [ "$got_dl" != "$version" ]; then
    echo "ci-dylint-libs: tools under $CARGO_HOME are not $version; pinned tools not published"
    return 0
  fi

  mkdir -p "$pinned/bin"
  cp "$CARGO_HOME/bin/cargo-dylint" "$CARGO_HOME/bin/dylint-link" "$pinned/bin/"
  cp "$CARGO_HOME/.crates.toml" "$pinned/.crates.toml"
  if [ -f "$CARGO_HOME/.crates.json" ]; then
    cp "$CARGO_HOME/.crates.json" "$pinned/.crates.json"
  fi
  echo "ci-dylint-libs: published pinned dylint $version tools to $pinned"
}

publish_prebuilt_dylint_libs() {
  local key_file key lib_dir lib_toolchain src_dir staged name fname

  key_file="$(mktemp)"
  if ! jmodguard_dylint_lib_key dylint.toml "$key_file"; then
    rm -f "$key_file"
    echo "ci-dylint-libs: library key lookup failed; prebuilt libs not published"
    return 0
  fi
  key="$(dylint_lib_key_digest "$key_file")"
  lib_dir="$JMODGUARD_CI_PREFIX/dylint-libs/$key"

  if [ -f "$lib_dir/key.txt" ] && cmp -s "$lib_dir/key.txt" "$key_file" \
    && dylint_prebuilt_lib_dir_valid "$lib_dir" "$key_file"; then
    rm -f "$key_file"
    echo "ci-dylint-libs: prebuilt libs already published at $lib_dir"
    return 0
  fi

  # dylint's metadata build copies each library next to its plain build
  # output as lib<name>@<toolchain>.so; the toolchain directory is the
  # channel-host pair recorded in the key.
  if ! lib_toolchain="$(dylint_lib_toolchain_from_key "$key_file")"; then
    rm -f "$key_file"
    echo "ci-dylint-libs: lint toolchain unavailable; prebuilt libs not published"
    return 0
  fi
  src_dir="$CARGO_TARGET_DIR/dylint/libraries/$lib_toolchain/release"
  for name in "${dylint_lib_names[@]}"; do
    fname="lib${name}@${lib_toolchain}.so"
    if [ ! -s "$src_dir/$fname" ]; then
      rm -f "$key_file"
      echo "ci-dylint-libs: $src_dir has no $fname; prebuilt libs not published"
      return 0
    fi
  done

  # Stage the whole directory before it becomes visible under the computed
  # key, then swap it in with one rename so a concurrent or interrupted run
  # can never observe a half-written entry as valid.
  mkdir -p "$JMODGUARD_CI_PREFIX/dylint-libs"
  staged="$(mktemp -d "$JMODGUARD_CI_PREFIX/dylint-libs/.publish.XXXXXX")"
  for name in "${dylint_lib_names[@]}"; do
    fname="lib${name}@${lib_toolchain}.so"
    cp "$src_dir/$fname" "$staged/$fname"
  done
  cp "$key_file" "$staged/key.txt"
  rm -f "$key_file"
  rm -rf "$lib_dir"
  mv "$staged" "$lib_dir"
  echo "ci-dylint-libs: published prebuilt lint libraries to $lib_dir"
}

publish_pinned_dylint_tools
publish_prebuilt_dylint_libs
