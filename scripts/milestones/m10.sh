#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
(
  cd ui
  bun install --frozen-lockfile
  bun run check
  bun run build
)
if [[ -z "${LOOM_URL:-}" ]]; then
  if [[ -z "${LOOMD_BINARY:-}" ]]; then
    cargo build --locked -p loomd
    export LOOMD_BINARY="${CARGO_TARGET_DIR:-target}/debug/loomd"
  fi
fi
bun scripts/ui-smoke.ts
