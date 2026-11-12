#!/usr/bin/env bash
# Publish the pinned dylint layout that scripts/dylint.sh consumes into
# $JMODGUARD_CI_PREFIX, so later runs restored from the CI tool cache skip
# the per-run tool resolution and the lint-library metadata build:
#   $JMODGUARD_CI_PREFIX/dylint-<tag>/       cargo-dylint and dylint-link
#   $JMODGUARD_CI_PREFIX/dylint-libs/<KEY>/  the six prebuilt lint libraries
# The tools come from the install root dylint.sh used ($DYLINT_CARGO_HOME on
# hosted runners); the libraries come from the metadata build output under
# $CARGO_TARGET_DIR/dylint/libraries, keyed by the library key dylint.sh
# recorded for that build (no network lookup here). Run after
# scripts/dylint.sh; safe to repeat. Every source is checked before anything
# is written: if any is missing, the prefix is left untouched and the script
# exits non-zero, because actions/cache saves only after a successful job and
# never re-saves a primary key, so an incomplete entry would otherwise stay
# for every later run on that key.
# dylint.sh itself never mutates this layout (contract in dylint-test.sh);
# this script is the only writer.
# shellcheck disable=SC2154  # version, dylint_pinned_root, dylint_build_key,
# CARGO_HOME, CARGO_TARGET_DIR and the key/library helpers come from the
# sourced prefix of scripts/dylint.sh.
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

fail() {
  echo "ci-dylint-libs: $*; nothing published" >&2
  exit 1
}

# Check every source first; nothing below writes until all checks pass.
pinned="$dylint_pinned_root"
publish_tools=1
if [ -f "$pinned/.crates.toml" ] && [ -x "$pinned/bin/cargo-dylint" ] && [ -x "$pinned/bin/dylint-link" ] \
  && got_cd="$(installed_version_root cargo-dylint "$pinned")" \
  && got_dl="$(installed_version_root dylint-link "$pinned")" \
  && [ "$got_cd" = "$version" ] && [ "$got_dl" = "$version" ]; then
  publish_tools=0
  echo "ci-dylint-libs: pinned tools already published at $pinned"
else
  # dylint.sh installs into (or finds the tools under) its own CARGO_HOME.
  if [ ! -f "$CARGO_HOME/.crates.toml" ] \
    || [ ! -x "$CARGO_HOME/bin/cargo-dylint" ] \
    || [ ! -x "$CARGO_HOME/bin/dylint-link" ]; then
    fail "no dylint tools under $CARGO_HOME"
  fi
  if ! got_cd="$(installed_version_root cargo-dylint "$CARGO_HOME")" \
    || ! got_dl="$(installed_version_root dylint-link "$CARGO_HOME")" \
    || [ "$got_cd" != "$version" ] || [ "$got_dl" != "$version" ]; then
    fail "tools under $CARGO_HOME are not $version"
  fi
fi

key_file="$dylint_build_key"
if [ ! -s "$key_file" ]; then
  fail "no library key recorded at $key_file (scripts/dylint.sh records it before building)"
fi
key="$(dylint_lib_key_digest "$key_file")"
lib_dir="$JMODGUARD_CI_PREFIX/dylint-libs/$key"
publish_libs=1
if dylint_prebuilt_lib_dir_valid "$lib_dir" "$key_file"; then
  publish_libs=0
  echo "ci-dylint-libs: prebuilt libs already published at $lib_dir"
else
  # dylint's metadata build copies each library next to its plain build
  # output as lib<name>@<toolchain>.so; the toolchain directory is the
  # channel-host pair recorded in the key.
  if ! lib_toolchain="$(dylint_lib_toolchain_from_key "$key_file")"; then
    fail "lint toolchain unavailable"
  fi
  src_dir="$CARGO_TARGET_DIR/dylint/libraries/$lib_toolchain/release"
  for name in "${dylint_lib_names[@]}"; do
    fname="lib${name}@${lib_toolchain}.so"
    if [ ! -s "$src_dir/$fname" ]; then
      fail "$src_dir has no $fname"
    fi
  done
fi

if [ "$publish_tools" -eq 1 ]; then
  mkdir -p "$pinned/bin"
  cp "$CARGO_HOME/bin/cargo-dylint" "$CARGO_HOME/bin/dylint-link" "$pinned/bin/"
  cp "$CARGO_HOME/.crates.toml" "$pinned/.crates.toml"
  if [ -f "$CARGO_HOME/.crates.json" ]; then
    cp "$CARGO_HOME/.crates.json" "$pinned/.crates.json"
  fi
  echo "ci-dylint-libs: published pinned dylint $version tools to $pinned"
fi

if [ "$publish_libs" -eq 1 ]; then
  # Stage the whole directory before it becomes visible under the computed
  # key, then swap it in with one rename so a concurrent or interrupted run
  # can never observe a half-written entry as valid. Only an entry that
  # failed validation above is ever replaced.
  mkdir -p "$JMODGUARD_CI_PREFIX/dylint-libs"
  staged="$(mktemp -d "$JMODGUARD_CI_PREFIX/dylint-libs/.publish.XXXXXX")"
  for name in "${dylint_lib_names[@]}"; do
    fname="lib${name}@${lib_toolchain}.so"
    cp "$src_dir/$fname" "$staged/$fname"
  done
  cp "$key_file" "$staged/key.txt"
  rm -rf "$lib_dir"
  mv "$staged" "$lib_dir"
  echo "ci-dylint-libs: published prebuilt lint libraries to $lib_dir"
fi
