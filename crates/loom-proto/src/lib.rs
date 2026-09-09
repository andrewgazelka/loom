mod cas;
pub use cas::*;
use serde::{Deserialize, Serialize};
pub use serde_json::Value;
use std::collections::BTreeMap;
#[cfg(feature = "codegen")]
use ts_rs::TS;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "codegen", derive(TS))]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    #[default]
    Ts,
    Rust,
}
impl Lang {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ts => "ts",
            Self::Rust => "rust",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "codegen", derive(TS))]
#[serde(deny_unknown_fields)]
pub struct TypeSig {
    pub exports: Vec<ExportSig>,
    #[serde(default)]
    pub effects: EffectSet,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "codegen", derive(TS))]
pub struct EffectSet {
    pub labels: Vec<String>,
    pub unknown: bool,
}
impl Default for EffectSet {
    fn default() -> Self {
        Self {
            labels: Vec::new(),
            unknown: true,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "codegen", derive(TS))]
#[serde(deny_unknown_fields)]
pub struct ExportSig {
    pub name: String,
    pub params: Vec<ParamSig>,
    pub returns: ValueShape,
    #[serde(default)]
    pub effects: EffectSet,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "codegen", derive(TS))]
#[serde(deny_unknown_fields)]
pub struct ParamSig {
    pub name: String,
    pub shape: ValueShape,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "codegen", derive(TS))]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ValueShape {
    Null,
    Boolean,
    Number,
    String,
    Array {
        items: Box<ValueShape>,
    },
    Object {
        properties: BTreeMap<String, ValueShape>,
        optional: Vec<String>,
    },
    Ref {
        target: Box<ValueShape>,
    },
    #[default]
    Value,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
pub struct Def {
    pub hash: String,
    pub lang: Lang,
    pub component_hash: Option<String>,
    pub sig: TypeSig,
    #[serde(default)]
    pub allowed_effects: Option<Vec<String>>,
    #[serde(default)]
    pub observed_effects: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
pub struct Diagnostic {
    pub lang: Lang,
    pub file: String,
    pub line: usize,
    pub col: usize,
    pub code: String,
    pub message: String,
    pub snippet: Option<String>,
    pub hint: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
pub struct DefineRequest {
    #[serde(default)]
    pub lang: Lang,
    pub name: String,
    pub source: String,
    #[serde(default)]
    pub deps: BTreeMap<String, String>,
    #[serde(default)]
    pub allowed_effects: Option<Vec<String>>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
pub struct EvalRequest {
    #[serde(default)]
    pub session: Option<String>,
    pub source: String,
    #[serde(default)]
    pub deps: BTreeMap<String, String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
pub struct CommandRequest {
    #[serde(default)]
    pub session: Option<String>,
    pub command: String,
    #[serde(default)]
    pub args: Value,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
pub struct Response {
    pub ok: bool,
    #[cfg_attr(feature = "codegen", ts(type = "number"))]
    pub seq: i64,
    pub result: Value,
    #[serde(default)]
    pub diagnostics: Vec<Diagnostic>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
pub struct Event {
    #[cfg_attr(feature = "codegen", ts(type = "number"))]
    pub seq: i64,
    pub actor: String,
    pub event: Value,
    #[cfg_attr(feature = "codegen", ts(type = "number"))]
    pub handler_seq: i64,
    #[cfg_attr(feature = "codegen", ts(type = "number"))]
    pub ts: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
pub struct Actor {
    pub id: String,
    pub behavior_hash: String,
    pub lang: Lang,
    pub component_hash: Option<String>,
    #[cfg_attr(feature = "codegen", ts(type = "number"))]
    pub last_seq: i64,
    #[cfg_attr(feature = "codegen", ts(type = "number"))]
    pub created_seq: i64,
    pub parent: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
pub struct Snapshot {
    #[cfg_attr(feature = "codegen", ts(type = "number"))]
    pub seq: i64,
    pub state: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
pub struct NameRevision {
    pub name: String,
    pub hash: String,
    #[cfg_attr(feature = "codegen", ts(type = "number"))]
    pub since_seq: i64,
}

/// The language-neutral wire description of a host operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
#[cfg_attr(feature = "codegen", ts(concrete(T = Value)))]
pub struct Desc<T = Value> {
    pub op: String,
    pub args: Value,
    #[serde(skip)]
    #[cfg_attr(feature = "codegen", ts(skip))]
    result: std::marker::PhantomData<fn() -> T>,
}
impl<T> Desc<T> {
    pub fn new(op: impl Into<String>, args: Value) -> Self {
        Self {
            op: op.into(),
            args,
            result: std::marker::PhantomData,
        }
    }
}

mod codec;
mod fs;
mod trace;
pub use trace::{CallTrace, TraceBlob, TraceBlobKind, TraceBundle, TraceEntry, TraceKey, TraceMemo, TraceOutcome};
pub use fs::{DirEntry, EntryKind};
pub use codec::host::{HostValue, decode_host, encode_host, encode_host_array};
pub use codec::{
    ContentAddress, DAG_CBOR_CODEC, RAW_CODEC, cid_for_hash, decode, encode, parse_reference,
    reference,
};
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_descriptor_codec_rejects_trailing_input() {
        let desc = Desc::<Value>::new(
            "random",
            reference(&"ab".repeat(32), DAG_CBOR_CODEC).unwrap(),
        );
        let mut bytes = encode(&desc).unwrap();
        let decoded: Desc = decode(&bytes).unwrap();
        assert_eq!(decoded.op, "random");
        assert_eq!(decoded.args, desc.args);
        bytes.push(0);
        assert!(decode::<Desc>(&bytes).is_err());
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "codegen", derive(TS))]
#[serde(rename_all = "lowercase")]
pub enum LlmRole {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
pub struct LlmMessage {
    pub role: LlmRole,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
pub struct LlmArgs {
    pub model: String,
    pub messages: Vec<LlmMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
pub struct LlmChoice {
    pub index: u32,
    pub message: LlmMessage,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
pub struct LlmResult {
    pub id: String,
    pub model: String,
    pub choices: Vec<LlmChoice>,
    pub usage: Option<Value>,
}

/// Canonical v1 identity shared by checking, persistence and migration.
pub fn definition_identity(
    lang: Lang,
    source: &str,
    deps: &BTreeMap<String, String>,
    allowed_effects: Option<&[String]>,
) -> Result<Vec<u8>, serde_json::Error> {
    let mut identity = serde_json::json!({"version":1,"lang":lang,"source":source,"deps":deps});
    if let Some(labels) = allowed_effects {
        let mut labels = labels.to_vec();
        labels.sort();
        labels.dedup();
        identity["allowed_effects"] = serde_json::to_value(labels)?;
    }
    serde_json::to_vec(&identity)
}
