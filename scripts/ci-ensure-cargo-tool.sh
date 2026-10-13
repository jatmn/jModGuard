#!/usr/bin/env bash
# Shared by scripts/ci-preflight.sh and scripts/ci-preflight-test.sh.
JMODGUARD_CI_PREFIX="${JMODGUARD_CI_PREFIX:-/opt/jmodguard-ci}"

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

  if command -v "$bin" >/dev/null 2>&1; then
    path_bin="$(command -v "$bin")"
    got_ver="$(cargo_tool_version_last_field "$path_bin")"
    if [ "$got_ver" = "$version" ]; then
      out="$("$path_bin" --version 2>&1 || true)"
      echo "ci-preflight: using $bin at $path_bin ($out)"
      return 0
    fi
    echo "ci-preflight: $bin at $path_bin is version $got_ver, need $version; installing $crate $version"
  else
    echo "ci-preflight: installing $crate $version"
  fi

  tool_root="$(jmodguard_preflight_cargo_tool_root)"
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
