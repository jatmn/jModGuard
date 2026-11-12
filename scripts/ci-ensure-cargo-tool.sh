#!/usr/bin/env bash
# Shared by scripts/ci-preflight.sh and scripts/ci-preflight-test.sh.
if [ -z "${JMODGUARD_CI_PREFIX:-}" ]; then
  if [ -n "${HOME:-}" ]; then
    JMODGUARD_CI_PREFIX="$HOME/.cache/jmodguard-ci"
  else
    JMODGUARD_CI_PREFIX=/opt/jmodguard-ci
  fi
fi

# Pinned supply-chain tool versions. The CI tool cache key in
# .github/workflows/ci.yml hashes this file, so a pin bump yields a new key and
# a save of the newly installed binary; a pin kept elsewhere would restore the
# old binary on every primary-key hit and reinstall each run.
# shellcheck disable=SC2034  # read by scripts/ci-preflight.sh, which sources this file
cargo_deny_version=0.20.2
# shellcheck disable=SC2034
cargo_audit_version=0.22.2

cargo_tool_version_last_field() {
  local tool="$1"
  local vout
  vout="$("$tool" --version 2>&1 || true)"
  printf '%s\n' "$vout" | awk '{print $NF}'
}

jmodguard_preflight_cargo_tool_root() {
  if [ -n "${JMODGUARD_PREFLIGHT_CARGO_TOOL_ROOT:-}" ]; then
    printf '%s\n' "$JMODGUARD_PREFLIGHT_CARGO_TOOL_ROOT"
    return 0
  fi
  local root checkout="${GITHUB_WORKSPACE:-$PWD}"
  mkdir -p "$checkout/target"
  if [ -n "${GITHUB_RUN_ID:-}" ]; then
    root="$checkout/target/ci-preflight-tools-${GITHUB_RUN_ID}"
  else
    root="$(mktemp -d "$checkout/target/ci-preflight-tools.XXXXXX")"
  fi
  JMODGUARD_PREFLIGHT_CARGO_TOOL_ROOT=$root
  export JMODGUARD_PREFLIGHT_CARGO_TOOL_ROOT
  mkdir -p "$root"
  printf '%s\n' "$root"
}

ensure_cargo_tool() {
  local bin="$1"
  local crate="$2"
  local version="$3"
  local pinned="$JMODGUARD_CI_PREFIX/${crate}-${version}/bin/${bin}"
  local pinned_ver out got_ver path_bin tool_root install_target_dir

  if [ -x "$pinned" ]; then
    pinned_ver="$(cargo_tool_version_last_field "$pinned")"
    if [ "$pinned_ver" = "$version" ]; then
      PATH="$(dirname "$pinned"):$PATH"
      export PATH
      out="$("$pinned" --version 2>&1 || true)"
      echo "ci-preflight: using pinned $bin at $pinned ($out)"
      return 0
    fi
    echo "ci-preflight: pinned $pinned is version $pinned_ver, need $version; checking PATH"
  else
    echo "ci-preflight: pinned $pinned absent; checking PATH"
  fi

  local path_seen="" path_ver=""
  if command -v "$bin" >/dev/null 2>&1; then
    path_bin="$(command -v "$bin")"
    path_seen="$path_bin"
    path_ver="$(cargo_tool_version_last_field "$path_bin")"
    got_ver="$path_ver"
    if [ "$got_ver" = "$version" ]; then
      out="$("$path_bin" --version 2>&1 || true)"
      echo "ci-preflight: using $bin at $path_bin ($out)"
      return 0
    fi
  fi

  # Hosted CI restores previously installed tools under the job-local tool
  # root (inside the actions/cache path) without putting them on PATH. Reuse
  # a matching binary instead of rerunning `cargo install`, which refreshes
  # the crates.io index and re-downloads the crate before no-op'ing with
  # "already installed".
  tool_root="$(jmodguard_preflight_cargo_tool_root)"
  if [ -x "$tool_root/bin/$bin" ]; then
    path_bin="$tool_root/bin/$bin"
    got_ver="$(cargo_tool_version_last_field "$path_bin")"
    if [ "$got_ver" = "$version" ]; then
      PATH="$tool_root/bin:$PATH"
      export PATH
      out="$("$path_bin" --version 2>&1 || true)"
      echo "ci-preflight: using cached $bin at $path_bin ($out)"
      return 0
    fi
    echo "ci-preflight: cached $path_bin is version $got_ver, need $version"
  fi

  if [ -n "$path_seen" ]; then
    echo "ci-preflight: $bin at $path_seen is version $path_ver, need $version; installing $crate $version"
  else
    echo "ci-preflight: installing $crate $version"
  fi

  install_target_dir="$tool_root/cargo-install-target"
  mkdir -p "$install_target_dir"
  if ! cargo install --locked --root "$tool_root" --target-dir "$install_target_dir" --version "$version" "$crate"; then
    rm -rf "$install_target_dir"
    echo "ci-preflight: failed to install $crate $version into $tool_root" >&2
    return 1
  fi
  rm -rf "$install_target_dir"

  PATH="$tool_root/bin:$PATH"
  export PATH

  if ! command -v "$bin" >/dev/null 2>&1; then
    echo "ci-preflight: $crate $version installed under $tool_root but $bin not found on PATH" >&2
    return 1
  fi

  path_bin="$(command -v "$bin")"
  case "$path_bin" in
    "$tool_root/bin/"*) ;;
    *)
      echo "ci-preflight: $bin resolved outside job-local root $tool_root ($path_bin)" >&2
      return 1
      ;;
  esac
  got_ver="$(cargo_tool_version_last_field "$path_bin")"
  if [ "$got_ver" != "$version" ]; then
    echo "ci-preflight: after install, $bin at $path_bin is version $got_ver, expected $version" >&2
    return 1
  fi
  out="$("$path_bin" --version 2>&1 || true)"
  echo "ci-preflight: using $bin at $path_bin ($out)"
}
