#!/usr/bin/env bash
set -euo pipefail

case "${1:-}" in
  build)
    [[ $# -eq 1 ]] || exit 2
    harness=$(
      cargo test --locked -p jmodguard-updater --bin jmodguard-launcher \
        --no-run --message-format=json-render-diagnostics |
        jq -r 'select(.reason == "compiler-artifact" and .target.name == "jmodguard-launcher"
                      and .profile.test and .executable != null) | .executable'
    )
    [[ -x "$harness" ]] || { echo 'Launcher test executable missing from Cargo output' >&2; exit 1; }
    printf '%s\n' "$harness"
    ;;
  run)
    [[ $# -eq 3 && -x "$2" ]] || { echo 'Usage: review-launcher-tests.sh run HARNESS LOG' >&2; exit 2; }
    # Kill the whole pipeline before the 120s cap, even if a fixture keeps the pipe open.
    # Libtest otherwise buffers failure diagnostics until the suite ends.
    # shellcheck disable=SC2016  # Expand the positional parameters in the child shell.
    exec timeout --signal=KILL 110s bash -o pipefail -c '
      "$1" --test-threads=4 --no-capture 2>&1 | tee -- "$2"
    ' bash "$2" "$3"
    ;;
  *)
    echo 'Usage: review-launcher-tests.sh build | run HARNESS LOG' >&2
    exit 2
    ;;
esac
