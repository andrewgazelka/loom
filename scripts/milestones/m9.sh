#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
cargo test --locked -p loom-process --lib
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
cargo run --locked --release -p loom-build --example build_smoke -- \
  "$PWD" examples/rust-largest/src/lib.rs "$scratch/largest.wasm"
cargo run --locked --release -p loom-rt --example machine_smoke -- "$scratch/largest.wasm"
cargo test --locked -p loom-rt snapshots_are_content_keyed_and_observations_are_scoped
if [[ $(uname -s) == Linux ]]; then
  : "${LOOM_STATIC_BUSYBOX:?Set LOOM_STATIC_BUSYBOX to a statically linked busybox executable}"
  cargo test --locked -p loom-rt machine::tests::hermetic_exec_uses_snapshot_and_caches_across_calls -- --ignored --exact
else
  echo "M9 hermetic execution requires the native Linux acceptance lane with LOOM_STATIC_BUSYBOX" >&2
  exit 1
fi
