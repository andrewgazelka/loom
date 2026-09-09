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
const MAX_SAFE_INTEGER: i128 = 9_007_199_254_740_991;

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
    // Preserve Serde unit/null and negative zero before applying the Value contract.
    // ipld-core::to_ipld rejects unit, and the DAG serializer normalizes -0.0.
    let value = serde_value::to_value(value).map_err(|error| error.to_string())?;
    let value = serde_to_ipld(value, 0)?;
    let value = to_wire(value)?;
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
    let value = from_wire(value)?;
    ipld_core::serde::from_ipld(value).map_err(|error| error.to_string())
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

fn to_wire(value: Ipld) -> Result<Ipld, String> {
    validate_number(&value)?;
    match value {
        Ipld::Float(number) if number.fract() == 0.0 && number.abs() <= MAX_SAFE_INTEGER as f64 => {
            Ok(Ipld::Integer(number as i128))
        }
        Ipld::Map(mut fields) => {
            if let Some(reference) = fields.remove("$ref") {
                if !fields.is_empty() {
                    return Err("reserved $ref object cannot contain other fields".into());
                }
                let Ipld::String(reference) = reference else {
                    return Err("$ref must be a CID string".into());
                };
                let cid: Cid = reference
                    .parse()
                    .map_err(|error: ipld_core::cid::Error| error.to_string())?;
                return Ok(Ipld::Link(cid));
            }
            let mut mapped = BTreeMap::new();
            for entry in fields {
                mapped.insert(entry.0, to_wire(entry.1)?);
            }
            Ok(Ipld::Map(mapped))
        }
        Ipld::List(values) => Ok(Ipld::List(
            values.into_iter().map(to_wire).collect::<Result<_, _>>()?,
        )),
        other => Ok(other),
    }
}

fn from_wire(value: Ipld) -> Result<Ipld, String> {
    validate_number(&value)?;
    match value {
        Ipld::Link(cid) => {
            let mut reference = BTreeMap::new();
            reference.insert("$ref".into(), Ipld::String(cid.to_string()));
            Ok(Ipld::Map(reference))
        }
        Ipld::Bytes(_) => {
            Err("raw byte strings are not a JSON Value; use a raw CID reference".into())
        }
        Ipld::Map(fields) => {
            if fields.contains_key("$ref") {
                return Err("wire references must use DAG-CBOR tag 42".into());
            }
            let mut mapped = BTreeMap::new();
            for entry in fields {
                mapped.insert(entry.0, from_wire(entry.1)?);
            }
            Ok(Ipld::Map(mapped))
        }
        Ipld::List(values) => Ok(Ipld::List(
            values
                .into_iter()
                .map(from_wire)
                .collect::<Result<_, _>>()?,
        )),
        other => Ok(other),
    }
}

fn serde_to_ipld(value: serde_value::Value, depth: usize) -> Result<Ipld, String> {
    if depth > 256 {
        return Err("Value exceeds maximum depth 256".into());
    }
    use serde_value::Value as S;
    Ok(match value {
        S::Bool(v) => Ipld::Bool(v),
        S::U8(v) => Ipld::Integer(v.into()),
        S::U16(v) => Ipld::Integer(v.into()),
        S::U32(v) => Ipld::Integer(v.into()),
        S::U64(v) => Ipld::Integer(v.into()),
        S::I8(v) => Ipld::Integer(v.into()),
        S::I16(v) => Ipld::Integer(v.into()),
        S::I32(v) => Ipld::Integer(v.into()),
        S::I64(v) => Ipld::Integer(v.into()),
        S::F32(v) => Ipld::Float(v.into()),
        S::F64(v) => Ipld::Float(v),
        S::Char(v) => Ipld::String(v.to_string()),
        S::String(v) => Ipld::String(v),
        S::Unit | S::Option(None) => Ipld::Null,
        S::Option(Some(v)) | S::Newtype(v) => return serde_to_ipld(*v, depth),
        S::Bytes(_) => {
            return Err("raw byte strings are not a JSON Value; use a raw CID reference".into());
        }
        S::Seq(v) => Ipld::List(
            v.into_iter()
                .map(|value| serde_to_ipld(value, depth + 1))
                .collect::<Result<_, _>>()?,
        ),
        S::Map(v) => {
            let mut map = BTreeMap::new();
            for entry in v {
                let S::String(key) = entry.0 else {
                    return Err("Value map keys must be strings".into());
                };
                map.insert(key, serde_to_ipld(entry.1, depth + 1)?);
            }
            Ipld::Map(map)
        }
    })
}

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
