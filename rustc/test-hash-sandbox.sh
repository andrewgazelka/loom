#!/usr/bin/env bash
# Run with RUSTC pointing to the built tools/hash-rustc driver.
set -euo pipefail
: "${RUSTC:?set RUSTC to the absolute hash-rustc executable}"
repo=$(cd "$(dirname "$0")/.." && pwd -P)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/source/root/.cargo" "$work/source/root/vendor" "$work/target"
cp "$repo/rustc/vendor-config.toml" "$work/source/root/.cargo/config.toml"
touch "$work/source/root/Cargo.lock"
printf '%s\n' 'pub fn witness() -> u32 { 42 }' > "$work/source/root/lib.rs"
cat > "$work/target/direct.sh" <<'SCRIPT'
#!/bin/sh
set -eu
test -z "${LOOM_SANDBOX_SECRET:-}"
exec "$RUSTC" --edition=2024 --crate-type=rlib --crate-name sandbox_witness \
  lib.rs -o "$CARGO_TARGET_DIR/libwitness.rlib"
SCRIPT
export LOOM_SANDBOX_SECRET=must-not-enter
export LOOM_ITEM_HASHES="$work/target/items.json"
export LOOM_ITEM_PREIMAGES="$work/target/item-preimages"
export LOOM_DEP_ITEMS="$work/target/dependency-items"
mkdir -p "$LOOM_DEP_ITEMS"
bash "$repo/rustc/sandbox.sh" rustc "$work/source" "$work/source/root" "$work/target" "$repo"
test -s "$work/target/libwitness.rlib"
test -s "$LOOM_ITEM_HASHES"
test -n "$(find "$LOOM_ITEM_PREIMAGES" -type f -print -quit)"
echo 'sandbox driver: artifact, item hashes, preimages, cleared caller environment'
