#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
loom_target_dir=${CARGO_TARGET_DIR:-target}
cargo build --locked -p loom-example-crash --target wasm32-wasip2 --release
cargo run --locked -p loom-rt --example crash_smoke -- parent "$loom_target_dir"/wasm32-wasip2/release/loom_example_crash.wasm
