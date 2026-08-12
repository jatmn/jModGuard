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
cargo test --locked

echo "==> cargo clippy --locked --bins -- -D warnings"
cargo clippy --locked --bins -- -D warnings

echo "==> cargo clippy slim bot (no web feature)"
cargo clippy --locked --bin jmodguard-bot --no-default-features -- -D warnings

echo "==> dual_process_smoke (shared DB + config_version poll)"
cargo test --locked --test dual_process_smoke -- --nocapture

echo "==> bot builds without web feature"
cargo build --locked --bin jmodguard-bot --no-default-features

echo "==> api builds with web feature"
cargo build --locked --bin jmodguard-api

echo "==> CI checks complete (no Discord network required)"
