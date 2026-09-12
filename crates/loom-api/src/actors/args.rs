use serde::Deserialize;
use serde_json::Value;
#[derive(Deserialize)]
pub struct IdArgs {
    pub(super) id: String,
}
#[derive(Deserialize)]
pub struct TreeArgs {
    pub(super) root: Option<String>,
}
#[derive(Deserialize)]
pub struct SendArgs {
    pub(super) id: String,
    pub(super) key: Option<String>,
    pub(super) msg: Value,
}
#[derive(Deserialize)]
pub struct SpawnArgs {
    pub(super) behavior_hash: String,
    /// JSON initialization message; null creates an empty inbox.
    pub(super) init: Value,
    pub(super) parent: Option<String>,
    /// Optional restart, shutdown, link, monitor, and type fields from ChildSpec.
    pub(super) spec: Option<Value>,
}
#[derive(Deserialize)]
pub struct StopArgs {
    pub(super) id: String,
    pub(super) reason: String,
}
#[derive(Deserialize)]
pub struct RestartArgs {
    pub(super) id: String,
    /// One of resume, skip, reset.
    pub(super) verb: String,
}
#[derive(Deserialize)]
pub struct PromoteArgs {
    pub(super) id: String,
    pub(super) behavior_hash: String,
    pub(super) author: String,
    pub(super) rationale: String,
}
#[derive(Deserialize)]
pub struct PromoteWhereArgs {
    pub(super) old_hash: String,
    pub(super) new_hash: String,
    pub(super) author: String,
    pub(super) rationale: String,
}
#[derive(Deserialize)]
pub struct ForkArgs {
    pub(super) id: String,
    pub(super) at_seq: i64,
}
#[derive(Deserialize)]
pub struct ValidateArgs {
    pub(super) id: String,
    pub(super) candidate_hash: String,
    pub(super) k: i64,
    /// Read-only SQL evaluated on the candidate; one nonzero numeric scalar passes.
    pub(super) assertions: Option<Vec<String>>,
}
#[derive(Deserialize)]
pub struct SqlArgs {
    pub(super) id: String,
    pub(super) query: String,
    pub(super) params: Option<Vec<Value>>,
}
#[derive(Deserialize)]
pub struct NameArgs {
    pub(super) name: String,
}
#[derive(Deserialize)]
pub struct RegisterArgs {
    pub(super) name: String,
    pub(super) id: String,
}
#[derive(Deserialize)]
pub struct GroupArgs {
    pub(super) group: String,
}
