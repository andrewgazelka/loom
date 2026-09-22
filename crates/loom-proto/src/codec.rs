pub(crate) mod host;
use crate::Value;
use ipld_core::{
    cid::{Cid, multihash::Multihash},
    ipld::Ipld,
};
use serde::{Serialize, de::DeserializeOwned};
use std::collections::BTreeMap;

pub const DAG_CBOR_CODEC: u64 = 0x71;
pub const RAW_CODEC: u64 = 0x55;
const BLAKE3_256: u64 = 0x1e;
pub(crate) const MAX_SAFE_INTEGER: i128 = 9_007_199_254_740_991;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentAddress {
    pub hash: String,
    pub codec: u64,
}

pub fn cid_for_hash(hash: &str, codec: u64) -> Result<String, String> {
    let digest = hex::decode(hash).map_err(|error| error.to_string())?;
    if digest.len() != 32 {
        return Err("BLAKE3-256 digest must contain 32 bytes".into());
    }
    let hash = Multihash::<64>::wrap(BLAKE3_256, &digest).map_err(|error| error.to_string())?;
    Ok(Cid::new_v1(codec, hash).to_string())
}

pub fn reference(hash: &str, codec: u64) -> Result<Value, String> {
    Ok(serde_json::json!({"$ref":cid_for_hash(hash,codec)?}))
}

pub fn parse_reference(cid: &str) -> Result<ContentAddress, String> {
    let cid: Cid = cid
        .parse()
        .map_err(|error: ipld_core::cid::Error| error.to_string())?;
    if cid.hash().code() != BLAKE3_256 || cid.hash().size() != 32 {
        return Err("local references require BLAKE3-256 multihashes".into());
    }
    Ok(ContentAddress {
        hash: hex::encode(cid.hash().digest()),
        codec: cid.codec(),
    })
}

pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    encode_with(value, Floats::Canonical)
}

/// Encode positional call arguments: the same `$ref` links and validation as
/// [`encode`], but a JSON `3.0` stays a CBOR float. A typed Rust callee decodes
/// `f64` parameters strictly, so the canonical integer collapse would make every
/// whole-valued float argument undecodable.
pub fn encode_arguments<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    encode_with(value, Floats::Preserved)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Floats {
    /// Whole-valued floats in the safe range become integers (content identity).
    Canonical,
    /// Floats stay floats (typed argument payloads).
    Preserved,
}

fn encode_with<T: Serialize>(value: &T, floats: Floats) -> Result<Vec<u8>, String> {
    // Build the canonical value tree once. Validation stays after collection so
    // Serde maps retain the established last-value-wins behavior before admission.
    let mut value = value
        .serialize(ValueSerializer)
        .map_err(|error| error.to_string())?;
    to_wire(&mut value, 0, floats)?;
    serde_ipld_dagcbor::to_vec(&value).map_err(|error| error.to_string())
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    let mut deserializer = serde_ipld_dagcbor::de::Deserializer::from_slice(bytes);
    let value =
        serde::de::DeserializeSeed::deserialize(BoundedIpld { depth: 0 }, &mut deserializer)
            .map_err(|error| error.to_string())?;
    deserializer.end().map_err(|error| error.to_string())?;
    // Validate canonical bytes before translating links or normalizing numbers.
    // In particular a valid f64 1.0 must stay Float here, rather than become int.
    let canonical = serde_ipld_dagcbor::to_vec(&value).map_err(|error| error.to_string())?;
    if canonical != bytes {
        return Err("noncanonical DAG-CBOR encoding".into());
    }
    let mut value = value;
    from_wire(&mut value)?;
    // IPLD dispatches every integer as i128. Serde's buffering visitor for
    // internally tagged/untagged enums rejects i128 before the destination's
    // u32/i64 visitor sees it. Our validated wire domain is JSON-safe already;
    // normalize that typed boundary once so all Serde shapes receive supported
    // integer widths, without changing canonical bytes or CID translation.
    let value: Value = ipld_core::serde::from_ipld(value).map_err(|error| error.to_string())?;
    serde_json::from_value(value).map_err(|error| error.to_string())
}

fn validate_number(value: &Ipld) -> Result<(), String> {
    match value {
        Ipld::Integer(number) if !(-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(number) => {
            Err("integer exceeds JavaScript safe range".into())
        }
        Ipld::Float(number) if !number.is_finite() => Err("nonfinite float is not a Value".into()),
        Ipld::Float(number) if *number == 0.0 && number.is_sign_negative() => {
            Err("negative zero is not a Value".into())
        }
        _ => Ok(()),
    }
}

fn to_wire(value: &mut Ipld, depth: usize, floats: Floats) -> Result<(), String> {
    if depth > 256 {
        return Err("Value exceeds maximum depth 256".into());
    }
    validate_number(value)?;
    match value {
        Ipld::Float(number)
            if floats == Floats::Canonical
                && number.fract() == 0.0
                && number.abs() <= MAX_SAFE_INTEGER as f64 =>
        {
            *value = Ipld::Integer(*number as i128);
        }
        Ipld::Bytes(_) => {
            return Err("raw byte strings are not a JSON Value; use a raw CID reference".into());
        }
        Ipld::Map(fields) => {
            if let Some(reference) = fields.get("$ref") {
                if fields.len() != 1 {
                    return Err("reserved $ref object cannot contain other fields".into());
                }
                let Ipld::String(reference) = reference else {
                    return Err("$ref must be a CID string".into());
                };
                // The reserved string is a child in the logical Value tree.
                if depth == 256 {
                    return Err("Value exceeds maximum depth 256".into());
                }
                let cid: Cid = reference
                    .parse()
                    .map_err(|error: ipld_core::cid::Error| error.to_string())?;
                *value = Ipld::Link(cid);
            } else {
                for child in fields.values_mut() {
                    to_wire(child, depth + 1, floats)?;
                }
            }
        }
        Ipld::List(values) => {
            for child in values {
                to_wire(child, depth + 1, floats)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn from_wire(value: &mut Ipld) -> Result<(), String> {
    validate_number(value)?;
    match value {
        Ipld::Link(cid) => {
            let mut reference = BTreeMap::new();
            reference.insert("$ref".into(), Ipld::String(cid.to_string()));
            *value = Ipld::Map(reference);
        }
        Ipld::Bytes(_) => {
            return Err("raw byte strings are not a JSON Value; use a raw CID reference".into());
        }
        Ipld::Map(fields) => {
            if fields.contains_key("$ref") {
                return Err("wire references must use DAG-CBOR tag 42".into());
            }
            for child in fields.values_mut() {
                from_wire(child)?;
            }
        }
        Ipld::List(values) => {
            for child in values {
                from_wire(child)?;
            }
        }
        _ => {}
    }
    Ok(())
}

// This adapter implements the existing serde-value model directly as IPLD. The
// maintained DAG-CBOR serializer still owns ordering and wire representation.
struct ValueSerializer;
type SerializeError = serde_value::SerializerError;
impl serde::Serializer for ValueSerializer {
    type Ok = Ipld;
    type Error = SerializeError;
    type SerializeSeq = ValueSequence;
    type SerializeTuple = ValueSequence;
    type SerializeTupleStruct = ValueSequence;
    type SerializeTupleVariant = ValueSequence;
    type SerializeMap = ValueMap;
    type SerializeStruct = ValueMap;
    type SerializeStructVariant = ValueMap;

    fn serialize_bool(self, value: bool) -> Result<Ipld, Self::Error> {
        Ok(Ipld::Bool(value))
    }
    fn serialize_i8(self, value: i8) -> Result<Ipld, Self::Error> {
        self.serialize_i64(value.into())
    }
    fn serialize_i16(self, value: i16) -> Result<Ipld, Self::Error> {
        self.serialize_i64(value.into())
    }
    fn serialize_i32(self, value: i32) -> Result<Ipld, Self::Error> {
        self.serialize_i64(value.into())
    }
    fn serialize_i64(self, value: i64) -> Result<Ipld, Self::Error> {
        Ok(Ipld::Integer(value.into()))
    }
    fn serialize_u8(self, value: u8) -> Result<Ipld, Self::Error> {
        self.serialize_u64(value.into())
    }
    fn serialize_u16(self, value: u16) -> Result<Ipld, Self::Error> {
        self.serialize_u64(value.into())
    }
    fn serialize_u32(self, value: u32) -> Result<Ipld, Self::Error> {
        self.serialize_u64(value.into())
    }
    fn serialize_u64(self, value: u64) -> Result<Ipld, Self::Error> {
        Ok(Ipld::Integer(value.into()))
    }
    fn serialize_f32(self, value: f32) -> Result<Ipld, Self::Error> {
        self.serialize_f64(value.into())
    }
    fn serialize_f64(self, value: f64) -> Result<Ipld, Self::Error> {
        Ok(Ipld::Float(value))
    }
    fn serialize_char(self, value: char) -> Result<Ipld, Self::Error> {
        Ok(Ipld::String(value.to_string()))
    }
    fn serialize_str(self, value: &str) -> Result<Ipld, Self::Error> {
        Ok(Ipld::String(value.into()))
    }
    fn serialize_bytes(self, value: &[u8]) -> Result<Ipld, Self::Error> {
        Ok(Ipld::Bytes(value.into()))
    }
    fn serialize_none(self) -> Result<Ipld, Self::Error> {
        Ok(Ipld::Null)
    }
    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<Ipld, Self::Error> {
        value.serialize(self)
    }
    fn serialize_unit(self) -> Result<Ipld, Self::Error> {
        Ok(Ipld::Null)
    }
    fn serialize_unit_struct(self, _name: &'static str) -> Result<Ipld, Self::Error> {
        Ok(Ipld::Null)
    }
    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> Result<Ipld, Self::Error> {
        self.serialize_str(variant)
    }
    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<Ipld, Self::Error> {
        value.serialize(self)
    }
    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Ipld, Self::Error> {
        let mut map = BTreeMap::new();
        map.insert(variant.into(), value.serialize(self)?);
        Ok(Ipld::Map(map))
    }
    fn serialize_seq(self, len: Option<usize>) -> Result<ValueSequence, Self::Error> {
        Ok(ValueSequence {
            values: Vec::with_capacity(len.unwrap_or(0).min(4096)),
            variant: None,
        })
    }
    fn serialize_tuple(self, len: usize) -> Result<ValueSequence, Self::Error> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<ValueSequence, Self::Error> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<ValueSequence, Self::Error> {
        Ok(ValueSequence {
            variant: Some(variant),
            ..self.serialize_seq(Some(len))?
        })
    }
    fn serialize_map(self, _len: Option<usize>) -> Result<ValueMap, Self::Error> {
        Ok(ValueMap {
            fields: BTreeMap::new(),
            key: None,
            variant: None,
        })
    }
    fn serialize_struct(self, _name: &'static str, len: usize) -> Result<ValueMap, Self::Error> {
        self.serialize_map(Some(len))
    }
    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<ValueMap, Self::Error> {
        Ok(ValueMap {
            variant: Some(variant),
            ..self.serialize_map(Some(len))?
        })
    }
}
struct ValueSequence {
    values: Vec<Ipld>,
    variant: Option<&'static str>,
}
impl ValueSequence {
    fn push<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), SerializeError> {
        self.values.push(value.serialize(ValueSerializer)?);
        Ok(())
    }
    fn finish(self) -> Ipld {
        wrap_variant(self.variant, Ipld::List(self.values))
    }
}
macro_rules! sequence_serializer {
    ($trait:ident, $element:ident) => {
        impl serde::ser::$trait for ValueSequence {
            type Ok = Ipld;
            type Error = SerializeError;
            fn $element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Self::Error> {
                self.push(value)
            }
            fn end(self) -> Result<Ipld, Self::Error> {
                Ok(self.finish())
            }
        }
    };
}
sequence_serializer!(SerializeSeq, serialize_element);
sequence_serializer!(SerializeTuple, serialize_element);
sequence_serializer!(SerializeTupleStruct, serialize_field);
sequence_serializer!(SerializeTupleVariant, serialize_field);

struct ValueMap {
    fields: BTreeMap<String, Ipld>,
    key: Option<String>,
    variant: Option<&'static str>,
}
fn wrap_variant(variant: Option<&'static str>, value: Ipld) -> Ipld {
    match variant {
        Some(variant) => {
            let mut map = BTreeMap::new();
            map.insert(variant.into(), value);
            Ipld::Map(map)
        }
        None => value,
    }
}
impl serde::ser::SerializeMap for ValueMap {
    type Ok = Ipld;
    type Error = SerializeError;
    fn serialize_key<T: ?Sized + Serialize>(&mut self, key: &T) -> Result<(), Self::Error> {
        // Retain the old key admission precisely: chars and newtypes wrapping
        // strings were rejected even though ordinary string keys were accepted.
        let serde_value::Value::String(key) = serde_value::to_value(key)? else {
            return Err(serde::ser::Error::custom("Value map keys must be strings"));
        };
        self.key = Some(key);
        Ok(())
    }
    fn serialize_value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Self::Error> {
        let key = self.key.take().ok_or_else(|| {
            <SerializeError as serde::ser::Error>::custom(
                "serialize_value called before serialize_key",
            )
        })?;
        self.fields.insert(key, value.serialize(ValueSerializer)?);
        Ok(())
    }
    fn end(self) -> Result<Ipld, Self::Error> {
        Ok(wrap_variant(self.variant, Ipld::Map(self.fields)))
    }
}
macro_rules! struct_serializer {
    ($trait:ident) => {
        impl serde::ser::$trait for ValueMap {
            type Ok = Ipld;
            type Error = SerializeError;
            fn serialize_field<T: ?Sized + Serialize>(
                &mut self,
                key: &'static str,
                value: &T,
            ) -> Result<(), Self::Error> {
                self.fields
                    .insert(key.into(), value.serialize(ValueSerializer)?);
                Ok(())
            }
            fn end(self) -> Result<Ipld, Self::Error> {
                Ok(wrap_variant(self.variant, Ipld::Map(self.fields)))
            }
        }
    };
}
struct_serializer!(SerializeStruct);
struct_serializer!(SerializeStructVariant);

// A Serde visitor limits nesting before allocation/recursion. The maintained
// DAG-CBOR deserializer remains responsible for every wire-format rule.
struct BoundedIpld {
    depth: usize,
}
impl<'de> serde::de::DeserializeSeed<'de> for BoundedIpld {
    type Value = Ipld;
    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Ipld, D::Error> {
        if self.depth > 256 {
            return Err(serde::de::Error::custom("Value exceeds maximum depth 256"));
        }
        deserializer.deserialize_any(self)
    }
}
impl<'de> serde::de::Visitor<'de> for BoundedIpld {
    type Value = Ipld;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a DAG-CBOR Value")
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<Ipld, E> {
        Ok(Ipld::Null)
    }
    fn visit_none<E: serde::de::Error>(self) -> Result<Ipld, E> {
        Ok(Ipld::Null)
    }
    fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Ipld, E> {
        Ok(Ipld::Bool(v))
    }
    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Ipld, E> {
        Ok(Ipld::Integer(v.into()))
    }
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Ipld, E> {
        Ok(Ipld::Integer(v.into()))
    }
    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Ipld, E> {
        Ok(Ipld::Float(v))
    }
    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Ipld, E> {
        Ok(Ipld::String(v.into()))
    }
    fn visit_bytes<E: serde::de::Error>(self, _v: &[u8]) -> Result<Ipld, E> {
        Err(E::custom(
            "raw byte strings are not a JSON Value; use a raw CID reference",
        ))
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut access: A) -> Result<Ipld, A::Error> {
        let mut list = Vec::new();
        while let Some(value) = access.next_element_seed(BoundedIpld {
            depth: self.depth + 1,
        })? {
            list.push(value);
        }
        Ok(Ipld::List(list))
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut access: A) -> Result<Ipld, A::Error> {
        let mut map = BTreeMap::new();
        while let Some(key) = access.next_key::<String>()? {
            let value = access.next_value_seed(BoundedIpld {
                depth: self.depth + 1,
            })?;
            if map.insert(key, value).is_some() {
                return Err(serde::de::Error::custom("duplicate map key"));
            }
        }
        Ok(Ipld::Map(map))
    }
    fn visit_newtype_struct<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Ipld, D::Error> {
        let Ipld::Bytes(bytes) = serde::Deserialize::deserialize(deserializer)? else {
            return Err(serde::de::Error::custom("invalid CID bytes"));
        };
        let cid = Cid::try_from(bytes.as_slice()).map_err(serde::de::Error::custom)?;
        Ok(Ipld::Link(cid))
    }
}
