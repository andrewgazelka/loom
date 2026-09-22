//! A byte string that serializes as CBOR major type 2, not as an array of
//! integers. Guests cannot use derive or proc macros, so this hand-written
//! newtype is the only way a `Vec<u8>` crosses the isolated-call boundary as
//! bytes. `Vec<u8>` itself still serializes as an array of numbers.
use serde::{Deserialize, Serialize, de::Visitor};

#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Bytes(pub Vec<u8>);

impl Bytes {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
    pub fn into_inner(self) -> Vec<u8> {
        self.0
    }
}
impl std::fmt::Debug for Bytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Bytes({} bytes)", self.0.len())
    }
}
impl From<Vec<u8>> for Bytes {
    fn from(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
}
impl From<&[u8]> for Bytes {
    fn from(bytes: &[u8]) -> Self {
        Self(bytes.to_vec())
    }
}
impl From<Bytes> for Vec<u8> {
    fn from(bytes: Bytes) -> Self {
        bytes.0
    }
}
impl std::ops::Deref for Bytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}
impl std::ops::DerefMut for Bytes {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.0
    }
}
impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl Serialize for Bytes {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.0)
    }
}
impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct BytesVisitor;
        impl<'de> Visitor<'de> for BytesVisitor {
            type Value = Bytes;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a byte string")
            }
            fn visit_bytes<E: serde::de::Error>(self, bytes: &[u8]) -> Result<Bytes, E> {
                Ok(Bytes(bytes.to_vec()))
            }
            fn visit_borrowed_bytes<E: serde::de::Error>(
                self,
                bytes: &'de [u8],
            ) -> Result<Bytes, E> {
                Ok(Bytes(bytes.to_vec()))
            }
            fn visit_byte_buf<E: serde::de::Error>(self, bytes: Vec<u8>) -> Result<Bytes, E> {
                Ok(Bytes(bytes))
            }
            // The host `Value` API (JSON) has no byte string; it hands a
            // `Vec<u8>` as an array of numbers. `deserialize_any` below lets
            // the DAG-CBOR decoder dispatch a byte string to `visit_byte_buf`
            // and an array to this arm; `deserialize_byte_buf` would refuse
            // the array outright.
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Bytes, A::Error> {
                let mut bytes = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(1 << 16));
                while let Some(byte) = seq.next_element::<u8>()? {
                    bytes.push(byte);
                }
                Ok(Bytes(bytes))
            }
        }
        deserializer.deserialize_any(BytesVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bytes_are_major_type_two_and_json_arrays_still_decode() {
        let value = Bytes::from(vec![1u8, 2, 255]);
        let wire = serde_ipld_dagcbor::to_vec(&value).unwrap();
        assert_eq!(wire, [0x43, 1, 2, 255]);
        let back: Bytes = serde_ipld_dagcbor::from_slice(&wire).unwrap();
        assert_eq!(back, value);
        let from_json: Bytes = serde_json::from_value(serde_json::json!([1, 2, 255])).unwrap();
        assert_eq!(from_json, value);
        assert!(serde_json::from_value::<Bytes>(serde_json::json!([256])).is_err());
        // A plain Vec<u8> stays an array of numbers: the newtype is the opt-in.
        assert_eq!(
            serde_ipld_dagcbor::to_vec(&vec![1u8, 2]).unwrap(),
            [0x82, 1, 2]
        );
    }
}
