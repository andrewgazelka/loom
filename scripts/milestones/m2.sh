#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
cargo test --locked -p loom-api --lib -- --nocapture
(cd checker && bun test)
