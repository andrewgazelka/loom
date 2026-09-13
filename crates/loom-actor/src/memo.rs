//! Content-addressed validation records. `store` evicts oldest records; there is no invalidation.
use crate::{ActorId, AssertionResult, Node, TableDifference, TableHash, ValidationResult, Verdict, actor};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use turso::{Connection, Value};

#[derive(Clone, Copy, Debug)]
pub struct MemoConfig {
    /// Maximum retained records. `store` leaves the retained state by evicting oldest `created_at` first.
    pub max_rows: usize,
}
impl Default for MemoConfig {
    fn default() -> Self {
        Self { max_rows: 10_000 }
    }
}

#[derive(Debug, Serialize)]
pub struct PromoteReport {
    pub verdict: Verdict,
    pub downstream_unaffected: bool,
    pub receivers: Vec<ActorId>,
}

pub(super) struct Record {
    pub key: String,
    pub result: ValidationResult,
    pub tables: BTreeMap<String, String>,
    pub outbox_hash: String,
}

// The persisted JSON boundary owns decoding until types.rs derives Deserialize.
#[derive(Deserialize)]
#[serde(remote = "TableHash")]
struct TableHashJson {
    name: String,
    hash: String,
}
#[derive(Deserialize)]
#[serde(remote = "TableDifference")]
struct TableDifferenceJson {
    name: String,
    original_hash: String,
    fork_hash: String,
}
#[derive(Deserialize)]
#[serde(transparent)]
struct HashJson {
    #[serde(with = "TableHashJson")]
    value: TableHash,
}
#[derive(Deserialize)]
#[serde(transparent)]
struct DifferenceJson {
    #[serde(with = "TableDifferenceJson")]
    value: TableDifference,
}
#[derive(Deserialize)]
enum VerdictJson {
    Matched { tables: Vec<HashJson> },
    Differs { tables: Vec<DifferenceJson> },
    DivergedAt { seq: i64, idx: i64, expected: Vec<u8>, got: Vec<u8> },
    Trapped { seq: i64, error: String },
}
impl From<VerdictJson> for Verdict {
    fn from(value: VerdictJson) -> Self {
        match value {
            VerdictJson::Matched { tables } => Self::Matched { tables: tables.into_iter().map(|v| v.value).collect() },
            VerdictJson::Differs { tables } => Self::Differs { tables: tables.into_iter().map(|v| v.value).collect() },
            VerdictJson::DivergedAt { seq, idx, expected, got } => Self::DivergedAt { seq, idx, expected, got },
            VerdictJson::Trapped { seq, error } => Self::Trapped { seq, error },
        }
    }
}
#[derive(Deserialize)]
struct AssertionJson {
    query: String,
    passed: bool,
}
#[derive(Deserialize)]
struct StoredTables {
    hashes: BTreeMap<String, String>,
    assertions: Vec<AssertionJson>,
}

async fn connection(node: &Node) -> Result<Connection> {
    let conn = actor::connect(&node.dir.join("_node.db"), node.config.io).await?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS validation_memo(key TEXT PRIMARY KEY, verdict BLOB, tables BLOB, outbox_hash TEXT, created_at INTEGER)",
        (),
    )
    .await?;
    Ok(conn)
}

pub(super) async fn lookup(node: &Node, key: &str) -> Result<Option<Record>> {
    let _guard = node.names.lock().await;
    let conn = connection(node).await?;
    let rows = actor::query(&conn, "SELECT verdict,tables,outbox_hash FROM validation_memo WHERE key=?", [key]).await?;
    let Some(row) = rows.rows.first() else { return Ok(None) };
    let verdict: VerdictJson = serde_json::from_slice(&row.get::<Vec<u8>>(0)?).context("validation_memo verdict JSON")?;
    let tables: StoredTables = serde_json::from_slice(&row.get::<Vec<u8>>(1)?).context("validation_memo tables JSON")?;
    Ok(Some(Record {
        key: key.to_owned(),
        result: ValidationResult {
            verdict: verdict.into(),
            assertions: tables.assertions.into_iter().map(|v| AssertionResult { query: v.query, passed: v.passed }).collect(),
        },
        tables: tables.hashes,
        outbox_hash: row.get(2)?,
    }))
}

pub(super) async fn store(node: &Node, key: &str, record: &Record, config: MemoConfig) -> Result<()> {
    let max_rows = i64::try_from(config.max_rows).context("validation_memo max_rows exceeds SQLite integer")?;
    let _guard = node.names.lock().await;
    let mut conn = connection(node).await?;
    let tx = conn.transaction().await?;
    let verdict = serde_json::to_vec(&serde_json::to_value(&record.result.verdict)?)?;
    let tables = serde_json::to_vec(&serde_json::json!({"hashes":record.tables,"assertions":record.result.assertions}))?;
    tx.execute(
        "INSERT OR IGNORE INTO validation_memo VALUES (?,?,?,?,(SELECT COALESCE(MAX(created_at),0)+1 FROM validation_memo))",
        turso::params![key, verdict, tables, record.outbox_hash.as_str()],
    )
    .await?;
    tx.execute(
        "DELETE FROM validation_memo WHERE key IN (SELECT key FROM validation_memo ORDER BY created_at DESC,key DESC LIMIT -1 OFFSET ?)",
        [max_rows],
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub(super) fn field(hash: &mut blake3::Hasher, bytes: &[u8]) {
    hash.update(&(bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

pub(super) async fn rows_hash(conn: &Connection, sql: &str, params: impl turso::IntoParams) -> Result<String> {
    let rows = actor::query(conn, sql, params).await?;
    let mut hash = blake3::Hasher::new();
    for row in rows.rows {
        hash.update(&(row.column_count() as u64).to_le_bytes());
        for index in 0..row.column_count() {
            let value = row.get_value(index)?;
            match value {
                Value::Null => {
                    hash.update(&[0]);
                }
                Value::Integer(value) => {
                    hash.update(&[1]);
                    field(&mut hash, &value.to_le_bytes());
                }
                Value::Real(value) => {
                    hash.update(&[2]);
                    field(&mut hash, &value.to_bits().to_le_bytes());
                }
                Value::Text(value) => {
                    hash.update(&[3]);
                    field(&mut hash, value.as_bytes());
                }
                Value::Blob(value) => {
                    hash.update(&[4]);
                    field(&mut hash, &value);
                }
            }
        }
    }
    Ok(hash.finalize().to_hex().to_string())
}

pub(super) async fn key(source: &Connection, candidate: &str, at: i64, end: i64, assertions: &[String]) -> Result<String> {
    let rows = actor::query(source, "SELECT seq,path FROM snapshots WHERE seq<=? ORDER BY seq DESC LIMIT 1", [at]).await?;
    let snapshot = rows.rows.first().context("validation_memo: no historical snapshot")?;
    let path: String = snapshot.get(1)?;
    let bytes = if path.starts_with("memory:") {
        // Immutable node-local snapshot identity; the remaining key includes every replay input.
        path.as_bytes().to_vec()
    } else {
        std::fs::read(&path).with_context(|| format!("validation_memo snapshot {path}"))?
    };
    let mut hash = blake3::Hasher::new();
    field(&mut hash, b"loom-validation-memo-v1");
    field(&mut hash, candidate.as_bytes());
    field(&mut hash, blake3::hash(&bytes).as_bytes());
    field(&mut hash, &at.to_le_bytes());
    field(&mut hash, &end.to_le_bytes());
    for sql in [
        "SELECT seq,key,sender,msg,received_at FROM inbox WHERE seq>? AND seq<=? ORDER BY seq",
        "SELECT seq,idx,kind,request,result FROM effects WHERE seq>? AND seq<=? ORDER BY seq,idx",
    ] {
        field(&mut hash, rows_hash(source, sql, turso::params![at, end]).await?.as_bytes());
    }
    field(&mut hash, &serde_json::to_vec(assertions)?);
    // Sparse snapshots, deferred commit order, upgrades, and the comparison baseline
    // are also replay inputs in the current engine. Never reuse a verdict across them.
    for sql in [
        "SELECT key,value FROM meta ORDER BY key",
        "SELECT * FROM code_changes ORDER BY seq",
        "SELECT seq,key,sender,msg,received_at FROM inbox ORDER BY seq",
        "SELECT seq,idx,kind,request,result FROM effects ORDER BY seq,idx",
    ] {
        field(&mut hash, rows_hash(source, sql, ()).await?.as_bytes());
    }
    field(&mut hash, &serde_json::to_vec(&super::table_hashes(source).await?)?);
    Ok(hash.finalize().to_hex().to_string())
}

pub(super) async fn outbox_hash(conn: &Connection, at: i64, end: i64) -> Result<String> {
    rows_hash(conn, "SELECT seq,idx,target,msg FROM outbox WHERE seq>? AND seq<=? ORDER BY seq,idx", turso::params![at, end]).await
}
