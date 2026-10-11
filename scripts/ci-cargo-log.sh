#!/usr/bin/env bash
# Shared Cargo log helpers for CI scripts (sourced, not executed).

# Strip common ANSI SGR sequences so grep works when CARGO_TERM_COLOR=always.
ci_cargo_log_strip_ansi() {
  sed 's/\x1b\[[0-9;]*m//g'
}

# True when stripped log contains a Cargo "Compiling" status line (per-test guard).
ci_cargo_log_has_compiling() {
  local log_file=$1 stripped
  stripped="$(mktemp "${TMPDIR:-/tmp}/jmodguard-cargo-log.XXXXXX")"
  # Avoid sed|grep pipes: under pipefail, grep -q closing early yields SIGPIPE/141.
  ci_cargo_log_strip_ansi <"$log_file" >"$stripped"
  if grep -qE '^[[:space:]]*Compiling ' "$stripped"; then
    rm -f "$stripped"
    return 0
  fi
  rm -f "$stripped"
  return 1
}
