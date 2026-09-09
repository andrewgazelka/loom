#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
if [[ -n "${LOOM_URL:-}" ]]; then
  exec bun scripts/languages-e2e.ts
fi
LOOM_E2E_SUITE=languages exec scripts/native-e2e.sh
