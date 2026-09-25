#!/usr/bin/env bash
# Buildkite cargo jobs need a private git credential and a job-local target dir.
# Local ci-preflight runs do not use this wrapper.
set -euo pipefail

step_key="${BUILDKITE_STEP_KEY:-local}"
mkdir -p ".tmp/${step_key}"
export TMPDIR="${PWD}/.tmp/${step_key}" CARGO_TARGET_DIR="${PWD}/target-${step_key}"

case "${GITHUB_TOKEN:-}" in
  ''|'$GITHUB_TOKEN')
    export GIT_CONFIG_COUNT=1
    export GIT_CONFIG_KEY_0="url.git@github.com:.insteadof"
    export GIT_CONFIG_VALUE_0="https://github.com/"
    ;;
  *)
    export GIT_CONFIG_COUNT=1
    export GIT_CONFIG_KEY_0=credential.helper
    export GIT_CONFIG_VALUE_0="!f() { echo username=x-access-token; echo password=${GITHUB_TOKEN}; }; f"
    ;;
esac

if [ "${BUILDKITE_SOURCE:-}" = "schedule" ]; then
  export JMODGUARD_AUDIT_FAIL_CLOSED=1
fi

exec "$@"
