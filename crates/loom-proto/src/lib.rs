pub mod vm;
pub use vm::{CasReference, VmArchitecture, VmImageEntry, VmImageFormat, VmImageManifest, VmLaunch, VmNetwork, VmSpec};
pub mod script_artifact;
pub use script_artifact::ScriptArtifact;
mod export_identity;
pub use export_identity::export_identity_preimage;
mod cas;
pub mod core_protocol;
pub mod verbs;
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
    Rust,
    JavaScript,
    #[default]
    TypeScript,
}
impl Lang {
    pub fn is_v8(self) -> bool {
        matches!(self, Self::JavaScript | Self::TypeScript)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::JavaScript => "javascript",
            Self::TypeScript => "typescript",
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
    pub event: Value,
    #[cfg_attr(feature = "codegen", ts(type = "number"))]
    pub ts: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
pub struct NameRevision {
    pub name: String,
    pub hash: String,
    #[cfg_attr(feature = "codegen", ts(type = "number"))]
    pub since_seq: i64,
}

/// The language-neutral wire description of a host effect.
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

mod bytes;
mod codec;
mod fs;
pub mod isolated;
mod trace;
pub use bytes::Bytes;
pub use codec::host::{HostValue, decode_host, encode_host, encode_host_array};
pub use codec::{
    ContentAddress, DAG_CBOR_CODEC, RAW_CODEC, cid_for_hash, decode, encode, parse_reference,
    reference,
};
pub use fs::{DirEntry, EntryKind};
pub use trace::{
    CallTrace, TRACE_MAX_BLOB_BYTES, TRACE_MAX_ENTRIES, TRACE_MAX_ERROR_BYTES,
    TRACE_MAX_METADATA_BYTES, TRACE_MAX_SCOPE_BYTES, TraceBlob, TraceBlobKind, TraceBundle,
    TraceEntry, TraceKey, TraceMemo, TraceObservation, TraceOutcome, decode_call_trace,
    encode_call_trace, validate_call_trace_limits,
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

/// Canonical compilation inputs used by the checker and build cache.
/// This is not a definition identity: published definitions use the driver entry hash.
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

/// A JavaScript identity binds source and execution policy to the engine ABI.
/// Loaders recompute this preimage before executing persisted source.
pub fn javascript_definition_identity(
    source: &str,
    deps: &BTreeMap<String, String>,
    allowed_effects: Option<&[String]>,
    backend_abi: &str,
) -> Result<Vec<u8>, serde_json::Error> {
    script_definition_identity(Lang::JavaScript, source, deps, allowed_effects, backend_abi)
}

/// Binds the original language/source and policy to its complete compiler ABI.
pub fn script_definition_identity(
    lang: Lang,
    source: &str,
    deps: &BTreeMap<String, String>,
    allowed_effects: Option<&[String]>,
    backend_abi: &str,
) -> Result<Vec<u8>, serde_json::Error> {
    let definition = definition_identity(lang, source, deps, allowed_effects)?;
    serde_json::to_vec(&serde_json::json!({
        "backend": backend_abi,
        "definition": serde_json::from_slice::<Value>(&definition)?,
    }))
}

/// A closed module graph binds compiler output and fetched source bytes to the
/// original definition. Reopening must load this artifact without network work.
pub fn module_definition_identity(
    lang: Lang,
    source: &str,
    deps: &BTreeMap<String, String>,
    allowed_effects: Option<&[String]>,
    backend_abi: &str,
    artifact_hash: &str,
    compiler: &str,
) -> Result<Vec<u8>, serde_json::Error> {
    let script = script_definition_identity(lang, source, deps, allowed_effects, backend_abi)?;
    let mut identity: Value = serde_json::from_slice(&script)?;
    identity["module"] = serde_json::json!({"artifact":artifact_hash,"compiler":compiler});
    serde_json::to_vec(&identity)
}

/// Content identities emitted by the mandatory item-hashing compiler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildIdentity {
    pub behavior_hash: String,
    pub wasm_hash: String,
    pub toolchain_hash: String,
    pub item_hashes_ref: String,
}
