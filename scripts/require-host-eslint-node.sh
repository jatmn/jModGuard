#!/usr/bin/env bash
# ESLint 10 (tools/eslint) requires a supported Node.js engine. Fail closed with a
# clear message before npm ci or eslint run on underpowered agents.
set -euo pipefail

fail() {
  echo "require-host-eslint-node: $*" >&2
  exit 1
}

if ! command -v node >/dev/null 2>&1; then
  fail "missing node; install Node.js ^20.19.0, ^22.13.0, or >=24 for host ESLint"
fi
if ! command -v npm >/dev/null 2>&1; then
  fail "missing npm; install npm alongside Node for: npm ci --ignore-scripts --prefix tools/eslint"
fi

if ! node -e '
const [major, minor, patch] = process.versions.node.split(".").map((part) => Number(part));
const supported =
  major >= 24
  || (major === 22 && minor >= 13)
  || (major === 20 && minor >= 19);
if (!supported) {
  console.error(
    "unsupported Node.js " + process.version
      + " (host ESLint needs ^20.19.0, ^22.13.0, or >=24)"
  );
  process.exit(1);
}
'; then
  fail "upgrade Node.js on this host (see tools/eslint/package.json engines and docs/CI.md)"
fi
