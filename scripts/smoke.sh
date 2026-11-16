#!/usr/bin/env bash
# Local smoke checks: builds, tests, lints, and prints diagnostics.
set -euo pipefail

cd "$(dirname "$0")/.."

echo "== cargo test =="
cargo test

echo "== cargo clippy --all-targets =="
cargo clippy --all-targets

echo "== tool catalog =="
cargo run -q -- tools

echo "== doctor =="
cargo run -q -- doctor

echo "== MCP stdio handshake =="
python3 scripts/stdio_smoke.py

echo "smoke checks passed"
