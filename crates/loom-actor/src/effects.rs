use crate::{Verdict, actor};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EffectKey {
    pub actor_id: String,
    pub seq: i64,
    pub idx: i64,
    /// Reset incarnation; external idempotency keys must include this field.
    pub generation: i64,
}

/// Handlers classify failures explicitly: only environmental failures are retried.
#[derive(Debug, thiserror::Error)]
pub enum EffectError {
    #[error("environmental effect error: {0:#}")]
    Environmental(anyhow::Error),
    #[error("deterministic effect error: {0:#}")]
    Deterministic(anyhow::Error),
}

#[async_trait]
pub trait EffectHandler: Send + Sync {
    /// External mutations must deduplicate this key across calls and restarts.
    async fn call(&self, key: &EffectKey, kind: &str, req: &[u8]) -> Result<Vec<u8>, EffectError>;
}

#[derive(Default)]
pub struct DefaultEffects;

pub(crate) fn now() -> Result<i64> {
    Ok(i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())?)
}

#[async_trait]
impl EffectHandler for DefaultEffects {
    async fn call(&self, key: &EffectKey, kind: &str, req: &[u8]) -> Result<Vec<u8>, EffectError> {
        let result: Result<Vec<u8>, EffectError> = async {
            match kind {
                "echo" => Ok(req.to_vec()),
                "now" => {
                    if !req.is_empty() {
                        return Err(EffectError::Deterministic(anyhow!("now takes no request bytes")));
                    }
                    Ok(now().map_err(EffectError::Environmental)?.to_le_bytes().to_vec())
                }
                // A JSON unsigned integer is a delay in milliseconds; reply echoes it.
                "alarm" => {
                    let delay: u64 = serde_json::from_slice(req).map_err(|error| EffectError::Deterministic(error.into()))?;
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    Ok(req.to_vec())
                }
                _ => Err(EffectError::Deterministic(anyhow!("unknown effect kind {kind:?}"))),
            }
        }
        .await;
        result.map_err(|error| {
            let context = format!("actor {} seq {} effect {}", key.actor_id, key.seq, key.idx);
            match error {
                EffectError::Environmental(error) => EffectError::Environmental(error.context(context)),
                EffectError::Deterministic(error) => EffectError::Deterministic(error.context(context)),
            }
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Position {
    seq: i64,
    idx: i64,
}
#[derive(Serialize)]
struct Signature<'a> {
    kind: &'a str,
    request: &'a [u8],
}
struct Recorded {
    kind: String,
    request: Vec<u8>,
    result: Vec<u8>,
}

pub(crate) struct ReplayEffects {
    records: BTreeMap<Position, Recorded>,
    seen: Mutex<BTreeSet<Position>>,
    divergence: Mutex<Option<Verdict>>,
}

impl ReplayEffects {
    pub async fn load(conn: &turso::Connection) -> Result<Self> {
        let rows = actor::query(conn, "SELECT seq,idx,kind,request,result FROM effects ORDER BY seq,idx", ()).await?;
        let mut records = BTreeMap::new();
        for row in rows.rows {
            records.insert(
                Position { seq: row.get(0)?, idx: row.get(1)? },
                Recorded { kind: row.get(2)?, request: row.get(3)?, result: row.get(4)? },
            );
        }
        Ok(Self { records, seen: Mutex::new(BTreeSet::new()), divergence: Mutex::new(None) })
    }

    pub async fn begin(&self, seq: i64) {
        self.seen.lock().await.retain(|position| position.seq != seq);
    }

    pub async fn finish(&self, seq: i64, completed: bool) -> Result<Option<Verdict>> {
        if let Some(verdict) = self.divergence.lock().await.clone() {
            return Ok(Some(verdict));
        }
        if !completed {
            return Ok(None);
        }
        let seen = self.seen.lock().await;
        for (position, record) in &self.records {
            if position.seq == seq && !seen.contains(position) {
                return Ok(Some(Verdict::DivergedAt {
                    seq,
                    idx: position.idx,
                    expected: serde_json::to_vec(&Signature { kind: &record.kind, request: &record.request })?,
                    got: Vec::new(),
                }));
            }
        }
        Ok(None)
    }
}

#[async_trait]
impl EffectHandler for ReplayEffects {
    async fn call(&self, key: &EffectKey, kind: &str, req: &[u8]) -> Result<Vec<u8>, EffectError> {
        let position = Position { seq: key.seq, idx: key.idx };
        let record = self.records.get(&position);
        if let Some(record) = record
            && record.kind == kind
            && record.request == req
        {
            self.seen.lock().await.insert(position);
            return Ok(record.result.clone());
        }
        let expected = match record {
            Some(record) => serde_json::to_vec(&Signature { kind: &record.kind, request: &record.request })
                .map_err(|error| EffectError::Deterministic(error.into()))?,
            None => Vec::new(),
        };
        let mut divergence = self.divergence.lock().await;
        if divergence.is_none() {
            *divergence = Some(Verdict::DivergedAt {
                seq: key.seq,
                idx: key.idx,
                expected,
                got: serde_json::to_vec(&Signature { kind, request: req }).map_err(|error| EffectError::Deterministic(error.into()))?,
            });
        }
        Err(EffectError::Deterministic(anyhow!("actor {} seq {}: effect {} differs from recorded history", key.actor_id, key.seq, key.idx)))
    }
}

pub(crate) struct RuntimeEffects<'a> {
    pub node: &'a crate::Node,
    pub external: &'a dyn EffectHandler,
}
#[async_trait]
impl EffectHandler for RuntimeEffects<'_> {
    async fn call(&self, key: &EffectKey, kind: &str, req: &[u8]) -> Result<Vec<u8>, EffectError> {
        if kind == "__inspect" {
            let id = std::str::from_utf8(req).map_err(|error| EffectError::Deterministic(error.into()))?;
            if id == key.actor_id {
                return Err(EffectError::Deterministic(anyhow!("actor {} seq {}: cannot inspect self", key.actor_id, key.seq)));
            }
            let state = self.node.child_state(id).await.map_err(EffectError::Environmental)?;
            return serde_json::to_vec(&state).map_err(|error| EffectError::Deterministic(error.into()));
        }
        self.external.call(key, kind, req).await
    }
}
