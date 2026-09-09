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
    pub scope: String,
    pub entries: Vec<TraceEntry>,
    /// None is a recoverable actor checkpoint; Some is a completed call.
    pub outcome: Option<TraceOutcome>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TraceBlobKind {
    #[serde(rename = "desc")]
    Descriptor,
    #[serde(rename = "result")]
    Result,
}
impl TraceBlobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Descriptor => "desc",
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceBundle {
    pub trace: CallTrace,
    pub blobs: Vec<TraceBlob>,
    #[serde(default)]
    pub memos: Vec<TraceMemo>,
}
