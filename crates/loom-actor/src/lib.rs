//! One Turso file per actor; committed outboxes are delivered by [`Node`].
//! Native behaviors must keep mutable state in SQL and use `Ctx` for effects.
#![forbid(unsafe_code)]

mod actor;
pub mod builtin;
mod directory;
mod effects;
mod history;
mod hooks;
mod ids;
mod lifecycle;
mod mailbox;
mod messaging;
mod node;
mod pump;
mod reset;
mod scheduler;
mod schema;
mod supervision;
mod supervisor;
mod supervisor_store;
mod types;

pub use actor::Actor;
pub use directory::{ActorInfo, MonitorInfo};
pub use effects::{DefaultEffects, EffectError, EffectHandler, EffectKey};
pub use history::memo::{MemoConfig, PromoteReport};
pub use ids::ActorId;
pub use node::Node;
pub use schema::SCHEMA;
pub use supervisor::Supervisor;
pub use turso::{IntoParams, Value};
pub use types::{
    AssertionResult, ChildSpec, ChildState, ChildType, Config, Io, RestartPolicy, RestartVerb, Rows, Shutdown, Status, TableDifference,
    TableHash, Trap, TreeEntry, ValidationResult, Verdict,
};

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};

pub type Registry = HashMap<String, Arc<dyn Behavior>>;

#[async_trait]
pub trait Behavior: Send + Sync {
    fn hash(&self) -> &str;
    fn child_type(&self) -> ChildType {
        ChildType::Worker
    }
    fn description(&self) -> &str {
        "Registered native behavior."
    }
    fn schema(&self) -> &str;
    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap>;
    async fn terminate(&self, _cx: &mut Ctx<'_>, _reason: &str) -> Result<(), Trap> {
        Ok(())
    }
    async fn upgrade(&self, _cx: &mut Ctx<'_>, _from_hash: &str) -> Result<(), Trap> {
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub(crate) enum Spawn {
    Child { id: ActorId, spec: ChildSpec, origin_seq: i64, origin_idx: i64 },
    Restart { id: ActorId, verb: RestartVerb },
}

struct OutboxPosition {
    seq: i64,
    idx: i64,
}

pub struct Ctx<'a> {
    pub(crate) conn: &'a turso::Connection,
    pub(crate) actor_id: &'a str,
    pub(crate) seq: i64,
    pub(crate) sender: Option<ActorId>,
    pub(crate) generation: i64,
    pub(crate) idx: i64,
    pub(crate) random_counter: u64,
    pub(crate) effects: &'a dyn EffectHandler,
    // Failed operations cannot be hidden by ignoring their returned error.
    pub(crate) failure: Option<Trap>,
    pub(crate) deferred: bool,
}

impl Ctx<'_> {
    /// Mark an environmental failure for retry after the message rolls back.
    pub fn runtime(&mut self, error: impl std::fmt::Display) -> Trap {
        let message = format!("actor {} seq {}: {error}", self.actor_id, self.seq);
        self.failure.get_or_insert_with(|| Trap { message: message.clone(), runtime: true });
        Trap { message, runtime: true }
    }

    fn effect_error(&mut self, error: EffectError) -> Trap {
        let runtime = matches!(&error, EffectError::Environmental(_));
        let message = format!("actor {} seq {}: {error}", self.actor_id, self.seq);
        if !runtime && self.failure.as_ref().is_some_and(|failure| failure.runtime) {
            self.failure = None;
        }
        self.failure.get_or_insert_with(|| Trap { message: message.clone(), runtime });
        Trap { message, runtime }
    }

    pub fn seq(&self) -> i64 {
        self.seq
    }

    /// Execute one statement on this message's transaction and drain all rows.
    /// Behaviors own domain SQL; transaction control and runtime-table mutation
    /// are outside the Behavior contract.
    pub async fn sql(&mut self, sql: &str, params: impl IntoParams + Send) -> Result<Rows, Trap> {
        actor::query(self.conn, sql, params).await.map_err(|e| self.runtime(e))
    }

    pub(crate) fn next_index(&mut self) -> Result<i64, Trap> {
        let idx = self.idx;
        self.idx = self.idx.checked_add(1).ok_or_else(|| Trap::new("effect index overflow"))?;
        Ok(idx)
    }

    pub(crate) async fn outbox(&mut self, idx: i64, target: &str, msg: &[u8]) -> Result<(), Trap> {
        self.insert_outbox(idx, target, msg).await?;
        Ok(())
    }

    // Logical inbox positions remain effect/child identities. Physical positions
    // follow committed send order even when an older deferred message runs later.
    async fn insert_outbox(&mut self, idx: i64, target: &str, msg: &[u8]) -> Result<OutboxPosition, Trap> {
        let rows = actor::query(self.conn, "SELECT seq,idx FROM outbox ORDER BY seq DESC,idx DESC LIMIT 1", ())
            .await
            .map_err(|e| self.runtime(e))?;
        let mut position = OutboxPosition { seq: self.seq.max(0), idx };
        if let Some(row) = rows.rows.first() {
            let last_seq: i64 = row.get(0).map_err(|e| self.runtime(e))?;
            let last_idx: i64 = row.get(1).map_err(|e| self.runtime(e))?;
            if position.seq < last_seq || (position.seq == last_seq && position.idx <= last_idx) {
                position.seq = last_seq;
                position.idx = last_idx.checked_add(1).ok_or_else(|| self.runtime("outbox index overflow"))?;
            }
        }
        self.conn
            .execute("INSERT INTO outbox(seq,idx,target,msg) VALUES (?,?,?,?)", turso::params![position.seq, position.idx, target, msg])
            .await
            .map_err(|e| self.runtime(e))?;
        Ok(position)
    }

    pub async fn send(&mut self, target: &str, msg: &[u8]) -> Result<(), Trap> {
        ids::check(target).map_err(|e| Trap::new(e.to_string()))?;
        let idx = self.next_index()?;
        self.outbox(idx, target, msg).await
    }

    pub async fn spawn(&mut self, spec: &ChildSpec) -> Result<ActorId, Trap> {
        let idx = self.next_index()?;
        let id = ids::child(&ids::incarnation(self.actor_id, self.generation), self.seq, idx);
        let spawn = Spawn::Child { id: id.clone(), spec: spec.clone(), origin_seq: self.seq, origin_idx: idx };
        let msg = serde_json::to_vec(&spawn).map_err(|e| self.runtime(e))?;
        self.outbox(idx, "spawn", &msg).await?;
        let restart = match spec.restart {
            RestartPolicy::Permanent => "permanent",
            RestartPolicy::Transient => "transient",
            RestartPolicy::Temporary => "temporary",
        };
        let shutdown = serde_json::to_string(&spec.shutdown).map_err(|e| self.runtime(e))?;
        self.conn.execute("INSERT INTO children(id,spawned_seq,behavior_hash,init,restart,shutdown,link,monitor,child_type) VALUES (?,?,?,?,?,?,?,?,?)",
            turso::params![id.as_str(), self.seq, spec.behavior_hash.as_str(), spec.init.as_slice(), restart, shutdown, spec.link, spec.monitor, serde_json::to_string(&spec.child_type).map_err(|e| self.runtime(e))?]).await.map_err(|e| self.runtime(e))?;
        Ok(id)
    }

    pub fn sender(&self) -> Option<ActorId> {
        self.sender.clone()
    }
    pub fn self_id(&self) -> &str {
        self.actor_id
    }
    async fn control(&mut self, kind: &str, target: &str, msg: &[u8]) -> Result<(), Trap> {
        let idx = self.next_index()?;
        self.outbox(idx, &format!("{kind}:{target}"), msg).await
    }
    pub async fn monitor(&mut self, id: &str) -> Result<String, Trap> {
        ids::check(id).map_err(|e| Trap::new(e.to_string()))?;
        let idx = self.next_index()?;
        let reference = format!("{}:{}:{idx}", ids::incarnation(self.actor_id, self.generation), self.seq);
        self.outbox(idx, &format!("monitor:{id}"), reference.as_bytes()).await?;
        Ok(reference)
    }
    pub async fn demonitor(&mut self, reference: &str, flush: bool) -> Result<(), Trap> {
        let rows = self.sql("SELECT target FROM monitors WHERE ref=?", [reference]).await?;
        let target = match rows.rows.first() {
            Some(row) => row.get::<String>(0).map_err(|e| self.runtime(e))?,
            None => String::new(),
        };
        let msg = serde_json::to_vec(&serde_json::json!({"target":target,"flush":flush})).map_err(|e| self.runtime(e))?;
        self.control("demonitor", reference, &msg).await
    }
    pub async fn link(&mut self, id: &str) -> Result<(), Trap> {
        self.control("link", id, &[]).await
    }
    pub async fn unlink(&mut self, id: &str) -> Result<(), Trap> {
        self.control("unlink", id, &[]).await
    }
    pub async fn stop(&mut self, id: &str, reason: &str) -> Result<(), Trap> {
        self.control("stop", id, reason.as_bytes()).await
    }
    pub fn defer(&mut self) -> Result<(), Trap> {
        self.deferred = true;
        Ok(())
    }
    pub async fn shutdown(&mut self, id: &str) -> Result<(), Trap> {
        self.control("shutdown", id, &[]).await
    }
    pub async fn exit(&mut self, reason: &str) -> Result<(), Trap> {
        let id = self.actor_id.to_owned();
        self.stop(&id, reason).await
    }
    pub async fn trap_exit(&mut self, enabled: bool) -> Result<(), Trap> {
        actor::set_meta(self.conn, "trap_exit", if enabled { "true" } else { "false" }).await.map_err(|e| self.runtime(e))
    }
    pub async fn restart(&mut self, id: &str, verb: RestartVerb) -> Result<(), Trap> {
        if id == self.actor_id {
            return Err(Trap::new("restart self through a supervisor or Node::restart"));
        }
        let msg = serde_json::to_vec(&Spawn::Restart { id: id.into(), verb }).map_err(|e| self.runtime(e))?;
        let idx = self.next_index()?;
        self.outbox(idx, "spawn", &msg).await
    }
    /// Inspect an owned child through a recorded effect, so supervision replays.
    pub async fn inspect(&mut self, id: &str) -> Result<ChildState, Trap> {
        let rows = self.sql("SELECT id FROM children WHERE id=?", [id]).await?;
        if rows.rows.is_empty() {
            return Err(Trap::new("inspect requires an owned child"));
        }
        let bytes = self.effect("__inspect", id.as_bytes()).await?;
        serde_json::from_slice(&bytes).map_err(|e| self.runtime(e))
    }

    pub async fn request(&mut self, kind: &str, req: &[u8]) -> Result<String, Trap> {
        let idx = self.next_index()?;
        let position = self.insert_outbox(idx, &format!("effect:{kind}"), req).await?;
        Ok(format!("req:{}:{}", position.seq, position.idx))
    }

    pub async fn effect(&mut self, kind: &str, req: &[u8]) -> Result<Vec<u8>, Trap> {
        let idx = self.next_index()?;
        let key = EffectKey { actor_id: self.actor_id.into(), seq: self.seq, idx, generation: self.generation };
        let result = self.effects.call(&key, kind, req).await.map_err(|e| self.effect_error(e))?;
        self.conn
            .execute(
                "INSERT INTO effects(seq,idx,kind,request,result) VALUES (?,?,?,?,?)",
                turso::params![self.seq, idx, kind, req, result.as_slice()],
            )
            .await
            .map_err(|e| self.runtime(e))?;
        Ok(result)
    }

    /// Host wall clock in milliseconds since the Unix epoch, recorded as an effect.
    pub async fn now(&mut self) -> Result<i64, Trap> {
        let result = self.effect("now", &[]).await?;
        let bytes: [u8; 8] = result.try_into().map_err(|_| self.runtime("now requires eight result bytes"))?;
        Ok(i64::from_le_bytes(bytes))
    }

    /// Deterministic XOF bytes; the counter is reset on every message attempt.
    pub fn random(&mut self, n: usize) -> Vec<u8> {
        let mut hash = blake3::Hasher::new();
        hash.update(self.actor_id.as_bytes());
        hash.update(&self.seq.to_le_bytes());
        hash.update(&self.random_counter.to_le_bytes());
        self.random_counter = self.random_counter.wrapping_add(1);
        let mut bytes = vec![0; n];
        hash.finalize_xof().fill(&mut bytes);
        bytes
    }
}
