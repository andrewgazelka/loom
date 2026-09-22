use std::collections::BTreeMap;

/// Canonical Merkle-root preimage of a definition: domain, export count, then
/// export path/hash pairs in path order. Length prefixes keep every pair
/// unambiguous. Exports are every definition reachable through `pub`
/// visibility from the crate root, as the compiler driver records them.
pub fn export_identity_preimage(exports: &BTreeMap<String, String>) -> Vec<u8> {
    let mut bytes = b"loom:definition:exports:v1\0".to_vec();
    bytes.extend_from_slice(&(exports.len() as u64).to_le_bytes());
    for (path, hash) in exports {
        bytes.extend_from_slice(&(path.len() as u64).to_le_bytes());
        bytes.extend_from_slice(path.as_bytes());
        bytes.extend_from_slice(&(hash.len() as u64).to_le_bytes());
        bytes.extend_from_slice(hash.as_bytes());
    }
    bytes
}
