#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
loom_target_dir=${CARGO_TARGET_DIR:-target}
cargo build --locked -p loom-example-counter -p loom-example-counter-v2 --target wasm32-wasip2 --release
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
cargo run --locked -p loom-rt --example recovery_smoke -- "$loom_target_dir"/wasm32-wasip2/release/loom_example_counter.wasm "$loom_target_dir"/wasm32-wasip2/release/loom_example_counter_v2.wasm "$scratch/loom.sqlite"
