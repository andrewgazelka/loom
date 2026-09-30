//! A reference to a stored object: its BLAKE3 hash and its length. A cell or kernel that produces
//! something large (a mesh, an atlas) returns this instead of the bytes; the embedder maps the object in
//! place (`Store::map_object`, `Runtime::map_blob`). The hash is the object's identity, so the reference
//! means the same on every host that holds the object, and the length lets a reader size a buffer (and a
//! GPU upload) without touching the store first.
//!
//! On the wire it is the two-element array `[hash_bytes(32), len]`, written by hand because guests cannot
//! use derive macros.
use crate::Bytes;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StoreRef {
    pub hash: [u8; 32],
    pub len: u64,
}

impl StoreRef {
    /// The hash as the 64 lowercase hex characters the store addresses objects by.
    pub fn hex(&self) -> String {
        self.hash.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

impl Serialize for StoreRef {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        (Bytes::new(self.hash.to_vec()), self.len).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for StoreRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let (hash, len) = <(Bytes, u64)>::deserialize(deserializer)?;
        let hash: [u8; 32] = hash.0[..]
            .try_into()
            .map_err(|_| serde::de::Error::custom("a store reference hash is 32 bytes"))?;
        Ok(Self { hash, len })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reference_round_trips_as_a_byte_string_and_a_length() {
        let mut hash = [0u8; 32];
        for (index, byte) in hash.iter_mut().enumerate() {
            *byte = (index * 7 + 1) as u8;
        }
        let reference = StoreRef { hash, len: 40_000_000 };
        let wire = crate::isolated::encode_payload(&(reference,)).unwrap();
        // The payload is an array of one argument; the reference inside is 0x82 (array of 2), 0x58 0x20
        // (a 32-byte string), the hash, then the length: not 32 separate numbers.
        assert!(wire.windows(3).any(|w| w == [0x82, 0x58, 0x20]), "{wire:x?}");
        assert!(wire.len() < 50, "{} bytes", wire.len());
        assert_eq!(crate::isolated::decode_payload::<(StoreRef,)>(&wire).unwrap().0, reference);
        assert_eq!(reference.hex().len(), 64);
        assert!(reference.hex().starts_with("01080f"));
        let short = crate::isolated::encode_payload(&((Bytes::new(vec![1, 2, 3]), 4u64),)).unwrap();
        assert!(crate::isolated::decode_payload::<(StoreRef,)>(&short).is_err());
    }
}
