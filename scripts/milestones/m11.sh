#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
cargo test --locked -p loom-maintenance -p loom-store -p loom-api
if [[ -z "${LOOMD_BINARY:-}" ]]; then
  cargo build --locked -p loomd
  export LOOMD_BINARY="${CARGO_TARGET_DIR:-target}/debug/loomd"
fi
bun scripts/limits-e2e.ts
