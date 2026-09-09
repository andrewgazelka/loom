#!/usr/bin/env bash
set -euo pipefail
repo=$(pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/source/root/src" "$work/target"
cat > "$work/source/root/Cargo.toml" <<'MANIFEST'
[package]
name = "sandbox-witness"
version = "0.1.0"
edition = "2024"
[lib]
crate-type = ["cdylib"]
[dependencies]
serde = "1"
MANIFEST
cat > "$work/source/root/src/lib.rs" <<'RUST'
pub fn witness() -> u32 { 42 }
RUST
cat > "$work/source/root/build.rs" <<'RUST'
fn main() {
    assert!(std::env::var("LOOM_SANDBOX_SECRET").is_err());
    assert!(!std::path::Path::new("/home/andrew/.ssh").exists());
    assert!(std::net::TcpStream::connect_timeout(&"1.1.1.1:443".parse().unwrap(), std::time::Duration::from_millis(100)).is_err());
    std::fs::write(std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("sandbox-witness"), "isolated").unwrap();
}
RUST
export LOOM_SANDBOX_SECRET=must-not-enter
bash "$repo/loom-rustc/sandbox.sh" vendor "$work/source" "$work/source/root" "$work/target" "$repo"
[[ -f $work/source/root/Cargo.lock && -d $work/source/root/vendor ]]
[[ -z $(find "$work/target" -name sandbox-witness -print -quit) ]]
echo 'sandbox vendor: pinned sources and no build-script execution'
bash "$repo/loom-rustc/sandbox.sh" build "$work/source" "$work/source/root" "$work/target" "$repo"
[[ -n $(find "$work/target" -name sandbox-witness -print -quit) ]]
[[ -f $work/target/wasm32-wasip1/release/sandbox_witness.wasm ]]
echo 'sandbox 4/4: vendor, credentials, network, component'
