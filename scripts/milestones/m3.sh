#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
cargo run --locked -p loom-build --example build_smoke -- "$PWD" examples/rust-crash/src/lib.rs "$scratch/crash.wasm"
cargo run --locked -p loom-rt --example crash_smoke -- parent "$scratch/crash.wasm"
