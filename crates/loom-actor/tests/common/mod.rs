use crate::registry::Registry;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use loom_actor::{Actor, Behavior, EffectError, EffectHandler, EffectKey};
use turso::Value;

pub const H1: &str = "counter-v1";
pub const H2: &str = "counter-v2";
pub const EXTRA: &str = "counter-extra-effect";

pub use loom_actor::builtin::{Counter, Forwarder};

#[derive(Clone, Debug)]
pub struct RecordedCall {
    pub actor_id: String,
    pub seq: i64,
    pub idx: i64,
    pub kind: String,
    pub request: Vec<u8>,
}

#[derive(Default)]
pub struct RecordingEffects {
    pub calls: Mutex<Vec<RecordedCall>>,
    pub fail_seq_two_once: AtomicBool,
    pub reject_deterministically: bool,
}

#[async_trait]
impl EffectHandler for RecordingEffects {
    async fn call(&self, key: &EffectKey, kind: &str, req: &[u8]) -> Result<Vec<u8>, EffectError> {
        self.calls.lock().unwrap().push(RecordedCall {
            actor_id: key.actor_id.clone(),
            seq: key.seq,
            idx: key.idx,
            kind: kind.to_owned(),
            request: req.to_vec(),
        });
        if key.seq == 2 && key.idx == 0 && self.fail_seq_two_once.swap(false, Ordering::SeqCst) {
            return Err(EffectError::Environmental(anyhow::anyhow!(
                "injected pre-commit effect failure for actor {} seq {}",
                key.actor_id,
                key.seq
            )));
        }
        if self.reject_deterministically {
            return Err(EffectError::Deterministic(anyhow::anyhow!("rejected deterministic request")));
        }
        Ok(req.to_vec())
    }
}

pub fn registry(behaviors: Vec<Arc<dyn Behavior>>) -> Arc<dyn loom_actor::Registry> {
    let mut registry = Registry::new();
    for behavior in behaviors {
        registry.insert(behavior.hash().to_owned(), behavior);
    }
    Arc::new(registry)
}

pub async fn integer(actor: &Actor, sql: &str) -> i64 {
    let rows = actor.sql(sql, ()).await.unwrap();
    assert_eq!(rows.rows.len(), 1);
    match rows.rows[0].get_value(0).unwrap() {
        Value::Integer(value) => value,
        other => panic!("expected integer, got {other:?}"),
    }
}

pub async fn table_fingerprints(actor: &Actor) -> BTreeMap<String, String> {
    let tables = actor.sql("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name", ()).await.unwrap();
    let mut result = BTreeMap::new();
    for table in tables.rows {
        let Value::Text(name) = table.get_value(0).unwrap() else { panic!("table name is not text") };
        let sql = format!("SELECT * FROM \"{}\" ORDER BY rowid", name.replace('"', "\"\""));
        let rows = actor.sql(&sql, ()).await.unwrap();
        let mut hasher = blake3::Hasher::new();
        for column in rows.columns {
            hasher.update(&(column.len() as u64).to_le_bytes());
            hasher.update(column.as_bytes());
        }
        for row in rows.rows {
            hasher.update(&(row.column_count() as u64).to_le_bytes());
            for column in 0..row.column_count() {
                let value = format!("{:?}", row.get_value(column).unwrap());
                hasher.update(&(value.len() as u64).to_le_bytes());
                hasher.update(value.as_bytes());
            }
        }
        result.insert(name, hasher.finalize().to_hex().to_string());
    }
    result
}
