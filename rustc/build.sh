#!/bin/sh
# Cargo bootstraps dependencies; the host adapts the reported core artifact.
set -eu
if [ "$#" -ne 2 ]; then
  echo 'usage: rustc/build.sh CRATE_DIR TARGET_DIR' >&2
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
export LOOM_COMPILER_LIB="$("${RUSTC:-rustc}" --print sysroot)/lib"
mkdir -p "$CARGO_TARGET_DIR"
locked=
if [ "${LOOM_LOCKED:-0}" = 1 ]; then locked=--locked; fi
export RUSTFLAGS="--cfg loom_core --check-cfg=cfg(loom_core) -Cno-redzone=yes -C target-feature=+atomics,+bulk-memory,+mutable-globals -Zunstable-options -Cpanic=immediate-abort -C link-arg=--shared-memory -C link-arg=--import-memory -C link-arg=--max-memory=268435456 -C link-arg=--export=__stack_low -C link-arg=--export=__stack_pointer -C link-arg=--export=__wasm_init_tls -C link-arg=--export=__tls_size -C link-arg=--export=__tls_align"
export RUSTC_BOOTSTRAP=1
exec cargo build -Zbuild-std=std,panic_abort $locked --release --lib --target "${LOOM_RUST_TARGET:-wasm32-unknown-unknown}" --message-format=json
