#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
loom_scratch=$(mktemp -d)
trap 'rm -rf "$loom_scratch"' EXIT
cargo run --locked --release -p loom-build --example build_smoke -- \
  "$PWD" examples/rust-dag/src/lib.rs "$loom_scratch/dag.wasm"
cargo run --locked --release -p loom-rt --example dag_smoke -- "$loom_scratch/dag.wasm"
