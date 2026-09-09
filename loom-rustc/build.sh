#!/bin/sh
# Cargo bootstraps dependencies; the host adapts the reported core artifact.
set -eu
if [ "$#" -ne 2 ]; then
  echo 'usage: loom-rustc/build.sh CRATE_DIR TARGET_DIR' >&2
  exit 64
fi
cd "$1"
export CARGO_TARGET_DIR="$2"
export CARGO_PROFILE_RELEASE_OPT_LEVEL=2
export CARGO_PROFILE_RELEASE_LTO=false
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
export CARGO_PROFILE_RELEASE_INCREMENTAL=true
export CARGO_PROFILE_RELEASE_DEBUG=false
export RUSTC_WRAPPER="$(dirname "$0")/capture.sh"
export LOOM_RUSTC_CAPTURE="$CARGO_TARGET_DIR/root-rustc.recipe"
export LOOM_COMPILER_LIB="$(rustc --print sysroot)/lib"
mkdir -p "$CARGO_TARGET_DIR"
locked=
if [ "${LOOM_LOCKED:-0}" = 1 ]; then locked=--locked; fi
export RUSTFLAGS=
exec cargo build $locked --release --lib --target wasm32-wasip1 --message-format=json
