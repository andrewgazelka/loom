#!/bin/sh
# The build worker owns the warm target directory; cargo emits JSON diagnostics.
set -eu
if [ "$#" -ne 2 ]; then
  echo 'usage: loom-rustc/build.sh CRATE_DIR TARGET_DIR' >&2
  exit 64
fi
cd "$1"
export CARGO_TARGET_DIR="$2"
# cargo-component 0.21.1 rejects plain json and consumes compiler-message records
# in json-render-diagnostics mode. Compile first through Cargo's native JSON
# stream, then adapt the already-built artifact through cargo-component.
locked=
if [ "${LOOM_LOCKED:-0}" = 1 ]; then locked=--locked; fi
cargo build $locked --release --lib --target wasm32-wasip1 --message-format=json
exec cargo component build $locked --release --lib --target wasm32-wasip1 --message-format=json-render-diagnostics
