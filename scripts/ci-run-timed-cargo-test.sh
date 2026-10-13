#!/usr/bin/env bash
# Per-test GNU timeout helpers for scripts/ci-preflight.sh (sourced, not executed).

_run_timed_cargo_test_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/ci-cargo-log.sh
source "$_run_timed_cargo_test_dir/ci-cargo-log.sh"

JMODGUARD_TEST_TIMEOUT_SEC="${JMODGUARD_TEST_TIMEOUT_SEC:-180}"
JMODGUARD_ACTIVE_TEST_SESSION_PGID=""

jmodguard_kill_active_test_session() {
  local pgid
  pgid="${JMODGUARD_ACTIVE_TEST_SESSION_PGID:-}"
  [ -n "$pgid" ] || return 0
  kill -TERM "-$pgid" 2>/dev/null || true
  sleep 1
  kill -KILL "-$pgid" 2>/dev/null || true
  JMODGUARD_ACTIVE_TEST_SESSION_PGID=""
}

jmodguard_runner_trap_exit() {
  if [ -n "${JMODGUARD_ACTIVE_TEST_SESSION_PGID:-}" ]; then
    jmodguard_kill_active_test_session
  fi
}

jmodguard_runner_trap_signal() {
  local sig=$1
  jmodguard_kill_active_test_session
  trap - INT TERM EXIT
  if [ "$sig" -eq 2 ]; then
    exit 130
  fi
  if [ "$sig" -eq 15 ]; then
    exit 143
  fi
  exit 128
}

jmodguard_timed_cargo_install_traps() {
  if [ -n "${JMODGUARD_TIMED_CARGO_TRAPS_INSTALLED:-}" ]; then
    return 0
  fi
  JMODGUARD_TIMED_CARGO_TRAPS_INSTALLED=1
  trap 'jmodguard_runner_trap_exit' EXIT
  trap 'jmodguard_runner_trap_signal 2' INT
  trap 'jmodguard_runner_trap_signal 15' TERM
}

jmodguard_timeout_epoch_now() {
  if [ -n "${JMODGUARD_TIMEOUT_NOW_SEC:-}" ]; then
    printf '%s\n' "$JMODGUARD_TIMEOUT_NOW_SEC"
    return 0
  fi
  date +%s
}

jmodguard_timeout_poll_sleep() {
  sleep 0.2
}

# Test hook: JMODGUARD_TEST_KILL0_SEQUENCE is comma-separated 1/0 answers for leader liveness.
jmodguard_test_leader_alive_answer() {
  local next
  [ -n "${JMODGUARD_TEST_KILL0_SEQUENCE:-}" ] || return 2
  next="${JMODGUARD_TEST_KILL0_SEQUENCE%%,*}"
  if [ "$next" = "$JMODGUARD_TEST_KILL0_SEQUENCE" ]; then
    JMODGUARD_TEST_KILL0_SEQUENCE=""
  else
    JMODGUARD_TEST_KILL0_SEQUENCE="${JMODGUARD_TEST_KILL0_SEQUENCE#*,}"
  fi
  if [ "$next" = 1 ]; then
    return 0
  fi
  return 1
}

jmodguard_leader_alive() {
  local leader=$1 hook_rc=0
  if [ -n "${JMODGUARD_TEST_KILL0_SEQUENCE:-}" ]; then
    jmodguard_test_leader_alive_answer
    return $?
  fi
  kill -0 "$leader" 2>/dev/null
}

# Run a single test invocation under timeout. Uses setsid so descendant processes
# (smoke services, fixtures) share a session and are terminated on timeout.
run_with_test_timeout() {
  local name=$1
  shift
  local rc=0 leader_rc duration_sec="${JMODGUARD_TEST_TIMEOUT_SEC}" kill_after=10s leader session_pgid
  local had_errexit=0 deadline now
  case $- in *e*) had_errexit=1 ;; esac
  jmodguard_timed_cargo_install_traps
  set +o errexit
  if command -v setsid >/dev/null 2>&1; then
    setsid "$@" &
    leader=$!
    session_pgid="$(ps -o pgid= -p "$leader" 2>/dev/null | tr -d ' ')" || session_pgid=""
    JMODGUARD_ACTIVE_TEST_SESSION_PGID="$session_pgid"
    # Poll so INT/TERM traps are not deferred behind a long-running external `timeout`.
    deadline=$(( $(jmodguard_timeout_epoch_now) + duration_sec ))
    while jmodguard_leader_alive "$leader"; do
      now=$(jmodguard_timeout_epoch_now)
      if [ "$now" -ge "$deadline" ]; then
        if jmodguard_leader_alive "$leader"; then
          rc=124
          jmodguard_kill_active_test_session
          wait "$leader" 2>/dev/null || true
          JMODGUARD_ACTIVE_TEST_SESSION_PGID=""
        fi
        break
      fi
      jmodguard_timeout_poll_sleep
    done
    if [ "$rc" -eq 0 ]; then
      wait "$leader"
      leader_rc=$?
      JMODGUARD_ACTIVE_TEST_SESSION_PGID=""
      if [ "$leader_rc" -ne 0 ]; then
        rc=$leader_rc
      fi
    fi
  else
    timeout --kill-after="$kill_after" "${duration_sec}s" "$@"
    rc=$?
  fi
  if [ "$rc" -eq 124 ]; then
    echo "ci-preflight: test timed out after ${JMODGUARD_TEST_TIMEOUT_SEC}s: $name" >&2
  fi
  if [ "$had_errexit" -eq 1 ] && [ "$rc" -eq 0 ]; then
    set -o errexit
  fi
  return "$rc"
}

# Run one filtered `cargo test` under timeout; fail if Cargo starts a new compile.
run_cargo_test_timed() {
  local name=$1
  shift
  local log rc had_errexit=0
  case $- in *e*) had_errexit=1 ;; esac
  log="$(mktemp)"
  set +o errexit
  run_with_test_timeout "$name" env CARGO_TERM_COLOR=never cargo test "$@" >"$log" 2>&1
  rc=$?
  if ci_cargo_log_has_compiling "$log"; then
    cat "$log" >&2
    rm -f "$log"
    echo "ci-preflight: timed cargo test recompiled for $name" >&2
    if [ "$had_errexit" -eq 1 ] && [ "$rc" -eq 0 ]; then
      set -o errexit
    fi
    return 1
  fi
  cat "$log" >&2
  rm -f "$log"
  if [ "$had_errexit" -eq 1 ] && [ "$rc" -eq 0 ]; then
    set -o errexit
  fi
  return "$rc"
}

test_list_name() {
  local line=$1 rest name
  [ -n "$line" ] || return 1
  rest="${line#*: }"
  [ "$rest" != "$line" ] || return 1
  case "$rest" in
    test|test,*) ;;
    *) return 1 ;;
  esac
  name="${line%:*}"
  [ -n "$name" ] || return 1
  printf '%s\n' "$name"
}

doctest_run_token() {
  local name=$1
  shift
  local candidate best="" best_len=0 count n item
  local -a parts=()
  read -r -a parts <<<"$name"
  for candidate in "${parts[@]}"; do
    case "$candidate" in
      ''|-) continue ;;
      -*) continue ;;
    esac
    count=0
    for n in "$@"; do
      case "$n" in
        *"$candidate"*) count=$((count + 1)) ;;
      esac
    done
    if [ "$count" -eq 1 ] && [ "${#candidate}" -gt "$best_len" ]; then
      best=$candidate
      best_len=${#candidate}
    fi
  done
  if [ -n "$best" ]; then
    printf '%s\n' "$best"
    return 0
  fi
  item="${name#* - }"
  item="${item%% \(line *}"
  case "$item" in
    ''|*' '*|-*) return 1 ;;
  esac
  printf '%s\n' "$item"
}

run_timed_cargo_test_skip_doctests() {
  local arg
  for arg in "$@"; do
    case "$arg" in
      --lib|--test|--bins|--examples|--tests) return 0 ;;
    esac
  done
  return 1
}

run_timed_cargo_test_package_name() {
  local -a args=("$@")
  local i=0 arg pkg=""
  while [ "$i" -lt "${#args[@]}" ]; do
    arg="${args[$i]}"
    case "$arg" in
      -p|--package)
        i=$((i + 1))
        pkg="${args[$i]}"
        ;;
    esac
    i=$((i + 1))
  done
  printf '%s\n' "$pkg"
}

run_timed_cargo_test_is_workspace_doctest_scope() {
  local arg
  for arg in "$@"; do
    case "$arg" in
      --workspace|--all) return 0 ;;
    esac
  done
  [ -z "$(run_timed_cargo_test_package_name "$@")" ] && return 0
  return 1
}

# Whether to run the doctest phase: 0 = run, 1 = skip (confirmed bin-only -p), 2+ = error.
run_timed_cargo_test_may_run_doctests() {
  local pkg arg meta meta_err
  if run_timed_cargo_test_is_workspace_doctest_scope "$@"; then
    return 0
  fi
  pkg="$(run_timed_cargo_test_package_name "$@")"
  if ! command -v jq >/dev/null 2>&1; then
    echo "ci-preflight: jq is required for per-package doctest selection (-p $pkg)" >&2
    return 2
  fi
  meta="$(mktemp)"
  meta_err="$(mktemp)"
  if ! cargo metadata --no-deps --format-version 1 >"$meta" 2>"$meta_err"; then
    echo "ci-preflight: cargo metadata failed; cannot determine doctests for -p $pkg" >&2
    cat "$meta_err" >&2
    rm -f "$meta" "$meta_err"
    return 2
  fi
  if ! jq -e --arg p "$pkg" '.packages[] | select(.name == $p)' "$meta" >/dev/null; then
    echo "ci-preflight: package $pkg not found in cargo metadata" >&2
    rm -f "$meta" "$meta_err"
    return 2
  fi
  if jq -e --arg p "$pkg" '
        .packages[]
        | select(.name == $p)
        | .targets[]
        | select(.kind | index("lib"))
      ' "$meta" >/dev/null; then
    rm -f "$meta" "$meta_err"
    return 0
  fi
  rm -f "$meta" "$meta_err"
  return 1
}

# Doctest `cargo test --doc` under timeout; no compile guard (rustdoc compiles as it runs).
run_cargo_test_doc_timed() {
  local name=$1
  shift
  local log rc had_errexit=0
  case $- in *e*) had_errexit=1 ;; esac
  log="$(mktemp)"
  set +o errexit
  run_with_test_timeout "$name" env CARGO_TERM_COLOR=never cargo test "$@" >"$log" 2>&1
  rc=$?
  cat "$log" >&2
  rm -f "$log"
  if [ "$had_errexit" -eq 1 ] && [ "$rc" -eq 0 ]; then
    set -o errexit
  fi
  return "$rc"
}

run_timed_cargo_test_doctests() {
  if run_timed_cargo_test_skip_doctests "$@"; then
    return 0
  fi
  set +e
  run_timed_cargo_test_may_run_doctests "$@"
  local doctest_gate_rc=$?
  set -e
  if [ "$doctest_gate_rc" -eq 1 ]; then
    return 0
  fi
  if [ "$doctest_gate_rc" -ne 0 ]; then
    return "$doctest_gate_rc"
  fi
  if ! command -v timeout >/dev/null 2>&1; then
    cargo test "$@" --doc
    return $?
  fi
  local list_out list_rc ignored_out ignored_rc name token rc
  local -a doc_names=()
  local -A ignored_names=() covered=()
  set +e
  list_out="$(cargo test "$@" --doc -- --list --format terse)"
  list_rc=$?
  set -e
  if [ "$list_rc" -ne 0 ]; then
    return "$list_rc"
  fi
  while IFS= read -r line; do
    name="$(test_list_name "$line")" || continue
    doc_names+=("$name")
  done <<<"$list_out"
  set +e
  ignored_out="$(cargo test "$@" --doc -- --list --ignored --format terse)"
  ignored_rc=$?
  set -e
  if [ "$ignored_rc" -ne 0 ]; then
    return "$ignored_rc"
  fi
  while IFS= read -r line; do
    name="$(test_list_name "$line")" || continue
    ignored_names["$name"]=1
  done <<<"$ignored_out"
  for name in "${doc_names[@]}"; do
    [ -n "${covered[$name]+x}" ] && continue
    [ -n "${ignored_names[$name]+x}" ] && continue
    token="$(doctest_run_token "$name" "${doc_names[@]}")" || token=""
    if [ -z "$token" ]; then
      run_cargo_test_doc_timed "$name" "$@" --doc -- --test-threads=1
      return $?
    fi
    run_cargo_test_doc_timed "$name" "$@" --doc -- "$token" --test-threads=1
    rc=$?
    [ "$rc" -eq 0 ] || return "$rc"
    for n in "${doc_names[@]}"; do
      case "$n" in
        *"$token"*) covered["$n"]=1 ;;
      esac
    done
  done
}

# List and run each test through `cargo test` so cwd, CARGO_MANIFEST_DIR, and
# dynamic-library paths match a normal `cargo test` invocation.
run_timed_cargo_test_gnu() {
  local cargo_args=()
  local harness=()
  local past_dd=0 arg
  for arg in "$@"; do
    if [ "$past_dd" -eq 1 ]; then
      harness+=("$arg")
      continue
    fi
    if [ "$arg" = -- ]; then
      past_dd=1
      continue
    fi
    cargo_args+=("$arg")
  done

  local name ignored_only=0 include_ignored=0 rc list_out ignored_out
  local -a run_args=()
  local -A ignored_names=()
  for arg in "${harness[@]}"; do
    case "$arg" in
      --ignored) ignored_only=1 ;;
      --include-ignored) include_ignored=1 ;;
    esac
  done

  jmodguard_timed_cargo_install_traps

  if ! cargo test "${cargo_args[@]}" --no-run >/dev/null 2>&1; then
    cargo test "${cargo_args[@]}" --no-run
    return 1
  fi

  if [ "$ignored_only" -eq 1 ]; then
    set +e
    list_out="$(cargo test "${cargo_args[@]}" -- --list --ignored --format terse)"
    rc=$?
    set -e
    if [ "$rc" -ne 0 ]; then
      return "$rc"
    fi
    while IFS= read -r tline; do
      name="$(test_list_name "$tline")" || continue
      run_args=(--exact "$name" --ignored --test-threads=1)
      for arg in "${harness[@]}"; do
        [ "$arg" = --nocapture ] && run_args+=(--nocapture)
      done
      run_cargo_test_timed "$name" "${cargo_args[@]}" -- "${run_args[@]}"
      rc=$?
      if [ "$rc" -ne 0 ]; then
        return "$rc"
      fi
    done <<<"$list_out"
    run_timed_cargo_test_doctests "${cargo_args[@]}"
    return 0
  fi

  set +e
  ignored_out="$(cargo test "${cargo_args[@]}" -- --list --ignored --format terse)"
  rc=$?
  set -e
  if [ "$rc" -ne 0 ]; then
    return "$rc"
  fi
  while IFS= read -r tline; do
    name="$(test_list_name "$tline")" || continue
    ignored_names["$name"]=1
  done <<<"$ignored_out"

  set +e
  list_out="$(cargo test "${cargo_args[@]}" -- --list --format terse)"
  rc=$?
  set -e
  if [ "$rc" -ne 0 ]; then
    return "$rc"
  fi

  while IFS= read -r tline; do
    name="$(test_list_name "$tline")" || continue
    if [ "$include_ignored" -eq 0 ] && [ -n "${ignored_names[$name]+x}" ]; then
      continue
    fi
    if [ "$include_ignored" -eq 1 ]; then
      run_args=(--exact "$name" --include-ignored --test-threads=1)
    else
      run_args=(--exact "$name" --test-threads=1)
    fi
    for arg in "${harness[@]}"; do
      [ "$arg" = --nocapture ] && run_args+=(--nocapture)
    done
    run_cargo_test_timed "$name" "${cargo_args[@]}" -- "${run_args[@]}"
    rc=$?
    if [ "$rc" -ne 0 ]; then
      return "$rc"
    fi
  done <<<"$list_out"

  run_timed_cargo_test_doctests "${cargo_args[@]}"
}
