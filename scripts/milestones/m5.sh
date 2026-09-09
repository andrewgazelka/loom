#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
loom_target_dir=${CARGO_TARGET_DIR:-target}
cargo test --locked -p loom-rt --lib
cargo build --locked -p loom-example-recursive --target wasm32-wasip2 --release
cargo run --locked -p loom-rt --example component_smoke -- "$loom_target_dir"/wasm32-wasip2/release/loom_example_recursive.wasm rust recursive

cargo build --locked -p loom-example-mailbox --target wasm32-wasip2 --release
cargo run --locked -p loom-rt --example mailbox_smoke -- "$loom_target_dir"/wasm32-wasip2/release/loom_example_mailbox.wasm
