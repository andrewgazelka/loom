#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
loom_root=$PWD
loom_target_dir=${CARGO_TARGET_DIR:-target}
loom_scratch=$(mktemp -d)
trap 'rm -rf "$loom_scratch"' EXIT

cargo build --locked --release -p loom-example-dag --target wasm32-wasip2
mkdir "$loom_scratch/ts"
cp examples/ts/dag.ts "$loom_scratch/ts/definition.ts"
: > "$loom_scratch/ts/dependencies.js"
# Use the production component builder's Bun entrypoint and exact input layout.
bun loom-checker/build.ts "$loom_scratch/ts" "$loom_root"
cargo run --locked --release -p loom-rt --example dag_smoke -- \
  "$loom_target_dir/wasm32-wasip2/release/loom_example_dag.wasm" \
  "$loom_scratch/ts/component.wasm"
