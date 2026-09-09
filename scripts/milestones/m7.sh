#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
: "${LOOM_TOKEN:?Set LOOM_TOKEN for a running loomd; LOOM_URL defaults to http://127.0.0.1:8787}"
bun scripts/e2e.ts
