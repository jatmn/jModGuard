#!/usr/bin/env bash
# Automated dual-process + badword contract checks (no Discord tokens required).
# This is the local equivalent of the CI job — prefer it over ad-hoc live bins.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
export TMPDIR="${TMPDIR:-$ROOT/.tmp}"
mkdir -p "$TMPDIR"

echo "==> cargo test --locked"
cargo test --locked --workspace

echo "==> cargo test --locked -p jmodguard-api --no-default-features --lib"
cargo test --locked -p jmodguard-api --no-default-features --lib

echo "==> cargo clippy --locked --workspace --bins --lib -- -D warnings"
cargo clippy --locked --workspace --bins --lib -- -D warnings

echo "==> cargo clippy independent bot package"
cargo clippy --locked -p jmodguard-bot -- -D warnings

echo "==> dual_process_smoke (shared DB + config_version poll)"
cargo test --locked -p jmodguard-integration-tests --test dual_process_smoke -- --nocapture

echo "==> bot package builds independently"
cargo build --locked -p jmodguard-bot

echo "==> API package builds with local frontend"
cargo build --locked -p jmodguard-api

cargo clippy --locked -p jmodguard-runtime -p jmodguard-conductor --all-targets -- -D warnings
cargo build --locked -p jmodguard-conductor
python3 scripts/check-service-boundaries.py

echo "==> CI checks complete (no Discord network required)"
