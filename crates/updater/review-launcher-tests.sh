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
    # Leave time for termination and log output before the review tool's 120s cap.
    # Libtest otherwise buffers failure diagnostics until the suite ends.
    timeout --kill-after=2s 110s "$2" --test-threads=4 --no-capture 2>&1 | tee "$3"
    ;;
  *)
    echo 'Usage: review-launcher-tests.sh build | run HARNESS LOG' >&2
    exit 2
    ;;
esac
