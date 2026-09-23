//! Actor authority is opaque token bytes obtained from messages or spawning.
use crate::{
    EffectError,
    isolated::{Def, Invocation},
    perform,
};

/// Host-authenticated token. Deserialization alone does not validate authority.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct Cap {
    pub token: Vec<u8>,
}

/// Name the host uses for a message sent through the node API (CLI, HTTP, MCP);
/// mirrors `loom_actor::EXTERNAL_SENDER`.
pub const EXTERNAL: &str = "external";

/// Who sent the message being handled: an actor id, [`EXTERNAL`] for an operator
/// using the node API, or `None` for host-internal messages. Identity only; it grants
/// no authority (see `actor.sender_cap` for a capability, which external senders lack).
pub fn sender() -> Result<Option<String>, EffectError> {
    perform("actor.sender", serde_json::Value::Null)
}

pub fn send(cap: &Cap, msg: &[u8]) -> Result<(), EffectError> {
    perform("actor.send", serde_json::json!({"cap":cap,"msg":msg}))
}

/// Verify and persist a capability received in a message, returning its handle.
pub fn accept(cap: Cap) -> Result<Cap, EffectError> {
    perform::<()>("actor.accept", serde_json::json!({"cap":cap}))?;
    Ok(cap)
}

/// Spawn a worker using ChildSpec's host-owned restart, link, and shutdown defaults.
/// `"$self"` (`Def::this()`) spawns the running behavior; the host resolves it.
pub fn spawn<F: Invocation>(def: Def<F>, init: &[u8]) -> Result<Cap, EffectError> {
    perform(
        "actor.spawn",
        serde_json::json!({"behavior_hash":def.hash(),"init":init,"type":"worker"}),
    )
}
