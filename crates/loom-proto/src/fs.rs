//! Typed filesystem results. Named Rust fields use a compact DAG-CBOR sequence.
use serde::{
    Deserialize, Serialize,
    de::{SeqAccess, Visitor},
    ser::SerializeTuple,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    #[serde(rename = "file")]
    File,
    #[serde(rename = "dir")]
    Directory,
    #[serde(rename = "symlink")]
    Symlink,
    #[serde(rename = "other")]
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// A basename for list/stat; a relative path from the requested root for walk.
    pub name: String,
    /// Logical file bytes; zero for directories, symlinks and other entries.
    pub size: u64,
    pub kind: EntryKind,
}
impl Serialize for DirEntry {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.size > super::codec::MAX_SAFE_INTEGER as u64 {
            return Err(serde::ser::Error::custom(
                "integer exceeds JavaScript safe range",
            ));
        }
        let mut entry = serializer.serialize_tuple(3)?;
        entry.serialize_element(&self.name)?;
        entry.serialize_element(&self.size)?;
        entry.serialize_element(&self.kind)?;
        entry.end()
    }
}
impl<'de> Deserialize<'de> for DirEntry {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct EntryVisitor;
        impl<'de> Visitor<'de> for EntryVisitor {
            type Value = DirEntry;
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("[name, size, kind]")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut fields: A) -> Result<Self::Value, A::Error> {
                let name = fields
                    .next_element()?
                    .ok_or_else(|| serde::de::Error::invalid_length(0, &self))?;
                let size = fields
                    .next_element()?
                    .ok_or_else(|| serde::de::Error::invalid_length(1, &self))?;
                let kind = fields
                    .next_element()?
                    .ok_or_else(|| serde::de::Error::invalid_length(2, &self))?;
                if fields.next_element::<serde::de::IgnoredAny>()?.is_some() {
                    return Err(serde::de::Error::invalid_length(4, &self));
                }
                if size > crate::codec::MAX_SAFE_INTEGER as u64 {
                    return Err(serde::de::Error::custom(
                        "integer exceeds JavaScript safe range",
                    ));
                }
                Ok(DirEntry { name, size, kind })
            }
        }
        deserializer.deserialize_tuple(3, EntryVisitor)
    }
}
impl crate::codec::host::sealed::HostValue for DirEntry {}
impl crate::HostValue for DirEntry {}
