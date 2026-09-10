pub const TRACE_MAX_METADATA_BYTES: usize = 16 * 1024 * 1024;
pub const TRACE_MAX_SCOPE_BYTES: usize = 4096;
pub const TRACE_MAX_ERROR_BYTES: usize = 64 * 1024;
pub const TRACE_MAX_ENTRIES: usize = 100_000;
pub const TRACE_MAX_BLOB_BYTES: usize = 64 * 1024 * 1024;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TraceKey {
    pub scope: String,
    pub occurrence: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TraceOutcome {
    Success { result_hash: String },
    Error { message: String },
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceEntry {
    pub key: TraceKey,
    pub descriptor_hash: String,
    pub outcome: TraceOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallTrace {
    pub version: u32,
    pub definition_hash: Option<String>,
    pub args_hash: Option<String>,
    pub scope: String,
    pub entries: Vec<TraceEntry>,
    /// None is a recoverable actor checkpoint; Some is a completed call.
    pub outcome: Option<TraceOutcome>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TraceBlobKind {
    #[serde(rename = "desc")]
    Descriptor,
    #[serde(rename = "args")]
    Arguments,
    #[serde(rename = "result")]
    Result,
}
impl TraceBlobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Descriptor => "desc",
            Self::Arguments => "args",
            Self::Result => "result",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceBlob {
    pub hash: String,
    pub kind: TraceBlobKind,
    /// Canonical DAG-CBOR bytes, validated against hash at the storage boundary.
    pub bytes: Vec<u8>,
}

/// Explicit shared-cache admission from the trusted effect policy owner.
/// Observational effects belong only in the call trace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceMemo {
    pub descriptor_hash: String,
    pub scope: String,
    pub occurrence: i64,
    pub result_hash: String,
}

/// Deduplicated operation metadata attributed to its executing definition.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TraceObservation {
    pub definition_hash: String,
    pub op: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceBundle {
    pub trace: CallTrace,
    pub blobs: Vec<TraceBlob>,
    #[serde(default)]
    pub memos: Vec<TraceMemo>,
    #[serde(default)]
    pub observations: Vec<TraceObservation>,
}

/// Compact persisted trace boundary: positional named records, binary digests,
/// and a parent-indexed scope table. The public trace remains readable JSON.
pub fn encode_call_trace(trace: &CallTrace) -> Result<Vec<u8>, String> {
    validate_call_trace_limits(trace)?;
    let wire = WireTrace::from_trace(trace)?;
    let bytes = serde_ipld_dagcbor::to_vec(&wire).map_err(|error| error.to_string())?;
    if bytes.len() > TRACE_MAX_METADATA_BYTES {
        return Err("trace encoded byte limit exceeded".into());
    }
    Ok(bytes)
}
pub fn decode_call_trace(bytes: &[u8]) -> Result<CallTrace, String> {
    if bytes.len() > TRACE_MAX_METADATA_BYTES {
        return Err("trace encoded byte limit exceeded".into());
    }
    let wire: WireTrace =
        serde_ipld_dagcbor::from_slice(bytes).map_err(|error| error.to_string())?;
    let trace = wire.into_trace()?;
    if encode_call_trace(&trace)? != bytes {
        return Err("noncanonical call trace".into());
    }
    Ok(trace)
}

#[derive(Clone, Debug)]
struct Digest {
    bytes: [u8; 32],
}
impl Digest {
    fn parse(hash: &str) -> Result<Self, String> {
        let bytes = hex::decode(hash).map_err(|error| error.to_string())?;
        Ok(Self {
            bytes: bytes
                .try_into()
                .map_err(|_| "trace digest must contain 32 bytes")?,
        })
    }
    fn hash(&self) -> String {
        hex::encode(self.bytes)
    }
}
impl Serialize for Digest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.bytes)
    }
}
impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct DigestVisitor;
        impl serde::de::Visitor<'_> for DigestVisitor {
            type Value = Digest;
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("32-byte trace digest")
            }
            fn visit_bytes<E: serde::de::Error>(self, bytes: &[u8]) -> Result<Digest, E> {
                Ok(Digest {
                    bytes: bytes
                        .try_into()
                        .map_err(|_| E::custom("trace digest must contain 32 bytes"))?,
                })
            }
            fn visit_byte_buf<E: serde::de::Error>(self, bytes: Vec<u8>) -> Result<Digest, E> {
                self.visit_bytes(&bytes)
            }
        }
        deserializer.deserialize_bytes(DigestVisitor)
    }
}
struct WireOutcome {
    status: u8,
    result: Option<Digest>,
    error: Option<String>,
}
impl WireOutcome {
    fn from_outcome(outcome: &TraceOutcome) -> Result<Self, String> {
        Ok(match outcome {
            TraceOutcome::Success { result_hash } => Self {
                status: 0,
                result: Some(Digest::parse(result_hash)?),
                error: None,
            },
            TraceOutcome::Error { message } => Self {
                status: 1,
                result: None,
                error: Some(message.clone()),
            },
            TraceOutcome::Cancelled => Self {
                status: 2,
                result: None,
                error: None,
            },
        })
    }
    fn into_outcome(self) -> Result<TraceOutcome, String> {
        match self.status {
            0 if self.error.is_none() => Ok(TraceOutcome::Success {
                result_hash: self.result.ok_or("success trace lacks result")?.hash(),
            }),
            1 if self.result.is_none() => Ok(TraceOutcome::Error {
                message: self.error.ok_or("error trace lacks message")?,
            }),
            2 if self.result.is_none() && self.error.is_none() => Ok(TraceOutcome::Cancelled),
            _ => Err("invalid trace outcome".into()),
        }
    }
}
impl Serialize for WireOutcome {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut sequence = serializer.serialize_seq(Some(3))?;
        sequence.serialize_element(&self.status)?;
        sequence.serialize_element(&self.result)?;
        sequence.serialize_element(&self.error)?;
        sequence.end()
    }
}
struct WireScope {
    parent: usize,
    segment: String,
}
impl Serialize for WireScope {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut sequence = serializer.serialize_seq(Some(2))?;
        sequence.serialize_element(&self.parent)?;
        sequence.serialize_element(&self.segment)?;
        sequence.end()
    }
}
struct WireEntry {
    scope: usize,
    occurrence: i64,
    descriptor: Digest,
    outcome: WireOutcome,
}
impl Serialize for WireEntry {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut sequence = serializer.serialize_seq(Some(4))?;
        sequence.serialize_element(&self.scope)?;
        sequence.serialize_element(&self.occurrence)?;
        sequence.serialize_element(&self.descriptor)?;
        sequence.serialize_element(&self.outcome)?;
        sequence.end()
    }
}
struct WireTrace {
    version: u32,
    definition: Option<Digest>,
    args: Option<Digest>,
    scope: String,
    scopes: Vec<WireScope>,
    entries: Vec<WireEntry>,
    outcome: Option<WireOutcome>,
}
impl Serialize for WireTrace {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut sequence = serializer.serialize_seq(Some(7))?;
        sequence.serialize_element(&self.version)?;
        sequence.serialize_element(&self.definition)?;
        sequence.serialize_element(&self.args)?;
        sequence.serialize_element(&self.scope)?;
        sequence.serialize_element(&self.scopes)?;
        sequence.serialize_element(&self.entries)?;
        sequence.serialize_element(&self.outcome)?;
        sequence.end()
    }
}
impl WireTrace {
    fn from_trace(trace: &CallTrace) -> Result<Self, String> {
        if trace.version != 1 || trace.scope.is_empty() {
            return Err("invalid call trace header".into());
        }
        let mut scope_ids = std::collections::BTreeMap::from_iter([(trace.scope.clone(), 0_usize)]);
        let mut scopes = Vec::new();
        let mut scope_metadata = trace.scope.len();
        let mut entries = Vec::new();
        let mut ordered = trace.entries.iter().collect::<Vec<_>>();
        ordered.sort_by(|left, right| left.key.cmp(&right.key));
        let mut previous = None;
        for entry in ordered {
            if previous == Some(&entry.key) {
                return Err("duplicate trace occurrence".into());
            }
            previous = Some(&entry.key);
            if !(0..=9_007_199_254_740_991).contains(&entry.key.occurrence) {
                return Err("trace occurrence out of range".into());
            }
            let scope = if let Some(id) = scope_ids.get(&entry.key.scope) {
                *id
            } else {
                let relative = entry
                    .key
                    .scope
                    .strip_prefix(&format!("{}/", trace.scope))
                    .ok_or("trace entry outside call scope")?;
                let mut parent = 0;
                let mut path = trace.scope.clone();
                for segment in relative.split('/') {
                    if segment.is_empty() {
                        return Err("empty trace scope segment".into());
                    }
                    path.push('/');
                    path.push_str(segment);
                    parent = if let Some(id) = scope_ids.get(&path) {
                        *id
                    } else {
                        scope_metadata = scope_metadata.saturating_add(path.len());
                        if scope_metadata > TRACE_MAX_METADATA_BYTES {
                            return Err("trace scope metadata limit exceeded".into());
                        }
                        scopes.push(WireScope {
                            parent,
                            segment: segment.into(),
                        });
                        let id = scopes.len();
                        scope_ids.insert(path.clone(), id);
                        id
                    };
                }
                parent
            };
            entries.push(WireEntry {
                scope,
                occurrence: entry.key.occurrence,
                descriptor: Digest::parse(&entry.descriptor_hash)?,
                outcome: WireOutcome::from_outcome(&entry.outcome)?,
            });
        }
        Ok(Self {
            version: 1,
            definition: trace
                .definition_hash
                .as_deref()
                .map(Digest::parse)
                .transpose()?,
            args: trace.args_hash.as_deref().map(Digest::parse).transpose()?,
            scope: trace.scope.clone(),
            scopes,
            entries,
            outcome: trace
                .outcome
                .as_ref()
                .map(WireOutcome::from_outcome)
                .transpose()?,
        })
    }
    fn into_trace(self) -> Result<CallTrace, String> {
        if self.entries.len() > TRACE_MAX_ENTRIES || self.scope.len() > TRACE_MAX_SCOPE_BYTES {
            return Err("trace entry or scope limit exceeded".into());
        }
        let mut metadata_bytes = self.scope.len();
        let mut scopes = vec![self.scope.clone()];
        for scope in self.scopes {
            if scope.segment.is_empty() || scope.segment.contains('/') {
                return Err("invalid trace scope segment".into());
            }
            let parent = scopes
                .get(scope.parent)
                .ok_or("invalid trace scope parent")?;
            let size = parent
                .len()
                .saturating_add(1)
                .saturating_add(scope.segment.len());
            metadata_bytes = metadata_bytes.saturating_add(size);
            if size > TRACE_MAX_SCOPE_BYTES || metadata_bytes > TRACE_MAX_METADATA_BYTES {
                return Err("trace scope metadata limit exceeded".into());
            }
            scopes.push(format!("{parent}/{}", scope.segment));
        }
        let entries = self
            .entries
            .into_iter()
            .map(|entry| {
                metadata_bytes = metadata_bytes
                    .saturating_add(scopes.get(entry.scope).ok_or("missing trace scope")?.len());
                if metadata_bytes > TRACE_MAX_METADATA_BYTES {
                    return Err("trace expanded metadata limit exceeded".into());
                }
                Ok(TraceEntry {
                    key: TraceKey {
                        scope: scopes
                            .get(entry.scope)
                            .ok_or("missing trace scope")?
                            .clone(),
                        occurrence: entry.occurrence,
                    },
                    descriptor_hash: entry.descriptor.hash(),
                    outcome: entry.outcome.into_outcome()?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(CallTrace {
            version: self.version,
            definition_hash: self.definition.map(|digest| digest.hash()),
            args_hash: self.args.map(|digest| digest.hash()),
            scope: self.scope,
            entries,
            outcome: self.outcome.map(WireOutcome::into_outcome).transpose()?,
        })
    }
}

macro_rules! deserialize_sequence {
    ($name:ident { $($field:ident),+ $(,)? }) => {
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct RecordVisitor;
                impl<'de> serde::de::Visitor<'de> for RecordVisitor {
                    type Value = $name;
                    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result { formatter.write_str(stringify!($name)) }
                    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
                        $(let $field = sequence.next_element()?.ok_or_else(|| <A::Error as serde::de::Error>::custom(concat!("missing ", stringify!($field))))?;)+
                        Ok($name { $($field),+ })
                    }
                }
                deserializer.deserialize_tuple([$(stringify!($field)),+].len(), RecordVisitor)
            }
        }
    };
}
deserialize_sequence!(WireOutcome {
    status,
    result,
    error
});
deserialize_sequence!(WireScope { parent, segment });
deserialize_sequence!(WireEntry {
    scope,
    occurrence,
    descriptor,
    outcome
});
deserialize_sequence!(WireTrace {
    version,
    definition,
    args,
    scope,
    scopes,
    entries,
    outcome
});

pub fn validate_call_trace_limits(trace: &CallTrace) -> Result<(), String> {
    if trace.entries.len() > TRACE_MAX_ENTRIES || trace.scope.len() > TRACE_MAX_SCOPE_BYTES {
        return Err("trace entry or scope limit exceeded".into());
    }
    let mut metadata = trace.scope.len();
    for entry in &trace.entries {
        if entry.key.scope.len() > TRACE_MAX_SCOPE_BYTES {
            return Err("trace scope limit exceeded".into());
        }
        metadata = metadata
            .saturating_add(entry.key.scope.len())
            .saturating_add(entry.descriptor_hash.len());
        metadata = metadata.saturating_add(outcome_metadata(&entry.outcome)?);
    }
    if let Some(outcome) = &trace.outcome {
        metadata = metadata.saturating_add(outcome_metadata(outcome)?);
    }
    if metadata > TRACE_MAX_METADATA_BYTES {
        return Err("trace metadata limit exceeded".into());
    }
    Ok(())
}
fn outcome_metadata(outcome: &TraceOutcome) -> Result<usize, String> {
    match outcome {
        TraceOutcome::Success { result_hash } => Ok(result_hash.len()),
        TraceOutcome::Error { message } => {
            if message.len() > TRACE_MAX_ERROR_BYTES {
                return Err("trace error limit exceeded".into());
            }
            Ok(message.len())
        }
        TraceOutcome::Cancelled => Ok(0),
    }
}
