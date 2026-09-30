//! Bulk numbers as one byte string.
//!
//! DAG-CBOR parses every number: a float is a one-byte header plus eight bytes, so an `f32` array costs 9
//! bytes and a decode per element. `Packed<T>` carries a slice of plain numbers (or fixed arrays of them) as
//! a single CBOR byte string of little-endian values: 1 byte per byte, decoded with one bulk copy. On the
//! wire it is major type 2, so a decoder that wants to can borrow it instead; guests cannot use derive
//! macros, so the serde impls are written by hand (like [`crate::Bytes`]).
//!
//! Little-endian only (wasm is, and so is every host Loom runs on); a big-endian target is a compile error.
//! The JSON transport has no byte strings, so `deserialize` also accepts an array of byte values (as `Bytes`
//! does: `[0, 0, 128, 63]` is the one `f32` 1.0), which is how a client with only JSON can send small data.
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Visitor};

#[cfg(target_endian = "big")]
compile_error!("Packed values are little-endian on the wire");

/// A plain number or a fixed array of them, with no padding: safe to reinterpret as bytes.
///
/// # Safety
/// Implement only for types where every byte pattern is a valid value and there is no padding.
pub unsafe trait Element: Copy + 'static {}
unsafe impl Element for u8 {}
unsafe impl Element for u16 {}
unsafe impl Element for u32 {}
unsafe impl Element for u64 {}
unsafe impl Element for i8 {}
unsafe impl Element for i16 {}
unsafe impl Element for i32 {}
unsafe impl Element for i64 {}
unsafe impl Element for f32 {}
unsafe impl Element for f64 {}
unsafe impl<T: Element, const N: usize> Element for [T; N] {}

#[derive(Clone, Default, PartialEq)]
pub struct Packed<T: Element>(pub Vec<T>);

impl<T: Element> Packed<T> {
    pub fn new(values: Vec<T>) -> Self {
        Self(values)
    }
    pub fn into_inner(self) -> Vec<T> {
        self.0
    }
    /// The wire bytes of this value, without copying.
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: `Element` types have no padding and any byte pattern is valid; the length is in bytes.
        unsafe { std::slice::from_raw_parts(self.0.as_ptr().cast(), std::mem::size_of_val(&self.0[..])) }
    }
    /// Read values from bytes (one copy; the source may be unaligned). `None` when the length is not a
    /// whole number of elements.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let size = std::mem::size_of::<T>();
        if size == 0 || bytes.len() % size != 0 {
            return None;
        }
        let mut values: Vec<T> = Vec::with_capacity(bytes.len() / size);
        // SAFETY: the destination has room for `bytes.len()` bytes, `Element` accepts any bit pattern, and
        // the copy is bytewise so the source's alignment does not matter.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), values.as_mut_ptr().cast::<u8>(), bytes.len());
            values.set_len(bytes.len() / size);
        }
        Some(Self(values))
    }
}

impl<T: Element> std::fmt::Debug for Packed<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Packed({} x {})", self.0.len(), std::any::type_name::<T>())
    }
}
impl<T: Element> From<Vec<T>> for Packed<T> {
    fn from(values: Vec<T>) -> Self {
        Self(values)
    }
}
impl<T: Element> std::ops::Deref for Packed<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.0
    }
}

impl<T: Element> Serialize for Packed<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(self.as_bytes())
    }
}

impl<'de, T: Element> Deserialize<'de> for Packed<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V<T>(std::marker::PhantomData<T>);
        impl<'de, T: Element> Visitor<'de> for V<T> {
            type Value = Packed<T>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a byte string of packed little-endian values")
            }
            fn visit_bytes<E: serde::de::Error>(self, bytes: &[u8]) -> Result<Self::Value, E> {
                Packed::from_bytes(bytes).ok_or_else(|| {
                    E::custom(format!(
                        "{} bytes is not a whole number of {}-byte elements",
                        bytes.len(),
                        std::mem::size_of::<T>()
                    ))
                })
            }
            fn visit_byte_buf<E: serde::de::Error>(self, bytes: Vec<u8>) -> Result<Self::Value, E> {
                self.visit_bytes(&bytes)
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                // The hint comes from the wire and is not trusted: a 5-byte header can claim a gigabyte.
                let mut bytes = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(1 << 16));
                while let Some(byte) = seq.next_element::<u8>()? {
                    bytes.push(byte);
                }
                self.visit_bytes(&bytes)
            }
        }
        deserializer.deserialize_any(V(std::marker::PhantomData))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_numbers_are_one_byte_string_and_round_trip() {
        let positions: Packed<[f32; 3]> = Packed::new((0..1000).map(|i| [i as f32, 0.5, -1.0]).collect());
        let wire = crate::isolated::encode_payload(&(positions.clone(),)).unwrap();
        // 12 bytes per element plus a few header bytes: not 9 bytes per float.
        assert!(wire.len() < 12_100, "{} bytes", wire.len());
        let (back,): (Packed<[f32; 3]>,) = crate::isolated::decode_payload(&wire).unwrap();
        assert_eq!(back, positions);
        // Integers of other widths pack the same way.
        let triangles: Packed<[u32; 3]> = Packed::new(vec![[0, 1, 2], [2, 1, 3]]);
        let wire = crate::isolated::encode_payload(&(triangles.clone(),)).unwrap();
        let (back,): (Packed<[u32; 3]>,) = crate::isolated::decode_payload(&wire).unwrap();
        assert_eq!(back, triangles);
    }

    #[test]
    fn a_length_that_is_not_whole_elements_is_an_error_and_json_arrays_work() {
        let wire = crate::isolated::encode_payload(&(crate::Bytes::new(vec![1, 2, 3, 4, 5]),)).unwrap();
        assert!(crate::isolated::decode_payload::<(Packed<u32>,)>(&wire).is_err());
        let json = crate::encode_arguments(&serde_json::json!([[0, 0, 128, 63]])).unwrap();
        let (one,): (Packed<f32>,) = crate::isolated::decode_payload(&json).unwrap();
        assert_eq!(one.0, vec![1.0_f32]);
    }

    #[test]
    fn unaligned_sources_are_fine_and_empty_is_empty() {
        let values = [1.5f32, -2.25, 3.0];
        let mut padded = vec![0u8];
        padded.extend(values.iter().flat_map(|v| v.to_le_bytes()));
        let packed = Packed::<f32>::from_bytes(&padded[1..]).unwrap();
        assert_eq!(packed.0, values);
        assert!(Packed::<f64>::from_bytes(&[]).unwrap().0.is_empty());
    }
}
