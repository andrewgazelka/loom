mod cas_browser;
mod dag_migration;
mod definitions;
mod effect_index;
mod effects;
mod events;
mod identity;
mod intake;
mod language;
mod legacy;
mod machine;
mod migration;
mod objects;
mod projections;
mod publication;
mod recording;
#[cfg(test)]
mod tests;
mod trace;
mod update;
pub use update::UpdateSession;
use anyhow::{Context, Result, anyhow, ensure};
pub use intake::IntakePublication;
use loom_proto::{Def, Event, Value};
pub use machine::MachineRoot;
pub use recording::RecordingTimings;
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
};
pub use trace::{TraceEffect, TraceEffectsPage};

#[derive(Clone)]
pub struct Store {
    recording: Arc<recording::Writer>,
    connection: Arc<Mutex<Connection>>,
}
impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::initialize(Connection::open(path)?, recording::Durability::Wal)
    }
    pub fn memory() -> Result<Self> {
        Self::initialize(
            Connection::open_in_memory()?,
            recording::Durability::Ephemeral,
        )
    }
    fn initialize(mut connection: Connection, durability: recording::Durability) -> Result<Self> {
        legacy::validate(&connection)?;
        language::validate(&connection)?;
        connection.create_scalar_function(
            "loom_json",
            1,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8
                | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
            |context| {
                let bytes: Vec<u8> = context.get(0)?;
                decode::<Value>(&bytes)
                    .and_then(|value| Ok(serde_json::to_vec(&value)?))
                    .map_err(|e| rusqlite::Error::UserFunctionError(e.into()))
            },
        )?;
        connection.create_scalar_function(
            "loom_trace_json",
            1,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8
                | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
            |context| {
                let bytes: Vec<u8> = context.get(0)?;
                let trace = loom_proto::decode_call_trace(&bytes)
                    .map_err(|error| rusqlite::Error::UserFunctionError(anyhow!(error).into()))?;
                serde_json::to_vec(&trace)
                    .map_err(|error| rusqlite::Error::UserFunctionError(Box::new(error)))
            },
        )?;
        dag_migration::run(&mut connection)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch(include_str!("schema.sql"))?;
        migration::run(&mut connection)?;
        effect_index::migrate(&mut connection)?;
        connection.execute_batch("PRAGMA synchronous=NORMAL;")?;
        durability.verify(&connection)?;
        let connection = Arc::new(Mutex::new(connection));
        let recording = Arc::new(recording::Writer::new(connection.clone(), durability)?);
        Ok(Self {
            recording,
            connection,
        })
    }
    fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        self.recording.check()?;
        self.connection
            .lock()
            .map_err(|_| anyhow!("store lock poisoned"))
    }
    /// Trusted native extension point. Callbacks must preserve the durability
    /// configuration throughout their operation, including temporary changes.
    /// Effective settings are checked afterward, even when the callback fails;
    /// a weaken/write/restore sequence cannot be inferred from final settings.
    pub fn with_connection<T>(
        &self,
        operation: impl FnOnce(&mut Connection) -> Result<T>,
    ) -> Result<T> {
        let _publication = self.recording.publication()?;
        self.recording.barrier(false)?;
        let mut connection = self.lock()?;
        let result = operation(&mut connection);
        // The callback may alter effect projections, including before an error.
        self.recording.effects_changed();
        self.recording.verify_connection(&connection)?;
        result
    }
}
fn put(c: &Connection, kind: &str, bytes: &[u8]) -> Result<String> {
    ensure!(
        !matches!(
            kind,
            "event" | "result" | "state" | "message" | "tree" | "desc"
        ),
        "structured CAS kind requires put_value"
    );
    let hash = blake3::hash(bytes).to_hex().to_string();
    c.execute(
        "INSERT OR IGNORE INTO cas(hash,kind,bytes,created_at,codec) VALUES (?,?,?,unixepoch(),85)",
        params![hash, kind, bytes],
    )?;
    c.execute("INSERT OR IGNORE INTO cas_codecs VALUES (?,85)", [&hash])?;
    Ok(hash)
}
fn put_value<T: serde::Serialize>(c: &Connection, kind: &str, value: &T) -> Result<String> {
    let bytes = encode(value)?;
    let hash = blake3::hash(&bytes).to_hex().to_string();
    c.execute("INSERT OR IGNORE INTO cas(hash,kind,bytes,created_at,codec) VALUES (?,?,?,unixepoch(),113)", params![hash,kind,bytes])?;
    c.execute("INSERT OR IGNORE INTO cas_codecs VALUES (?,113)", [&hash])?;
    Ok(hash)
}
fn record_definition_event(c: &Connection, event: &Value) -> Result<i64> {
    let hash = put_value(c, "event", event)?;
    c.execute(
        "INSERT INTO definition_records(event_hash,ts) VALUES (?,unixepoch())",
        params![hash],
    )?;
    let seq = c.last_insert_rowid();
    if event["type"] == "effect_invoked" {
        record_observed_effect(c, event)?;
    }
    Ok(seq)
}
fn record_observed_effect(c: &Connection, event: &Value) -> Result<()> {
    // Trusted host operations are logged without a guest definition owner.
    if event["def_hash"].is_null() {
        return Ok(());
    }
    let hash = event["def_hash"]
        .as_str()
        .context("effect invocation missing definition hash")?;
    let op = event["op"]
        .as_str()
        .context("effect invocation missing operation")?;
    c.execute(
        "INSERT OR IGNORE INTO def_effects VALUES (?,?)",
        params![hash, op],
    )?;
    Ok(())
}
fn executable_definition(c: &Connection, hash: &str) -> Result<Option<Def>> {
    let value: Option<String> = c.query_row("SELECT json_object('hash',hash,'lang',lang,'component_hash',component_hash,'sig',json(type_sig),'allowed_effects',json(allowed_effects)) FROM defs WHERE hash=?", [hash], |row| row.get(0)).optional()?;
    value
        .map(|value| serde_json::from_str(&value).map_err(anyhow::Error::from))
        .transpose()
}
fn definition(c: &Connection, hash: &str) -> Result<Option<Def>> {
    let Some(mut def) = executable_definition(c, hash)? else {
        return Ok(None);
    };
    let mut q = c.prepare("SELECT op FROM def_effects WHERE def_hash=? ORDER BY op")?;
    def.observed_effects = q
        .query_map([hash], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Some(def))
}
fn encode<T: serde::Serialize>(value: &T) -> Result<Vec<u8>> {
    loom_proto::encode(value).map_err(anyhow::Error::msg)
}
fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    loom_proto::decode(bytes).map_err(anyhow::Error::msg)
}

pub use definitions::EntryReference;
