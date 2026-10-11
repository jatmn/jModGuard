#!/usr/bin/env bash
# Per-test GNU timeout helpers for scripts/ci-preflight.sh (sourced, not executed).

JMODGUARD_TEST_TIMEOUT_SEC="${JMODGUARD_TEST_TIMEOUT_SEC:-180}"

# Run a single test invocation under timeout. Uses setsid so descendant processes
# (smoke services, fixtures) share a session and are terminated on timeout.
run_with_test_timeout() {
  local name=$1
  shift
  local rc leader_rc duration="${JMODGUARD_TEST_TIMEOUT_SEC}s" kill_after=10s leader pgid had_errexit=0
  case $- in *e*) had_errexit=1 ;; esac
  set +o errexit
  if command -v setsid >/dev/null 2>&1; then
    setsid "$@" &
    leader=$!
    # `wait` only works on shell children; tail --pid blocks until the leader exits.
    timeout --kill-after="$kill_after" "$duration" tail --pid="$leader" -f /dev/null
    rc=$?
    if [ "$rc" -eq 124 ]; then
      pgid="$(ps -o pgid= -p "$leader" 2>/dev/null | tr -d ' ')" || pgid=""
      if [ -n "$pgid" ]; then
        kill -TERM "-$pgid" 2>/dev/null || true
        sleep 1
        kill -KILL "-$pgid" 2>/dev/null || true
      fi
      kill -KILL "$leader" 2>/dev/null || true
    fi
    wait "$leader"
    leader_rc=$?
    if [ "$rc" -ne 124 ] && [ "$leader_rc" -ne 0 ]; then
      rc=$leader_rc
    fi
  else
    timeout --kill-after="$kill_after" "$duration" "$@"
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
  local log rc
  log="$(mktemp)"
  set +e
  run_with_test_timeout "$name" cargo test "$@" >"$log" 2>&1
  rc=$?
  set -e
  if grep -qE '^   Compiling ' "$log"; then
    cat "$log" >&2
    rm -f "$log"
    echo "ci-preflight: timed cargo test recompiled for $name" >&2
    return 1
  fi
  cat "$log" >&2
  rm -f "$log"
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

run_timed_cargo_test_doctests() {
  if run_timed_cargo_test_skip_doctests "$@"; then
    return 0
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
      run_cargo_test_timed "$name" "$@" --doc -- --test-threads=1
      return $?
    fi
    run_cargo_test_timed "$name" "$@" --doc -- "$token" --test-threads=1
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
