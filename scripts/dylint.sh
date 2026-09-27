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

echo "dylint: cargo dylint --all --fail-on-no-libraries -- --locked --all-targets ${package_args[*]}"
env -u RUSTFLAGS -u CARGO_ENCODED_RUSTFLAGS \
  TMPDIR="$dylint_tmpdir" DYLINT_RUSTFLAGS="$dylint_rustflags" \
  cargo dylint --all --fail-on-no-libraries -- --locked --all-targets "${package_args[@]}"
