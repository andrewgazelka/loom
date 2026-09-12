#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
dag_scratch=$(mktemp -d)
trap 'rm -rf "$dag_scratch"' EXIT
cargo run --locked --release -p loom-build --example build_smoke -- \
  "$PWD" examples/rust-dag/src/lib.rs "$dag_scratch/dag.wasm"
cargo run --locked --release -p loom-rt --example dag_smoke -- "$dag_scratch/dag.wasm"
