//! Actor lifecycle in the definition journal (`GET /v1/events`, `/v1/stream`).
//!
//! One writer per event type. A verb that changed the node and then failed
//! to reach the journal reports the failure: the caller learns the journal
//! fell behind instead of reading a clean reply over a silent gap. Only verbs
//! accepted by this daemon are journaled; messages actors exchange among
//! themselves never pass through here.
use super::ActorService;
use crate::Access;
use anyhow::{Context, Result};
use loom_store::Store;
use serde_json::{Value, json};

pub(super) fn actor_spawned(
    store: &Store,
    actor: &str,
    definition_hash: &str,
    name: Option<&str>,
    parent: &str,
) -> Result<i64> {
    store.record_definition_event(&json!({
        "type": "actor_spawned", "actor": actor, "definition_hash": definition_hash,
        "name": name, "parent": parent,
    }))
}

pub(super) fn actor_message(store: &Store, actor: &str, seq: i64, cursor: i64) -> Result<i64> {
    store.record_definition_event(
        &json!({"type": "actor_message", "actor": actor, "seq": seq, "cursor": cursor}),
    )
}

pub(super) fn actor_stopped(
    store: &Store,
    actor: &str,
    definition_hash: &str,
    reason: &str,
) -> Result<i64> {
    store.record_definition_event(&json!({
        "type": "actor_stopped", "actor": actor, "definition_hash": definition_hash,
        "reason": reason,
    }))
}

pub(super) fn actor_restarted(
    store: &Store,
    actor: &str,
    definition_hash: &str,
    verb: &str,
) -> Result<i64> {
    store.record_definition_event(&json!({
        "type": "actor_restarted", "actor": actor, "definition_hash": definition_hash,
        "verb": verb,
    }))
}

pub(super) fn actor_promoted(store: &Store, actor: &str, definition_hash: &str) -> Result<i64> {
    store.record_definition_event(&json!({
        "type": "actor_promoted", "actor": actor, "definition_hash": definition_hash,
    }))
}

/// Journal one successful actor verb. `args` are the verb's arguments after
/// definition references were replaced by hashes; `result` is its reply;
/// `name` is the definition name a `spawn` was addressed by, when it used one.
/// Verbs outside the lifecycle set record nothing.
pub(super) async fn record_verb(
    store: &Store,
    actors: &ActorService,
    command: &str,
    args: &Value,
    result: &Value,
    name: Option<&str>,
) -> Result<()> {
    match command {
        "spawn" => {
            let parent = match args["parent"].as_str() {
                Some(parent) => parent.to_owned(),
                None => actors.node.root(),
            };
            actor_spawned(
                store,
                crate::field(result, "id")?,
                crate::field(args, "def")?,
                name,
                &parent,
            )?;
        }
        "send" => {
            actor_message(
                store,
                crate::field(result, "id")?,
                integer(result, "seq")?,
                integer(result, "cursor")?,
            )?;
        }
        "stop" => {
            let actor = crate::field(args, "id")?;
            let hash = behavior_hash(actors, actor).await?;
            actor_stopped(store, actor, &hash, crate::field(args, "reason")?)?;
        }
        "restart" => {
            let actor = crate::field(args, "id")?;
            let hash = behavior_hash(actors, actor).await?;
            actor_restarted(store, actor, &hash, crate::field(args, "verb")?)?;
        }
        "promote" => {
            actor_promoted(
                store,
                crate::field(args, "id")?,
                crate::field(args, "hash")?,
            )?;
        }
        _ => {}
    }
    Ok(())
}

fn integer(value: &Value, key: &str) -> Result<i64> {
    value[key]
        .as_i64()
        .with_context(|| format!("actor reply field {key} must be an integer"))
}

/// The pinned definition after the verb ran, through the `info` verb so a
/// remotely owned actor answers the same way a local one does. The caller
/// already passed the verb's own authority check.
async fn behavior_hash(actors: &ActorService, actor: &str) -> Result<String> {
    let info = actors
        .command(&Access::owner(), "info", json!({"id": actor}))
        .await
        .with_context(|| format!("journal actor {actor}: read pinned definition"))?;
    Ok(crate::field(&info, "behavior_hash")?.to_owned())
}
