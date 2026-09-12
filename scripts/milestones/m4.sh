#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
cargo run --locked -p loom-build --example build_smoke -- "$PWD" examples/rust-counter/src/lib.rs "$scratch/counter.wasm"
cargo run --locked -p loom-build --example build_smoke -- "$PWD" examples/rust-counter-v2/src/lib.rs "$scratch/counter-v2.wasm"
cargo run --locked -p loom-rt --example recovery_smoke -- "$scratch/counter.wasm" "$scratch/counter-v2.wasm" "$scratch/loom.sqlite"
