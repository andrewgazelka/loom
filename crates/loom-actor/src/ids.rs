use anyhow::{Result, ensure};

pub type ActorId = String;

pub(crate) fn root() -> ActorId {
    format!("a0{}", ulid::Ulid::new())
}

/// Fixed-width little-endian sequence and index make the child derivation unambiguous.
pub(crate) fn child(parent: &str, seq: i64, idx: i64) -> ActorId {
    let mut hash = blake3::Hasher::new();
    hash.update(parent.as_bytes());
    hash.update(&seq.to_le_bytes());
    hash.update(&idx.to_le_bytes());
    format!("a0{}", &hash.finalize().to_hex()[..26])
}

pub(crate) fn check(id: &str) -> Result<()> {
    ensure!(id.len() == 28 && id.starts_with("a0") && id.bytes().all(|b| b.is_ascii_alphanumeric()), "actor {id} seq -1: invalid actor id");
    Ok(())
}

/// Generation zero retains the v1 delivery namespace; reset generations are distinct.
pub(crate) fn incarnation(id: &str, generation: i64) -> String {
    if generation == 0 { id.to_owned() } else { format!("{id}@{generation}") }
}
