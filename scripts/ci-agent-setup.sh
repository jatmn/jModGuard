#!/usr/bin/env bash
# One-time re-runnable Linux agent setup for jModGuard CI pins.
# Usage:
#   sudo install -d -o buildkite-agent -g buildkite-agent /opt/jmodguard-ci
#   sudo -u buildkite-agent -H bash scripts/ci-agent-setup.sh
# from a checkout of the PR head.
# Installs: /opt/jmodguard-ci/dylint-6.1.0/bin/{cargo-dylint,dylint-link},
# /opt/jmodguard-ci/cargo-deny-0.20.2/bin/cargo-deny,
# /opt/jmodguard-ci/cargo-audit-0.22.2/bin/cargo-audit,
# and /opt/jmodguard-ci/dylint-libs/<KEY>/ (six prebuilt lint libraries plus key.txt).
# key.txt is three lines (dylint_toml_sha256, lints_rev, toolchain channel and commit-hash);
# KEY is the first 32 hex characters of sha256(key.txt).
# Re-run after any change to dylint.toml, the dylint tag named there, the nightly channel in
# examples/general/rust-toolchain.toml at that tag, or that channel's rustc commit-hash.
# cargo install (not a downloaded release tarball) is required so the driver source path is on this machine.
# This script does not print host names.
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
cd "$root"

JMODGUARD_CI_PREFIX="${JMODGUARD_CI_PREFIX:-/opt/jmodguard-ci}"

# BEGIN dylint-lib-key
# Writes key.txt to $2. $1 is the repo dylint.toml. Returns 1 on lookup failure.
jmodguard_dylint_lib_key() {
  local toml=$1 out=$2 version lints_rev channel commit_hash toml_sha
  local remote_lines peeled toolchain_toml vv
  version=$(awk '
    /^\[\[workspace\.metadata\.dylint\.libraries\]\]/ { inlib = 1; next }
    inlib && /^\[/ { inlib = 0 }
    inlib && /^tag = "/ {
      line = $0
      sub(/^tag = "v/, "", line)
      sub(/".*$/, "", line)
      print line
    }
  ' "$toml") || return 1
  [ -n "$version" ] || return 1
  [ "$(printf '%s\n' "$version" | sort -u | wc -l)" -eq 1 ] || return 1
  version=$(printf '%s\n' "$version" | head -n 1)
  toml_sha=$(sha256sum "$toml" | awk '{print $1}') || return 1
  remote_lines=$(git ls-remote https://github.com/trailofbits/dylint.git "refs/tags/v${version}") || return 1
  peeled=$(printf '%s\n' "$remote_lines" | awk '$2 ~ /\^\{\}$/ {print $1; exit}')
  if [ -n "$peeled" ]; then
    lints_rev=$peeled
  else
    lints_rev=$(printf '%s\n' "$remote_lines" | awk 'NR==1 {print $1}')
  fi
  [ -n "$lints_rev" ] || return 1
  toolchain_toml=$(curl -fsSL "https://raw.githubusercontent.com/trailofbits/dylint/${lints_rev}/examples/general/rust-toolchain.toml") || return 1
  channel=$(printf '%s\n' "$toolchain_toml" | awk -F'"' '/^channel = / {print $2; exit}')
  [ -n "$channel" ] || return 1
  vv=$(rustc "+${channel}" -vV) || return 1
  commit_hash=$(printf '%s\n' "$vv" | awk '/^commit-hash: / {print $2; exit}')
  [ -n "$commit_hash" ] || return 1
  {
    printf 'dylint_toml_sha256=%s\n' "$toml_sha"
    printf 'lints_rev=%s\n' "$lints_rev"
    printf 'toolchain=%s %s\n' "$channel" "$commit_hash"
  } >"$out"
}
# END dylint-lib-key

dylint_lib_names=(
  abs_home_path
  await_holding_span_guard
  basic_dead_store
  crate_wide_allow
  incorrect_matches_operation
  wrong_serialize_struct_arg
)

crates_list_version() {
  local crate="$1" root="$2"
  cargo install --list --root "$root" 2>/dev/null | awk -v crate="$crate" '
    $1 == crate {
      ver = $2
      sub(/^v/, "", ver)
      sub(/:$/, "", ver)
      print ver
      exit
    }
  '
}

toolchain_components_installed() {
  local channel="$1"
  shift
  local comp needle missing=0
  for comp in "$@"; do
    # rustup lists the preview component as llvm-tools-<host>.
    needle=$comp
    if [ "$comp" = llvm-tools-preview ]; then
      needle=llvm-tools
    fi
    if ! rustup component list --toolchain "$channel" | awk -v c="$needle" '$1 ~ "^" c "-" && /\(installed\)/ { found=1; exit } END { exit !found }'; then
      missing=1
    fi
  done
  [ "$missing" -eq 0 ]
}

ensure_main_toolchain() {
  local channel
  channel=$(awk -F'"' '/^channel = / {print $2; exit}' rust-toolchain.toml)
  if toolchain_components_installed "$channel" clippy rustfmt; then
    echo "toolchain-1.96.0: already-present"
  else
    rustup toolchain install "$channel" --component clippy,rustfmt
    echo "toolchain-1.96.0: installed"
  fi
}

ensure_dylint_toolchain() {
  local key_file channel
  key_file="$(mktemp)"
  if ! jmodguard_dylint_lib_key dylint.toml "$key_file"; then
    rm -f "$key_file"
    echo "ci-agent-setup: dylint key lookup failed" >&2
    exit 1
  fi
  channel=$(awk '/^toolchain=/ { line=$0; sub(/^toolchain=/, "", line); print line; exit }' "$key_file" | awk '{print $1}')
  rm -f "$key_file"
  if toolchain_components_installed "$channel" llvm-tools-preview rustc-dev; then
    echo "toolchain-${channel}: already-present"
  else
    rustup toolchain install "$channel" --component llvm-tools-preview,rustc-dev
    echo "toolchain-${channel}: installed"
  fi
}

ensure_cargo_root_tool() {
  local label="$1" crate="$2" version="$3"
  shift 3
  local root="$JMODGUARD_CI_PREFIX/${label}"
  local crate_name got need=0
  if [ -f "$root/.crates.toml" ]; then
    for crate_name in "$@"; do
      got="$(crates_list_version "$crate_name" "$root")"
      if [ "$got" != "$version" ]; then
        need=1
        break
      fi
    done
    if [ "$need" -eq 0 ]; then
      echo "${label}: already-present"
      return 0
    fi
  fi
  cargo install --locked --version "$version" --root "$root" --force "$@"
  echo "${label}: installed"
}

dylint_lib_key_digest() {
  sha256sum "$1" | awk '{print substr($1, 1, 32)}'
}

dylint_lib_toolchain_string() {
  local key_file=$1 channel host vv
  channel=$(awk '/^toolchain=/ { line=$0; sub(/^toolchain=/, "", line); print line; exit }' "$key_file" | awk '{print $1}')
  vv=$(rustc "+${channel}" -vV)
  host=$(printf '%s\n' "$vv" | awk '/^host: / {print $2; exit}')
  printf '%s-%s' "$channel" "$host"
}

dylint_lib_dir_valid() {
  local dir=$1 key_expected=$2
  local lib_toolchain name fname so_count
  if [ ! -f "$dir/key.txt" ]; then
    return 1
  fi
  if ! cmp -s "$dir/key.txt" "$key_expected"; then
    return 1
  fi
  lib_toolchain=$(dylint_lib_toolchain_string "$key_expected")
  for name in "${dylint_lib_names[@]}"; do
    fname="lib${name}@${lib_toolchain}.so"
    if [ ! -s "$dir/$fname" ]; then
      return 1
    fi
  done
  so_count=$(find "$dir" -maxdepth 1 -type f -name '*.so' | wc -l)
  [ "$so_count" -eq 6 ]
}

ensure_dylint_libs() {
  local key_file KEY dest stage aside lib_toolchain name fname src
  # EXIT runs after this function returns, so the path cannot be a local.
  JMODGUARD_SETUP_CARGO_TARGET=""
  trap 'if [ -n "${JMODGUARD_SETUP_CARGO_TARGET:-}" ] && [ -d "${JMODGUARD_SETUP_CARGO_TARGET}" ]; then rm -rf "${JMODGUARD_SETUP_CARGO_TARGET}"; fi' EXIT

  key_file="$(mktemp)"
  if ! jmodguard_dylint_lib_key dylint.toml "$key_file"; then
    rm -f "$key_file"
    echo "ci-agent-setup: dylint key lookup failed" >&2
    exit 1
  fi
  KEY=$(dylint_lib_key_digest "$key_file")
  dest="$JMODGUARD_CI_PREFIX/dylint-libs/$KEY"

  export PATH="$JMODGUARD_CI_PREFIX/dylint-6.1.0/bin:$PATH"
  lib_toolchain=$(dylint_lib_toolchain_string "$key_file")

  if dylint_lib_dir_valid "$dest" "$key_file"; then
    rm -f "$key_file"
    echo "dylint-libs/$KEY: already-present"
    return 0
  fi

  JMODGUARD_SETUP_CARGO_TARGET="$(mktemp -d)"
  export CARGO_TARGET_DIR="$JMODGUARD_SETUP_CARGO_TARGET"
  for name in "${dylint_lib_names[@]}"; do
    cargo dylint list --lib "$name"
  done

  stage="$(mktemp -d "$JMODGUARD_CI_PREFIX/.stage.XXXXXX")"
  cp "$key_file" "$stage/key.txt"
  for name in "${dylint_lib_names[@]}"; do
    fname="lib${name}@${lib_toolchain}.so"
    src="$JMODGUARD_SETUP_CARGO_TARGET/dylint/libraries/${lib_toolchain}/release/$fname"
    if [ ! -s "$src" ]; then
      echo "ci-agent-setup: missing library $fname" >&2
      rm -rf "$stage"
      rm -f "$key_file"
      exit 1
    fi
    cp "$src" "$stage/$fname"
  done
  rm -f "$key_file"

  mkdir -p "$JMODGUARD_CI_PREFIX/dylint-libs"
  exec 9>"$JMODGUARD_CI_PREFIX/dylint-libs.lock"
  flock 9
  if dylint_lib_dir_valid "$dest" "$stage/key.txt"; then
    flock -u 9
    rm -rf "$stage"
    echo "dylint-libs/$KEY: already-present"
    return 0
  fi
  if [ -d "$dest" ]; then
    aside="${dest}.replaced.$$"
    mv "$dest" "$aside"
    mv "$stage" "$dest"
    rm -rf "$aside"
  else
    mv "$stage" "$dest"
  fi
  flock -u 9
  echo "dylint-libs/$KEY: installed"
}

ensure_main_toolchain
ensure_dylint_toolchain
ensure_cargo_root_tool dylint-6.1.0 cargo-dylint 6.1.0 cargo-dylint dylint-link
ensure_cargo_root_tool cargo-deny-0.20.2 cargo-deny 0.20.2 cargo-deny
ensure_cargo_root_tool cargo-audit-0.22.2 cargo-audit 0.22.2 cargo-audit
ensure_dylint_libs

du -sh "$JMODGUARD_CI_PREFIX"/*
