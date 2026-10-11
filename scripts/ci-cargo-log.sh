#!/usr/bin/env bash
# Shared Cargo log helpers for CI scripts (sourced, not executed).

# Strip common ANSI SGR sequences so grep works when CARGO_TERM_COLOR=always.
ci_cargo_log_strip_ansi() {
  sed 's/\x1b\[[0-9;]*m//g'
}

# True when stripped log contains a Cargo "Compiling" status line (per-test guard).
ci_cargo_log_has_compiling() {
  local log_file=$1
  ci_cargo_log_strip_ansi <"$log_file" | grep -qE '^[[:space:]]*Compiling '
}
