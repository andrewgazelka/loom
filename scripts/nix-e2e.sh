#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
package=${LOOM_NIX_PACKAGE:-$(nix build .#default --no-link --print-out-paths)}
exec "$package/libexec/bun" scripts/nix-e2e.ts "$package/bin/loomd"
