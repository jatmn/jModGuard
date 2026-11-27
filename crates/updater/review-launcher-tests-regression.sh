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

# The watchdog returns 124 if the review command overruns the scaled budget.
status=0
PATH="$fixture:$PATH" "$REVIEW_TEST_TIMEOUT" --foreground --kill-after=1s 4s \
  bash "$script_dir/review-launcher-tests.sh" run "$fixture/harness" "$fixture/review.log" \
  > "$fixture/console.log" 2>&1 || status=$?

diagnostic='test lingering_child panicked: fixture still holds stdout'
grep -Fxq -- "$diagnostic" "$fixture/review.log"
grep -Fxq -- "$diagnostic" "$fixture/console.log"
if [[ "$status" -ne 137 ]]; then
  printf 'Expected the launcher deadline to kill the command (137), got %s\n' "$status" >&2
  exit 1
fi
echo 'Launcher review deadline and diagnostic regression passed'
