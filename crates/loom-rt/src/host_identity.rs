//! Which build of the runtime this is, as one 32-byte digest.
//!
//! Anything kept across restarts that a build of this runtime produced is only
//! valid for that build: a rebuilt host can compute a different answer for the same
//! inputs (a changed kernel, a changed guest ABI, a newer wasmtime). The digest
//! covers what identifies a build without hashing the whole file: this crate's
//! version, the wasmtime version, and the running executable's path, length and
//! modification time (the path because store-built binaries carry a fixed mtime, so
//! only their path tells two builds apart). It is computed once.
//!
//! If the executable cannot be inspected the digest is random for this process,
//! so nothing persisted by an earlier run can match: a restart starts cold, which
//! is always safe.
use std::{io, sync::OnceLock, time::UNIX_EPOCH};

/// The wasmtime this crate is pinned to. Keep in step with the `=` pin on
/// `wasmtime` in `crates/loom-rt/Cargo.toml`.
const WASMTIME_VERSION: &str = "48.0.1";

/// The identity of the running host build.
pub(crate) fn host_identity() -> &'static [u8; 32] {
    static IDENTITY: OnceLock<[u8; 32]> = OnceLock::new();
    IDENTITY.get_or_init(compute)
}

fn compute() -> [u8; 32] {
    let Ok(stamp) = executable_stamp() else {
        return *blake3::hash(uuid::Uuid::new_v4().as_bytes()).as_bytes();
    };
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"loom host identity v1");
    for part in [
        env!("CARGO_PKG_VERSION").as_bytes(),
        WASMTIME_VERSION.as_bytes(),
        stamp.as_slice(),
    ] {
        hasher.update(&(part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    *hasher.finalize().as_bytes()
}

/// The executable's path, length and modification time, as bytes.
fn executable_stamp() -> io::Result<Vec<u8>> {
    let path = std::env::current_exe()?;
    let metadata = std::fs::metadata(&path)?;
    let modified = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?;
    let mut stamp = path.as_os_str().as_encoded_bytes().to_vec();
    stamp.extend_from_slice(&metadata.len().to_le_bytes());
    stamp.extend_from_slice(&modified.as_nanos().to_le_bytes());
    Ok(stamp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wasmtime_constant_is_the_version_this_crate_pins() {
        let manifest = include_str!("../Cargo.toml");
        let pinned = manifest
            .lines()
            .find_map(|line| {
                line.trim_start()
                    .strip_prefix("wasmtime = {")?
                    .split("version = \"=")
                    .nth(1)?
                    .split('"')
                    .next()
            })
            .expect("crates/loom-rt/Cargo.toml pins wasmtime with `version = \"=x.y.z\"`");
        assert_eq!(
            pinned, WASMTIME_VERSION,
            "update WASMTIME_VERSION in host_identity.rs with the pin in Cargo.toml"
        );
    }
}
