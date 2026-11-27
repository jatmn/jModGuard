#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
fixture=$(mktemp -d)
trap 'rm -rf -- "$fixture"' EXIT

# Keep the real timeout and process-group behavior with a shorter deadline.
export REVIEW_TEST_TIMEOUT
REVIEW_TEST_TIMEOUT=$(command -v timeout)
cat > "$fixture/timeout" <<'TIMEOUT'
#!/usr/bin/env bash
set -euo pipefail
args=("$@")
for i in "${!args[@]}"; do
  if [[ "${args[i]}" == 110s ]]; then
    args[i]=1s
    exec "$REVIEW_TEST_TIMEOUT" "${args[@]}"
  fi
done
echo 'Expected a 110s launcher deadline' >&2
exit 2
TIMEOUT

cat > "$fixture/harness" <<'HARNESS'
#!/usr/bin/env bash
# This child keeps the harness's output pipe open after the harness dies.
(
  trap '' TERM
  echo 'test lingering_child panicked: fixture still holds stdout'
  exec sleep 6
) &
wait
HARNESS
chmod +x "$fixture/timeout" "$fixture/harness"

# Check the review command inside the watchdog because either timeout can return 124.
# shellcheck disable=SC2016  # Expand the positional parameters inside the watchdog.
PATH="$fixture:$PATH" "$REVIEW_TEST_TIMEOUT" --foreground --kill-after=1s 4s \
  bash -c '
    status=0
    bash "$@" || status=$?
    case "$status" in
      124|137) exit 0 ;;
    esac
    printf "Expected a launcher timeout (124 or 137), got %s\n" "$status" >&2
    exit 1
  ' bash "$script_dir/review-launcher-tests.sh" run "$fixture/harness" "$fixture/review.log" \
  > "$fixture/console.log" 2>&1 || {
    watchdog_status=$?
    cat "$fixture/console.log" >&2
    printf 'Launcher deadline check failed under the watchdog (exit %s)\n' "$watchdog_status" >&2
    exit 1
  }

diagnostic='test lingering_child panicked: fixture still holds stdout'
grep -Fxq -- "$diagnostic" "$fixture/review.log"
grep -Fxq -- "$diagnostic" "$fixture/console.log"
echo 'Launcher review deadline and diagnostic regression passed'
