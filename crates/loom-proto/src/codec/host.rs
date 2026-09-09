//! Direct typed transport for values produced by the host. Foreign bytes still
//! enter through `decode`, which verifies canonical identity before admission.
use serde::{Serialize, de::{self, DeserializeOwned, DeserializeSeed, Visitor, IntoDeserializer}};
use ipld_core::cid::Cid;

pub(crate) mod sealed { pub trait HostValue {} }
/// Types whose serializers already implement the canonical Value contract.
/// Sealed so arbitrary serializers cannot bypass reference/number admission.
pub trait HostValue: Serialize + sealed::HostValue {}
impl<T: HostValue> sealed::HostValue for Vec<T> {}
impl<T: HostValue> HostValue for Vec<T> {}
impl<T: HostValue> sealed::HostValue for [T] {}
impl<T: HostValue> HostValue for [T] {}

pub fn encode_host<T: ?Sized + HostValue>(value: &T) -> Result<Vec<u8>, String> {
    serde_ipld_dagcbor::to_vec(value).map_err(|error| error.to_string())
}

/// Compose already canonical host results without decoding their trees.
/// Every child must originate from `encode`, `encode_host`, or a verified store
/// record. This function is not an admission boundary for arbitrary bytes.
pub fn encode_host_array<'a>(values: impl IntoIterator<Item = &'a [u8], IntoIter: ExactSizeIterator>) -> Result<Vec<u8>, String> {
    let values = values.into_iter();
    // CBOR integer and array headers share the same canonical length argument;
    // the maintained encoder chooses its width, and this boundary sets major 4.
    let mut bytes = serde_ipld_dagcbor::to_vec(&(values.len() as u64)).map_err(|error| error.to_string())?;
    bytes[0] |= 0x80;
    for value in values { bytes.extend_from_slice(value); }
    Ok(bytes)
}

/// Deserialize host-produced bytes directly into the requested Rust type. The
/// streaming adapter keeps CID references and numeric/depth checks while avoiding
/// an intermediate Value/IPLD tree and a second canonical serialization pass.
pub fn decode_host<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    let mut decoder = serde_ipld_dagcbor::de::Deserializer::from_slice(bytes);
    let result = T::deserialize(HostDeserializer { inner: &mut decoder, depth: 0 }).map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())?;
    Ok(result)
}
struct HostDeserializer<D> { inner: D, depth: usize }
macro_rules! deserialize_method {
    ($method:ident $(, $arg:ident : $ty:ty)*) => {
        fn $method<V: Visitor<'de>>(self, $($arg: $ty,)* visitor: V) -> Result<V::Value, Self::Error> {
            if self.depth > 256 { return Err(de::Error::custom("Value exceeds maximum depth 256")); }
            self.inner.$method($($arg,)* HostVisitor { inner: visitor, depth: self.depth })
        }
    };
}
impl<'de, D: de::Deserializer<'de>> de::Deserializer<'de> for HostDeserializer<D> {
    type Error = D::Error;
    deserialize_method!(deserialize_any);
    deserialize_method!(deserialize_bool);
    deserialize_method!(deserialize_i8);
    deserialize_method!(deserialize_i16);
    deserialize_method!(deserialize_i32);
    deserialize_method!(deserialize_i64);
    deserialize_method!(deserialize_i128);
    deserialize_method!(deserialize_u8);
    deserialize_method!(deserialize_u16);
    deserialize_method!(deserialize_u32);
    deserialize_method!(deserialize_u64);
    deserialize_method!(deserialize_u128);
    deserialize_method!(deserialize_f32);
    deserialize_method!(deserialize_f64);
    deserialize_method!(deserialize_char);
    deserialize_method!(deserialize_str);
    deserialize_method!(deserialize_string);
    deserialize_method!(deserialize_bytes);
    deserialize_method!(deserialize_byte_buf);
    deserialize_method!(deserialize_option);
    deserialize_method!(deserialize_unit);
    deserialize_method!(deserialize_unit_struct, name: &'static str);
    deserialize_method!(deserialize_seq);
    deserialize_method!(deserialize_tuple, len: usize);
    deserialize_method!(deserialize_tuple_struct, name: &'static str, len: usize);
    deserialize_method!(deserialize_identifier);
    fn deserialize_newtype_struct<V: Visitor<'de>>(self, _name: &'static str, visitor: V) -> Result<V::Value, Self::Error> { visitor.visit_newtype_struct(self) }
    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> { self.deserialize_any(visitor) }
    fn deserialize_struct<V: Visitor<'de>>(self, _name: &'static str, _fields: &'static [&'static str], visitor: V) -> Result<V::Value, Self::Error> { self.deserialize_any(visitor) }
    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> { self.deserialize_any(visitor) }
    fn deserialize_enum<V: Visitor<'de>>(self, _name: &'static str, _variants: &'static [&'static str], visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_any(EnumVisitor { inner: visitor })
    }
    fn is_human_readable(&self) -> bool { false }
}
struct EnumVisitor<V> { inner: V }
impl<'de, V: Visitor<'de>> Visitor<'de> for EnumVisitor<V> {
    type Value = V::Value;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { self.inner.expecting(f) }
    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> { self.inner.visit_enum(value.into_deserializer()) }
    fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> { self.inner.visit_enum(value.into_deserializer()) }
    fn visit_map<A: de::MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> { self.inner.visit_enum(de::value::MapAccessDeserializer::new(map)) }
}
struct HostVisitor<V> { inner: V, depth: usize }
macro_rules! visit_integer {
    ($method:ident, $ty:ty) => {
        fn $method<E: de::Error>(self, value: $ty) -> Result<Self::Value, E> {
            if !(-super::MAX_SAFE_INTEGER..=super::MAX_SAFE_INTEGER).contains(&(value as i128)) { return Err(E::custom("integer exceeds JavaScript safe range")); }
            self.inner.$method(value)
        }
    };
}
impl<'de, V: Visitor<'de>> Visitor<'de> for HostVisitor<V> {
    type Value = V::Value;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { self.inner.expecting(f) }
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> { self.inner.visit_unit() }
    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> { self.inner.visit_none() }
    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> { self.inner.visit_bool(value) }
    visit_integer!(visit_i8, i8);
    visit_integer!(visit_i16, i16);
    visit_integer!(visit_i32, i32);
    visit_integer!(visit_i64, i64);
    visit_integer!(visit_i128, i128);
    visit_integer!(visit_u8, u8);
    visit_integer!(visit_u16, u16);
    visit_integer!(visit_u32, u32);
    visit_integer!(visit_u64, u64);
    fn visit_u128<E: de::Error>(self, value: u128) -> Result<Self::Value, E> {
        if value > super::MAX_SAFE_INTEGER as u128 { return Err(E::custom("integer exceeds JavaScript safe range")); }
        self.inner.visit_u128(value)
    }
    fn visit_f32<E: de::Error>(self, value: f32) -> Result<Self::Value, E> {
        if !value.is_finite() || (value == 0.0 && value.is_sign_negative()) { return Err(E::custom("invalid Value float")); }
        self.inner.visit_f32(value)
    }
    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
        if !value.is_finite() || (value == 0.0 && value.is_sign_negative()) { return Err(E::custom("invalid Value float")); }
        self.inner.visit_f64(value)
    }
    fn visit_char<E: de::Error>(self, value: char) -> Result<Self::Value, E> { self.inner.visit_char(value) }
    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> { self.inner.visit_str(value) }
    fn visit_borrowed_str<E: de::Error>(self, value: &'de str) -> Result<Self::Value, E> { self.inner.visit_borrowed_str(value) }
    fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> { self.inner.visit_string(value) }
    fn visit_bytes<E: de::Error>(self, _value: &[u8]) -> Result<Self::Value, E> { Err(E::custom("raw byte strings are not a JSON Value; use a raw CID reference")) }
    fn visit_borrowed_bytes<E: de::Error>(self, value: &'de [u8]) -> Result<Self::Value, E> { self.visit_bytes(value) }
    fn visit_byte_buf<E: de::Error>(self, value: Vec<u8>) -> Result<Self::Value, E> { self.visit_bytes(&value) }
    fn visit_some<D: de::Deserializer<'de>>(self, inner: D) -> Result<Self::Value, D::Error> { self.inner.visit_some(HostDeserializer { inner, depth: self.depth }) }
    fn visit_seq<A: de::SeqAccess<'de>>(self, inner: A) -> Result<Self::Value, A::Error> { self.inner.visit_seq(HostSequence { inner, depth: self.depth + 1 }) }
    fn visit_map<A: de::MapAccess<'de>>(self, inner: A) -> Result<Self::Value, A::Error> { self.inner.visit_map(HostMap { inner, depth: self.depth + 1 }) }
    fn visit_newtype_struct<D: de::Deserializer<'de>>(self, decoder: D) -> Result<Self::Value, D::Error> {
        struct CidVisitor;
        impl<'de> Visitor<'de> for CidVisitor {
            type Value = Cid;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { f.write_str("CID bytes") }
            fn visit_bytes<E: de::Error>(self, bytes: &[u8]) -> Result<Cid, E> { Cid::try_from(bytes).map_err(E::custom) }
        }
        let cid = decoder.deserialize_bytes(CidVisitor)?;
        self.inner.visit_map(ReferenceMap { reference: Some(cid.to_string()), key: true }).map_err(de::Error::custom)
    }
}
struct HostSeed<S> { inner: S, depth: usize }
impl<'de, S: DeserializeSeed<'de>> DeserializeSeed<'de> for HostSeed<S> {
    type Value = S::Value;
    fn deserialize<D: de::Deserializer<'de>>(self, inner: D) -> Result<Self::Value, D::Error> { self.inner.deserialize(HostDeserializer { inner, depth: self.depth }) }
}
struct HostSequence<A> { inner: A, depth: usize }
impl<'de, A: de::SeqAccess<'de>> de::SeqAccess<'de> for HostSequence<A> {
    type Error = A::Error;
    fn next_element_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<Option<S::Value>, A::Error> { self.inner.next_element_seed(HostSeed { inner: seed, depth: self.depth }) }
    fn size_hint(&self) -> Option<usize> { self.inner.size_hint() }
}
struct HostMap<A> { inner: A, depth: usize }
impl<'de, A: de::MapAccess<'de>> de::MapAccess<'de> for HostMap<A> {
    type Error = A::Error;
    fn next_key_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<Option<S::Value>, A::Error> { self.inner.next_key_seed(seed) }
    fn next_value_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<S::Value, A::Error> { self.inner.next_value_seed(HostSeed { inner: seed, depth: self.depth }) }
    fn size_hint(&self) -> Option<usize> { self.inner.size_hint() }
}
struct ReferenceMap { reference: Option<String>, key: bool }
impl<'de> de::MapAccess<'de> for ReferenceMap {
    type Error = serde::de::value::Error;
    fn next_key_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<Option<S::Value>, Self::Error> {
        if !self.key { return Ok(None); }
        self.key = false;
        seed.deserialize("$ref".into_deserializer()).map(Some)
    }
    fn next_value_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<S::Value, Self::Error> {
        let reference = self.reference.take().ok_or_else(|| de::Error::custom("CID value already consumed"))?;
        seed.deserialize(reference.into_deserializer())
    }
    fn size_hint(&self) -> Option<usize> { Some(usize::from(self.key)) }
}
