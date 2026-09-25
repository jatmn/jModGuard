#!/usr/bin/env bash
# Local alias for the full ci-preflight suite. Prefer this over ad-hoc live bins.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
exec bash "$ROOT/scripts/ci-preflight.sh" "$@"
