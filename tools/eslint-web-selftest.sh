#!/bin/sh
# Prove host ESLint accepts the existing browser scripts and rejects an
# undefined name with the no-undef rule. Install first:
#   npm ci --ignore-scripts --prefix tools/eslint

set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ESLINT="$REPO_ROOT/tools/eslint/node_modules/.bin/eslint"
CONFIG="$REPO_ROOT/tools/eslint/eslint.config.js"

if ! bash "$REPO_ROOT/scripts/require-host-eslint-node.sh"; then
    echo "FAIL: host ESLint requires a supported Node.js and npm (see require-host-eslint-node.sh)" >&2
    exit 1
fi
if [ ! -x "$ESLINT" ]; then
    echo "FAIL: ESLint is not installed. Run: npm ci --ignore-scripts --prefix tools/eslint" >&2
    exit 1
fi
if [ -e "$REPO_ROOT/frontend/package.json" ] || [ -d "$REPO_ROOT/frontend/node_modules" ]; then
    echo "FAIL: host npm files must not live in frontend/ (that tree is packaged and embedded)" >&2
    exit 1
fi

cd "$REPO_ROOT"
"$ESLINT" -c "$CONFIG" frontend crates/conductor/frontend
echo "PASS: browser JavaScript matches the ESLint rules"

# Exit 1 is a reported rule error. Exit 0 means the name was allowed.
# Any other status is an ESLint failure, not proof that the rule ran.
probe_status=0
probe_out="$(printf '%s\n' 'void notDeclared;' \
    | "$ESLINT" -c "$CONFIG" --stdin --stdin-filename frontend/eslint-selftest-probe.js 2>&1)" || probe_status=$?
if [ "$probe_status" -ne 1 ] || ! printf '%s\n' "$probe_out" | grep -q 'no-undef'; then
    echo "FAIL: ESLint did not reject an undefined name (exit ${probe_status})" >&2
    printf '%s\n' "$probe_out" >&2
    exit 1
fi
echo "PASS: ESLint rejects an undefined name (frontend pattern)"

probe_status=0
probe_out="$(printf '%s\n' 'void notDeclared;' \
    | "$ESLINT" -c "$CONFIG" --stdin --stdin-filename crates/conductor/frontend/eslint-selftest-probe.js 2>&1)" || probe_status=$?
if [ "$probe_status" -ne 1 ] || ! printf '%s\n' "$probe_out" | grep -q 'no-undef'; then
    echo "FAIL: ESLint did not reject an undefined name for conductor (exit ${probe_status})" >&2
    printf '%s\n' "$probe_out" >&2
    exit 1
fi
echo "PASS: ESLint rejects an undefined name (conductor pattern)"

if ! printf '%s\n' 'const ready = true;' 'void ready;' \
    | "$ESLINT" -c "$CONFIG" --stdin --stdin-filename frontend/eslint-selftest-ok.js
then
    echo "FAIL: ESLint rejected an ordinary browser script" >&2
    exit 1
fi
echo "PASS: ESLint accepts an ordinary browser script"
echo "ESLint web self-test: PASS"
