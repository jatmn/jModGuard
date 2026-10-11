#!/usr/bin/env bash
# Shared by scripts/ci-preflight.sh and scripts/ci-preflight-test.sh.
JMODGUARD_CI_PREFIX="${JMODGUARD_CI_PREFIX:-/opt/jmodguard-ci}"

cargo_tool_version_last_field() {
  local tool="$1"
  local vout
  vout="$("$tool" --version 2>&1 || true)"
  printf '%s\n' "$vout" | awk '{print $NF}'
}

ensure_cargo_tool() {
  local bin="$1"
  local crate="$2"
  local version="$3"
  local pinned="$JMODGUARD_CI_PREFIX/${crate}-${version}/bin/${bin}"
  local pinned_ver out got_ver path_bin cargo_bin

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

  if ! cargo install "$crate" --locked --version "$version"; then
    echo "ci-preflight: failed to install $crate $version" >&2
    return 1
  fi

  cargo_bin="${CARGO_HOME:-$HOME/.cargo}/bin"
  if [ -d "$cargo_bin" ]; then
    PATH="$cargo_bin:$PATH"
    export PATH
  fi

  if ! command -v "$bin" >/dev/null 2>&1; then
    echo "ci-preflight: $crate $version installed but $bin not found on PATH" >&2
    return 1
  fi

  path_bin="$(command -v "$bin")"
  got_ver="$(cargo_tool_version_last_field "$path_bin")"
  if [ "$got_ver" != "$version" ]; then
    echo "ci-preflight: after install, $bin at $path_bin is version $got_ver, expected $version" >&2
    return 1
  fi
  out="$("$path_bin" --version 2>&1 || true)"
  echo "ci-preflight: using $bin at $path_bin ($out)"
}
