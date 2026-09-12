use std::collections::BTreeMap;

/// Canonical Merkle-root preimage: domain, entry count, then name/hash pairs in
/// name order. Length prefixes keep every pair unambiguous.
pub fn entry_identity_preimage(entries: &BTreeMap<String, String>) -> Vec<u8> {
    let mut bytes = b"loom:definition:entries:v1\0".to_vec();
    bytes.extend_from_slice(&(entries.len() as u64).to_le_bytes());
    for (name, hash) in entries {
        bytes.extend_from_slice(&(name.len() as u64).to_le_bytes());
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(&(hash.len() as u64).to_le_bytes());
        bytes.extend_from_slice(hash.as_bytes());
    }
    bytes
}
