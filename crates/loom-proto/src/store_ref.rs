//! A reference to a stored object: its BLAKE3 hash and its length. A cell or kernel that produces
//! something large (a mesh, an atlas) returns this instead of the bytes; the embedder maps the object in
//! place (`Store::map_object`, `Runtime::map_blob`). The hash is the object's identity, so the reference
//! means the same on every host that holds the object, and the length lets a reader size a buffer (and a
//! GPU upload) without touching the store first.
//!
//! On the wire it is the two-element array `["<64 hex characters>", len]`: text, so a reference survives every
//! transport (the JSON `Value` a reply is decoded into refuses byte strings) and reads in a log. Deserializing
//! also accepts the hash as a byte string or an array of 32 numbers. Written by hand because guests cannot use
//! derive macros.
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Visitor};

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
        (self.hex(), self.len).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for StoreRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // The hash as text (what we write) or as bytes (a byte string or an array of numbers).
        struct Hash([u8; 32]);
        impl<'de> Deserialize<'de> for Hash {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct V;
                impl<'de> Visitor<'de> for V {
                    type Value = Hash;
                    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                        f.write_str("a 64-character hex hash or 32 bytes")
                    }
                    fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<Hash, E> {
                        let bytes = text.as_bytes();
                        if bytes.len() != 64 {
                            return Err(E::custom("a store reference hash is 64 hex characters"));
                        }
                        let mut out = [0u8; 32];
                        for (index, pair) in bytes.chunks(2).enumerate() {
                            let digit = |c: u8| (c as char).to_digit(16);
                            match (digit(pair[0]), digit(pair[1])) {
                                (Some(high), Some(low)) => out[index] = (high * 16 + low) as u8,
                                _ => return Err(E::custom("a store reference hash is hex")),
                            }
                        }
                        Ok(Hash(out))
                    }
                    fn visit_bytes<E: serde::de::Error>(self, bytes: &[u8]) -> Result<Hash, E> {
                        <[u8; 32]>::try_from(bytes)
                            .map(Hash)
                            .map_err(|_| E::custom("a store reference hash is 32 bytes"))
                    }
                    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Hash, A::Error> {
                        let mut bytes = Vec::with_capacity(32);
                        while let Some(byte) = seq.next_element::<u8>()? {
                            bytes.push(byte);
                        }
                        self.visit_bytes(&bytes)
                    }
                }
                deserializer.deserialize_any(V)
            }
        }
        let (Hash(hash), len) = <(Hash, u64)>::deserialize(deserializer)?;
        Ok(Self { hash, len })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> StoreRef {
        let mut hash = [0u8; 32];
        for (index, byte) in hash.iter_mut().enumerate() {
            *byte = (index * 7 + 1) as u8;
        }
        StoreRef { hash, len: 40_000_000 }
    }

    #[test]
    fn a_reference_is_text_and_a_length_and_survives_the_value_path() {
        let reference = sample();
        assert_eq!(reference.hex().len(), 64);
        assert!(reference.hex().starts_with("01080f"));
        let wire = crate::isolated::encode_payload(&(reference,)).unwrap();
        assert!(wire.len() < 90, "{} bytes", wire.len());
        assert_eq!(crate::isolated::decode_payload::<(StoreRef,)>(&wire).unwrap().0, reference);
        // The JSON `Value` path (a reply to an HTTP client) carries it: no byte strings on the wire.
        let as_value: crate::Value = crate::decode(&crate::encode(&reference).unwrap()).unwrap();
        assert_eq!(as_value[0], reference.hex());
        assert_eq!(as_value[1], 40_000_000);
    }

    #[test]
    fn a_reference_also_reads_bytes_and_number_arrays_and_refuses_the_wrong_size() {
        let reference = sample();
        let numbers = crate::encode_arguments(&serde_json::json!([[reference.hash.to_vec(), 40_000_000]])).unwrap();
        assert_eq!(crate::isolated::decode_payload::<(StoreRef,)>(&numbers).unwrap().0, reference);
        let bytes = crate::isolated::encode_payload(&((crate::Bytes::new(reference.hash.to_vec()), 40_000_000u64),)).unwrap();
        assert_eq!(crate::isolated::decode_payload::<(StoreRef,)>(&bytes).unwrap().0, reference);
        let short = crate::isolated::encode_payload(&((crate::Bytes::new(vec![1, 2, 3]), 4u64),)).unwrap();
        assert!(crate::isolated::decode_payload::<(StoreRef,)>(&short).is_err());
        let text = crate::encode_arguments(&serde_json::json!([["zz", 4]])).unwrap();
        assert!(crate::isolated::decode_payload::<(StoreRef,)>(&text).is_err());
    }
}
