#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
cargo test --locked -p loom-rt --lib
cargo run --locked -p loom-build --example build_smoke -- "$PWD" examples/rust-recursive/src/lib.rs "$scratch/recursive.wasm"
cargo run --locked -p loom-rt --example component_smoke -- "$scratch/recursive.wasm" recursive
cargo run --locked -p loom-build --example build_smoke -- "$PWD" examples/rust-mailbox/src/lib.rs "$scratch/mailbox.wasm"
cargo run --locked -p loom-rt --example mailbox_smoke -- "$scratch/mailbox.wasm"
