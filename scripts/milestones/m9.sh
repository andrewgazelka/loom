#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
loom_target_dir=${CARGO_TARGET_DIR:-target}
cargo test --locked -p loom-process --lib
cargo build --locked -p loom-example-largest --target wasm32-wasip2 --release
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
cp examples/ts/largest.ts "$scratch/definition.ts"
: > "$scratch/dependencies.js"
bun loom-checker/build.ts "$scratch" "$PWD"
cargo run --locked --release -p loom-rt --example machine_smoke -- "$loom_target_dir"/wasm32-wasip2/release/loom_example_largest.wasm "$scratch/component.wasm"
cargo test --locked -p loom-rt snapshots_are_content_keyed_and_observations_are_scoped
if [[ $(uname -s) == Linux ]]; then
  : "${LOOM_STATIC_BUSYBOX:?Set LOOM_STATIC_BUSYBOX to a statically linked busybox executable}"
  cargo test --locked -p loom-rt machine::tests::hermetic_exec_uses_snapshot_and_caches_across_actors -- --ignored --exact
else
  echo "M9 hermetic execution requires the native Linux acceptance lane with LOOM_STATIC_BUSYBOX" >&2
  exit 1
fi
