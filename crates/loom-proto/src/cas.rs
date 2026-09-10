use crate::Value;
use serde::{Deserialize, Serialize};
#[cfg(feature = "codegen")]
use ts_rs::TS;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "codegen", derive(TS))]
pub struct CasCodec {
    #[cfg_attr(feature = "codegen", ts(type = "number"))]
    pub code: u64,
    pub name: String,
    pub cid: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "codegen", derive(TS))]
pub struct CasEntry {
    pub hash: String,
    pub kind: String,
    #[cfg_attr(feature = "codegen", ts(type = "number"))]
    pub size: u64,
    #[cfg_attr(feature = "codegen", ts(type = "number"))]
    pub created_at: i64,
    pub codecs: Vec<CasCodec>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
#[serde(deny_unknown_fields)]
pub struct CasListRequest {
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub after: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub q: Option<String>,
}
fn default_limit() -> usize {
    100
}
impl Default for CasListRequest {
    fn default() -> Self {
        Self {
            limit: default_limit(),
            after: None,
            kind: None,
            q: None,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
pub struct CasPage {
    pub items: Vec<CasEntry>,
    pub next_cursor: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
#[serde(deny_unknown_fields)]
pub struct CasInspectRequest {
    pub hash: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "codegen", derive(TS))]
pub struct CasLink {
    pub path: String,
    pub cid: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
pub struct CasInspection {
    pub entry: CasEntry,
    pub codec: CasCodec,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub value: Option<Value>,
    pub links: Vec<CasLink>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub text: Option<String>,
    pub hex: String,
    pub truncated: bool,
}

/// Canonical directory schema shared by machine snapshots and registry sources.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tree {
    pub entries: Vec<TreeEntry>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeEntry {
    pub name: String,
    pub reference: Value,
    pub directory: bool,
    pub executable: bool,
}
