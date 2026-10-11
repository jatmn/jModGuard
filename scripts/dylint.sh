#!/usr/bin/env bash
# Run the libraries pinned in dylint.toml.
# Install cargo-dylint and dylint-link from source at the same version.
# Prebuilt binaries look for the dylint driver sources at the path where
# they were built, so `cargo binstall` fails when it has to build a driver.
set -euo pipefail

root="$(git rev-parse --show-toplevel 2>/dev/null || true)"
if [ -z "$root" ]; then
  root="$(cd "$(dirname "$0")/.." && pwd)"
fi
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

read_library_tags() {
  awk '
    /^\[\[workspace\.metadata\.dylint\.libraries\]\]/ { inlib = 1; next }
    inlib && /^\[/ { inlib = 0 }
    inlib && /^tag = "/ {
      line = $0
      sub(/^tag = "v/, "", line)
      sub(/".*$/, "", line)
      print line
    }
  ' dylint.toml
}

mapfile -t library_tags < <(read_library_tags)
if [ "${#library_tags[@]}" -eq 0 ]; then
  echo "dylint: could not read library tag from dylint.toml" >&2
  exit 1
fi
version="${library_tags[0]}"
if [ "${#library_tags[@]}" -gt 1 ]; then
  for tag in "${library_tags[@]}"; do
    if [ "$tag" != "$version" ]; then
      echo "dylint: dylint.toml has multiple library tags; use one tag for all [[workspace.metadata.dylint.libraries]] entries" >&2
      exit 1
    fi
  done
fi

# The selector supplies known workspace packages for an affected-only CI job.
# Direct/local invocations and global changes default to the full workspace.
package_args=(--workspace)
if [ -n "${JMODGUARD_DYLINT_PACKAGES:-}" ]; then
  package_args=()
  read -r -a packages <<<"$JMODGUARD_DYLINT_PACKAGES"
  for package in "${packages[@]}"; do
    case "$package" in
      jmodguard|jmodguard-api|jmodguard-bot|jmodguard-runtime|jmodguard-updater|jmodguard-integration-tests)
        package_args+=(-p "$package") ;;
      *) echo "dylint: unknown package scope: $package" >&2; exit 2 ;;
    esac
  done
  [ "${#package_args[@]}" -gt 0 ] || { echo 'dylint: empty package scope' >&2; exit 2; }
fi

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target-dylint}"

# Buildkite runs many cargo steps in parallel on one agent. Give dylint its own
# CARGO_HOME so `cargo fetch` for Trail of Bits lint libraries does not race
# shared git checkouts under the agent default home.
scratch="${TMPDIR:-$root/.tmp/dylint}"
mkdir -p "$scratch"
export CARGO_HOME="${DYLINT_CARGO_HOME:-$scratch/cargo-home}"
mkdir -p "$CARGO_HOME"

# cargo-dylint creates temporary packages under TMPDIR while resolving git
# libraries. Inside this repository those trees must stay outside the Cargo
# workspace root or Cargo walks up to the root [workspace] and rejects
# unlisted members. The rustc driver is built separately and cached under
# $HOME/.dylint_drivers (or DYLINT_DRIVER_PATH), not in this TMPDIR.
dylint_tmpdir="$(cd -P "$scratch" && pwd)"
root="$(cd -P "$root" && pwd)"
case "$dylint_tmpdir/" in
  "$root/"*) dylint_tmpdir=/tmp ;;
esac

# `cargo-dylint -V` is not a version flag: Cargo only prints it for
# `cargo dylint -V`. `dylint-link` is a linker wrapper and has no version
# flag, so read both versions from `cargo install --list`.
installed_version() {
  local crate="$1"
  cargo install --list 2>/dev/null | awk -v crate="$crate" '
    $1 == crate {
      ver = $2
      sub(/^v/, "", ver)
      sub(/:$/, "", ver)
      print ver
      exit
    }
  '
}

installed_version_root() {
  local crate="$1" install_root="$2"
  cargo install --list --root "$install_root" 2>/dev/null | awk -v crate="$crate" '
    $1 == crate {
      ver = $2
      sub(/^v/, "", ver)
      sub(/:$/, "", ver)
      print ver
      exit
    }
  '
}

dylint_lib_key_digest() {
  sha256sum "$1" | awk '{print substr($1, 1, 32)}'
}

dylint_lib_toolchain_from_key() {
  local key_file=$1 channel host vv
  channel=$(awk '/^toolchain=/ { line=$0; sub(/^toolchain=/, "", line); print line; exit }' "$key_file" | awk '{print $1}')
  vv=$(rustc "+${channel}" -vV)
  host=$(printf '%s\n' "$vv" | awk '/^host: / {print $2; exit}')
  printf '%s-%s' "$channel" "$host"
}

dylint_prebuilt_lib_dir_valid() {
  local lib_dir=$1 key_file=$2 lib_toolchain name fname so_count
  if [ ! -f "$lib_dir/key.txt" ]; then
    return 1
  fi
  if ! cmp -s "$lib_dir/key.txt" "$key_file"; then
    return 1
  fi
  lib_toolchain=$(dylint_lib_toolchain_from_key "$key_file")
  for name in "${dylint_lib_names[@]}"; do
    fname="lib${name}@${lib_toolchain}.so"
    if [ ! -s "$lib_dir/$fname" ]; then
      return 1
    fi
  done
  so_count=$(find "$lib_dir" -maxdepth 1 -type f -name '*.so' | wc -l)
  [ "$so_count" -eq 6 ]
}

pinned_root="$JMODGUARD_CI_PREFIX/dylint-6.1.0"
pinned_ok=0
if [ -f "$pinned_root/.crates.toml" ] \
  && [ -x "$pinned_root/bin/cargo-dylint" ] \
  && [ -x "$pinned_root/bin/dylint-link" ]; then
  got_cd="$(installed_version_root cargo-dylint "$pinned_root")"
  got_dl="$(installed_version_root dylint-link "$pinned_root")"
  if [ "$got_cd" = "$version" ] && [ "$got_dl" = "$version" ]; then
    pinned_ok=1
  fi
fi

if [ "$pinned_ok" -eq 1 ]; then
  echo "dylint: using pinned tools at $pinned_root (cargo-dylint $version, dylint-link $version)"
  export PATH="$pinned_root/bin:$PATH"
else
  echo "dylint: pinned tools at $pinned_root unavailable or not $version; using per-run install"
  need_install=0
  for crate in cargo-dylint dylint-link; do
    got="$(installed_version "$crate")"
    if [ "$got" != "$version" ]; then
      need_install=1
    fi
  done

  if [ "$need_install" -eq 1 ]; then
    echo "dylint: installing cargo-dylint and dylint-link ${version} from source"
    # Cargo-wide rustc flags are not for this tool build. The workspace
    # check below uses DYLINT_RUSTFLAGS.
    env -u RUSTFLAGS -u CARGO_ENCODED_RUSTFLAGS \
      cargo install cargo-dylint dylint-link --locked --version "$version" --force
  fi
fi

# `-D warnings` is a rustc flag. Arguments after `--` are forwarded to
# `cargo check`, which rejects it. `RUSTFLAGS` is the wrong channel:
# cargo-dylint 6.1.0 prepends it onto the temporary driver build, and the
# workspace `cargo check` applies it to every crate. Lint-library builds
# clear it, so they are not the leak. `DYLINT_RUSTFLAGS` reaches rustc only
# for workspace packages, because `cargo dylint` sets
# `RUSTC_WORKSPACE_WRAPPER`.
# `cargo dylint` checks the workspace with the lint libraries' nightly
# (nightly-2026-08-20 at the pinned tag). Allow `deprecated` on that check
# so a nightly-only rename does not fail a tree that still builds on 1.96.0
# under Clippy and release builds.
# Apply the gate's lint levels last: a caller may have an earlier copy of
# either flag followed by an overriding `-A warnings` or `-D deprecated`.
dylint_rustflags="${RUSTFLAGS:+$RUSTFLAGS }-D warnings -A deprecated"

use_prebuilt=0
lib_dir=""
if [ ! -d "$JMODGUARD_CI_PREFIX/dylint-libs" ]; then
  echo "dylint: prebuilt libs skipped: $JMODGUARD_CI_PREFIX/dylint-libs absent; using metadata build"
else
  key_file="$(mktemp)"
  if ! jmodguard_dylint_lib_key dylint.toml "$key_file"; then
    rm -f "$key_file"
    echo "dylint: prebuilt libs skipped: key lookup failed; using metadata build"
  else
    KEY=$(dylint_lib_key_digest "$key_file")
    lib_dir="$JMODGUARD_CI_PREFIX/dylint-libs/$KEY"
    if [ ! -d "$lib_dir" ] || ! cmp -s "$lib_dir/key.txt" "$key_file" 2>/dev/null; then
      echo "dylint: prebuilt libs skipped: $JMODGUARD_CI_PREFIX/dylint-libs/$KEY missing or key.txt mismatch; using metadata build"
    elif ! dylint_prebuilt_lib_dir_valid "$lib_dir" "$key_file"; then
      echo "dylint: prebuilt libs skipped: $JMODGUARD_CI_PREFIX/dylint-libs/$KEY does not contain exactly the 6 libraries; using metadata build"
    else
      if cargo dylint --help >/dev/null 2>&1 && cargo dylint --help | grep -Fq -- '--no-metadata'; then
        use_prebuilt=1
      else
        echo "dylint: prebuilt libs skipped: cargo dylint --help has no --no-metadata; using metadata build"
      fi
    fi
    rm -f "$key_file"
  fi
fi

if [ "$use_prebuilt" -eq 1 ]; then
  echo "dylint: using prebuilt libs at $lib_dir"
  echo "dylint: DYLINT_LIBRARY_PATH=$lib_dir"
  echo "dylint: cargo dylint --all --no-metadata --fail-on-no-libraries -- --locked --all-targets ${package_args[*]}"
  env -u RUSTFLAGS -u CARGO_ENCODED_RUSTFLAGS \
    TMPDIR="$dylint_tmpdir" DYLINT_RUSTFLAGS="$dylint_rustflags" \
    DYLINT_LIBRARY_PATH="$lib_dir" \
    cargo dylint --all --no-metadata --fail-on-no-libraries -- --locked --all-targets "${package_args[@]}"
else
  echo "dylint: cargo dylint --all --fail-on-no-libraries -- --locked --all-targets ${package_args[*]}"
  env -u RUSTFLAGS -u CARGO_ENCODED_RUSTFLAGS \
    TMPDIR="$dylint_tmpdir" DYLINT_RUSTFLAGS="$dylint_rustflags" \
    cargo dylint --all --fail-on-no-libraries -- --locked --all-targets "${package_args[@]}"
fi
