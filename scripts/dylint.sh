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

version="6.1.0"

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
  cargo install cargo-dylint dylint-link --locked --version "$version" --force
fi

# `-D warnings` must arrive through RUSTFLAGS. Arguments after `--` are
# forwarded to `cargo check`, which rejects `-D warnings` as a cargo flag.
# The lint libraries typecheck with their own nightly, which is newer than
# this repo's pinned toolchain, so allow `deprecated`: a rename that exists
# only on that nightly is not a failure of the project compiler.
if [ -z "${RUSTFLAGS:-}" ]; then
  export RUSTFLAGS='-D warnings -A deprecated'
fi

echo "dylint: cargo dylint --all -- --locked --all-targets --workspace"
cargo dylint --all -- --locked --all-targets --workspace
