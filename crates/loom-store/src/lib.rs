mod cas_browser;
mod dag_migration;
mod effect_index;
mod migration;
mod recording;
use anyhow::{Context, Result, anyhow, ensure};
use loom_proto::{Actor, Def, Event, Snapshot, Value};
pub use recording::RecordingTimings;
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
};

#[derive(Debug, Default, serde::Serialize)]
pub struct Compaction {
    pub events: usize,
    pub archive_hash: Option<String>,
    pub before_bytes: u64,
    pub after_bytes: u64,
}

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
        connection.create_scalar_function(
            "loom_archive",
            1,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8
                | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
            |context| {
                let bytes: Vec<u8> = context.get(0)?;
                let decoded = zstd::stream::decode_all(bytes.as_slice())
                    .map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e)))?;
                decode::<Value>(&decoded)
                    .and_then(|value| Ok(serde_json::to_string(&value)?))
                    .map_err(|e| rusqlite::Error::UserFunctionError(e.into()))
            },
        )?;
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
        dag_migration::run(&mut connection)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch(include_str!("schema.sql"))?;
        migration::run(&mut connection)?;
        effect_index::rebuild(&mut connection)?;
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
    pub fn put(&self, kind: &str, bytes: &[u8]) -> Result<String> {
        self.recording.barrier(false)?;
        put(&*self.lock()?, kind, bytes)
    }
    pub fn get(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        self.recording.barrier(false)?;
        let address = if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            None
        } else {
            Some(loom_proto::parse_reference(hash).map_err(anyhow::Error::msg)?)
        };
        let hash = address.as_ref().map_or(hash, |a| a.hash.as_str());
        let c = self.lock()?;
        if let Some(address) = &address {
            let exists: bool = c.query_row(
                "SELECT EXISTS(SELECT 1 FROM cas WHERE hash=?)",
                [hash],
                |r| r.get(0),
            )?;
            let registered: bool = c.query_row(
                "SELECT EXISTS(SELECT 1 FROM cas_codecs WHERE hash=? AND codec=?)",
                params![hash, address.codec],
                |r| r.get(0),
            )?;
            ensure!(
                !exists || registered,
                "CID codec is not registered for stored object"
            );
        }
        if let Some(bytes) = c
            .query_row("SELECT bytes FROM cas WHERE hash=?", [hash], |r| r.get(0))
            .optional()?
        {
            return Ok(Some(bytes));
        }
        let archive:Option<Vec<u8>>=c.query_row("SELECT c.bytes FROM archive_entries e JOIN cas c ON c.hash=e.archive_hash WHERE e.event_hash=?",[hash],|r|r.get(0)).optional()?;
        let Some(archive) = archive else {
            return Ok(None);
        };
        ensure!(
            address.as_ref().is_none_or(|a| a.codec == 113),
            "archived event requires DAG-CBOR CID"
        );
        let decoded = zstd::stream::decode_all(archive.as_slice())?;
        let records: Vec<Event> = decode(&decoded)?;
        for record in records {
            let bytes = encode(&record.event)?;
            if blake3::hash(&bytes).to_hex().as_str() == hash {
                return Ok(Some(bytes));
            }
        }
        anyhow::bail!("archive index refers to missing event {hash}")
    }
    pub fn put_value<T: serde::Serialize>(&self, kind: &str, value: &T) -> Result<String> {
        self.recording.barrier(false)?;
        put_value(&*self.lock()?, kind, value)
    }
    pub fn get_value<T: serde::de::DeserializeOwned>(&self, hash: &str) -> Result<Option<T>> {
        // A typed lookup selects DAG-CBOR for internal hex identities, while an
        // explicit CID must retain its caller-selected codec.
        let dag = if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            Some(loom_proto::cid_for_hash(hash, 113).map_err(anyhow::Error::msg)?)
        } else {
            None
        };
        let hash = dag.as_deref().unwrap_or(hash);
        ensure!(
            self.codec(hash)?.is_none_or(|codec| codec == 113),
            "CAS object is raw bytes, not DAG-CBOR"
        );
        self.get(hash)?.map(|b| decode(&b)).transpose()
    }
    pub fn codec(&self, hash: &str) -> Result<Option<u64>> {
        self.recording.barrier(false)?;
        let address = if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            None
        } else {
            Some(loom_proto::parse_reference(hash).map_err(anyhow::Error::msg)?)
        };
        let hash = address.as_ref().map_or(hash, |a| a.hash.as_str());
        let c = self.lock()?;
        let codec: Option<u64> = c.query_row("SELECT codec FROM cas WHERE hash=? UNION ALL SELECT 113 FROM archive_entries WHERE event_hash=? LIMIT 1", params![hash,hash], |r| r.get(0)).optional()?;
        if let Some(address) = &address {
            if codec.is_none() {
                return Ok(None);
            }
            let registered: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM cas_codecs WHERE hash=? AND codec=? UNION ALL SELECT 1 FROM archive_entries WHERE event_hash=? AND ?=113)",params![hash,address.codec,hash,address.codec],|r|r.get(0))?;
            ensure!(registered, "CID codec is not registered for stored object");
            return Ok(Some(address.codec));
        }
        Ok(codec)
    }
    pub fn reference(&self, hash: &str, codec: u64) -> Result<Value> {
        let hash = if hash.len() == 64 {
            hash.to_owned()
        } else {
            loom_proto::parse_reference(hash)
                .map_err(anyhow::Error::msg)?
                .hash
        };
        let reference = loom_proto::reference(&hash, codec).map_err(anyhow::Error::msg)?;
        self.codec(
            reference["$ref"]
                .as_str()
                .context("invalid generated reference")?,
        )?
        .context("CAS object not found")?;
        Ok(reference)
    }
    pub fn latest_seq(&self) -> Result<i64> {
        self.recording.barrier(false)?;
        Ok(self
            .lock()?
            .query_row("SELECT coalesce(max(seq),0) FROM log", [], |r| r.get(0))?)
    }
    pub fn define(
        &self,
        def: &Def,
        name: Option<&str>,
        source: &str,
        deps: &BTreeMap<String, String>,
    ) -> Result<i64> {
        self.recording.barrier(false)?;
        let mut connection = self.lock()?;
        let tx = connection.transaction()?;
        let identity = loom_proto::definition_identity(
            def.lang,
            source,
            deps,
            def.allowed_effects.as_deref(),
        )?;
        ensure!(
            blake3::hash(&identity).to_hex().as_str() == def.hash,
            "definition hash does not match canonical identity"
        );
        put(&tx, "def", &identity)?;
        let mut def = def.clone();
        if let Some(labels) = def.allowed_effects.as_mut() {
            labels.sort();
            labels.dedup();
        }
        def.observed_effects.clear();
        let source_hash = put(&tx, "source_bundle", source.as_bytes())?;
        let event = serde_json::json!({"type":"defined","def":def,"name":name,"source_hash":source_hash,"deps":deps});
        let seq = append(&tx, "system", &event, 0)?;
        tx.execute("INSERT INTO defs(hash,lang,name_hint,type_sig,component_hash,source_hash,allowed_effects) VALUES (?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(hash) DO UPDATE SET component_hash=coalesce(excluded.component_hash,defs.component_hash)",params![def.hash,def.lang.as_str(),name,serde_json::to_string(&def.sig)?,def.component_hash,source_hash,def.allowed_effects.as_ref().map(serde_json::to_string).transpose()?])?;
        for hash in deps.values() {
            tx.execute(
                "INSERT OR IGNORE INTO def_deps VALUES (?,?)",
                params![def.hash, hash],
            )?;
        }
        if let Some(name) = name {
            tx.execute(
                "INSERT INTO names VALUES (?,?,?)",
                params![name, def.hash, seq],
            )?;
        }
        tx.commit()?;
        Ok(seq)
    }
    /// Execution metadata is published synchronously. Reading it does not drain
    /// effect recordings; observed_effects is intentionally excluded.
    pub fn executable_definition(&self, hash: &str) -> Result<Option<Def>> {
        executable_definition(&*self.lock()?, hash)
    }
    pub fn definition(&self, hash: &str) -> Result<Option<Def>> {
        self.recording.barrier(false)?;
        definition(&*self.lock()?, hash)
    }
    pub fn resolve(&self, name: &str) -> Result<Option<Def>> {
        if let Some(hash) = name.strip_prefix('#') {
            return self.definition(hash);
        }
        if let Some(def) = self.definition(name)? {
            return Ok(Some(def));
        }
        let connection = self.lock()?;
        let hash: Option<String> = connection
            .query_row(
                "SELECT hash FROM names WHERE name=? ORDER BY since_seq DESC LIMIT 1",
                [name],
                |r| r.get(0),
            )
            .optional()?;
        hash.map(|h| definition(&connection, &h))
            .transpose()
            .map(Option::flatten)
    }
    /// Reconstruct durable projections from the append-only log. Snapshots are disposable.
    pub fn rebuild_views(&self) -> Result<()> {
        let _publication = self.recording.publication()?;
        self.recording.barrier(false)?;
        let mut c = self.lock()?;
        let tx = c.transaction()?;
        let recorded: Vec<Event> = {
            let mut q =
                tx.prepare("SELECT seq,actor,bytes,handler_seq,ts FROM events ORDER BY seq")?;
            let mut rows = q.query([])?;
            let mut result = Vec::new();
            while let Some(r) = rows.next()? {
                result.push(Event {
                    seq: r.get(0)?,
                    actor: r.get(1)?,
                    event: serde_json::from_slice(&r.get::<_, Vec<u8>>(2)?)?,
                    handler_seq: r.get(3)?,
                    ts: r.get(4)?,
                });
            }
            result
        };
        let signature_replacements = migration::replacements(&recorded)?;
        tx.execute_batch("DELETE FROM def_effects; DELETE FROM message_keys; DELETE FROM inbox; DELETE FROM sessions; DELETE FROM snapshots; DELETE FROM names; DELETE FROM def_deps; DELETE FROM defs; DELETE FROM actors; DELETE FROM effect_results;")?;
        for record in recorded {
            let e = &record.event;
            if record.actor != "system" {
                tx.execute(
                    "UPDATE actors SET last_seq=? WHERE id=?",
                    params![record.seq, record.actor],
                )?;
                continue;
            }
            match e.get("type").and_then(Value::as_str) {
                Some("effect_invoked") => {
                    record_observed_effect(&tx, e)?;
                }
                Some("dag_cbor_migrated") => {
                    tx.execute_batch("UPDATE defs SET component_hash=NULL; UPDATE actors SET component_hash=NULL;")?;
                }
                Some("message_enqueued") => {
                    if let Some(key) = e["key"].as_str() {
                        let hash = put_value(&tx, "message", &e["msg"])?;
                        tx.execute(
                            "INSERT INTO message_keys VALUES (?,?,?,?)",
                            params![
                                key,
                                e["actor"].as_str().context("missing actor")?,
                                record.seq,
                                hash
                            ],
                        )?;
                    }
                    tx.execute(
                        "INSERT INTO inbox VALUES (?,?,?)",
                        params![
                            e["actor"].as_str().context("missing actor")?,
                            record.seq,
                            serde_json::to_string(&e["msg"])?
                        ],
                    )?;
                }
                Some("message_completed") => {
                    tx.execute(
                        "DELETE FROM inbox WHERE actor=? AND handler_seq=?",
                        params![
                            e["actor"].as_str().context("missing actor")?,
                            e["handler_seq"]
                                .as_i64()
                                .context("missing handler sequence")?
                        ],
                    )?;
                }
                Some("defined") => {
                    let mut definition = e["def"].clone();
                    let hash = definition["hash"]
                        .as_str()
                        .context("definition missing hash")?;
                    if let Some(sig) = signature_replacements.get(hash) {
                        definition["sig"] = sig.clone();
                    }
                    let def: Def = serde_json::from_value(definition)?;
                    tx.execute("INSERT INTO defs(hash,lang,name_hint,type_sig,component_hash,source_hash,allowed_effects) VALUES (?,?,?,?,?,?,?) ON CONFLICT(hash) DO UPDATE SET component_hash=coalesce(excluded.component_hash,defs.component_hash)",params![def.hash,def.lang.as_str(),e["name"].as_str(),serde_json::to_string(&def.sig)?,def.component_hash,e["source_hash"].as_str().context("missing source hash")?,def.allowed_effects.as_ref().map(serde_json::to_string).transpose()?])?;
                    let deps: BTreeMap<String, String> = serde_json::from_value(e["deps"].clone())?;
                    for hash in deps.values() {
                        tx.execute(
                            "INSERT OR IGNORE INTO def_deps VALUES (?,?)",
                            params![def.hash, hash],
                        )?;
                    }
                    if let Some(name) = e["name"].as_str() {
                        tx.execute(
                            "INSERT INTO names VALUES (?,?,?)",
                            params![name, def.hash, record.seq],
                        )?;
                    }
                }
                Some("actor_created") => {
                    let a: Actor = serde_json::from_value(e["actor"].clone())?;
                    tx.execute(
                        "INSERT INTO actors VALUES (?,?,?,?,?,?,?)",
                        params![
                            a.id,
                            a.behavior_hash,
                            a.lang.as_str(),
                            a.component_hash,
                            record.seq,
                            record.seq,
                            a.parent
                        ],
                    )?;
                }
                Some("actor_updated") => {
                    let a: Actor = serde_json::from_value(e["actor"].clone())?;
                    tx.execute("UPDATE actors SET behavior_hash=?,lang=?,component_hash=?,last_seq=? WHERE id=?",params![a.behavior_hash,a.lang.as_str(),a.component_hash,record.seq,a.id])?;
                }
                Some("effect_recorded") => {
                    tx.execute(
                        "INSERT OR IGNORE INTO effect_results VALUES (?,?,?,?)",
                        params![
                            e["desc_hash"].as_str().context("missing desc")?,
                            e["scope"].as_str().context("missing scope")?,
                            e["occurrence"].as_i64().context("missing occurrence")?,
                            e["result_hash"].as_str().context("missing result")?
                        ],
                    )?;
                }
                Some("session_created") => {
                    tx.execute(
                        "INSERT INTO sessions VALUES (?,?,?)",
                        params![
                            e["id"].as_str().context("missing id")?,
                            e["actor"].as_str().context("missing actor")?,
                            e["owner"].as_str().context("missing owner")?
                        ],
                    )?;
                }
                _ => {}
            }
        }
        tx.commit()?;
        self.recording.effects_changed();
        Ok(())
    }
    pub fn definition_name(&self, hash: &str) -> Result<Option<String>> {
        Ok(self
            .lock()?
            .query_row("SELECT name_hint FROM defs WHERE hash=?", [hash], |r| {
                r.get(0)
            })
            .optional()?
            .flatten())
    }
    pub fn enqueue(&self, actor: &str, msg: &Value) -> Result<PendingMessage> {
        self.enqueue_with_key(actor, msg, None)
    }
    pub fn enqueue_once(&self, actor: &str, msg: &Value, key: &str) -> Result<PendingMessage> {
        self.enqueue_with_key(actor, msg, Some(key))
    }
    fn enqueue_with_key(
        &self,
        actor: &str,
        msg: &Value,
        key: Option<&str>,
    ) -> Result<PendingMessage> {
        self.recording.barrier(false)?;
        let mut c = self.lock()?;
        let tx = c.transaction()?;
        if let Some(key) = key {
            let mut q=tx.prepare("SELECT k.actor,k.handler_seq,c.bytes FROM message_keys k JOIN cas c ON c.hash=k.msg_hash WHERE k.key=?")?;
            let mut rows = q.query([key])?;
            if let Some(r) = rows.next()? {
                let receipt = PendingMessage {
                    actor: r.get(0)?,
                    handler_seq: r.get(1)?,
                    msg: decode(&r.get::<_, Vec<u8>>(2)?)?,
                };
                ensure!(
                    receipt.actor == actor && receipt.msg == *msg,
                    "message idempotency key conflicts with original request"
                );
                return Ok(receipt);
            }
        }
        ensure!(
            tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM actors WHERE id=?)",
                [actor],
                |r| r.get::<_, bool>(0)
            )?,
            "unknown actor"
        );
        let seq = append(
            &tx,
            "system",
            &serde_json::json!({"type":"message_enqueued","actor":actor,"msg":msg,"key":key}),
            0,
        )?;
        tx.execute(
            "INSERT INTO inbox VALUES (?,?,?)",
            params![actor, seq, serde_json::to_string(msg)?],
        )?;
        if let Some(key) = key {
            let hash = put_value(&tx, "message", msg)?;
            tx.execute(
                "INSERT INTO message_keys VALUES (?,?,?,?)",
                params![key, actor, seq, hash],
            )?;
        }
        tx.commit()?;
        Ok(PendingMessage {
            actor: actor.into(),
            handler_seq: seq,
            msg: msg.clone(),
        })
    }
    pub fn pending(&self, actor: &str) -> Result<Option<PendingMessage>> {
        pending(&*self.lock()?, actor)
    }
    pub fn pending_messages(&self) -> Result<Vec<PendingMessage>> {
        let c = self.lock()?;
        let mut q = c.prepare("SELECT actor,handler_seq,msg FROM inbox ORDER BY handler_seq")?;
        let mut rows = q.query([])?;
        let mut messages = Vec::new();
        while let Some(r) = rows.next()? {
            messages.push(PendingMessage {
                actor: r.get(0)?,
                handler_seq: r.get(1)?,
                msg: serde_json::from_str(&r.get::<_, String>(2)?)?,
            });
        }
        Ok(messages)
    }
    pub fn message_pending(&self, actor: &str, handler_seq: i64) -> Result<bool> {
        Ok(self.lock()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM inbox WHERE actor=? AND handler_seq=?)",
            params![actor, handler_seq],
            |r| r.get(0),
        )?)
    }
    pub fn complete_message(&self, actor: &str, handler_seq: i64, events: &[Value]) -> Result<i64> {
        self.recording.barrier(false)?;
        let mut c = self.lock()?;
        let tx = c.transaction()?;
        let message = pending(&tx, actor)?.context("no pending message")?;
        ensure!(
            message.handler_seq == handler_seq,
            "pending handler sequence mismatch"
        );
        let mut seq: i64 =
            tx.query_row("SELECT last_seq FROM actors WHERE id=?", [actor], |r| {
                r.get(0)
            })?;
        for event in events {
            seq = append(&tx, actor, event, handler_seq)?;
        }
        tx.execute(
            "UPDATE actors SET last_seq=? WHERE id=?",
            params![seq, actor],
        )?;
        let completed = append(
            &tx,
            "system",
            &serde_json::json!({"type":"message_completed","actor":actor,"handler_seq":handler_seq}),
            handler_seq,
        )?;
        tx.execute(
            "DELETE FROM inbox WHERE actor=? AND handler_seq=?",
            params![actor, handler_seq],
        )?;
        tx.commit()?;
        Ok(completed)
    }
    pub fn compact_log(&self, through_seq: i64, limit: usize) -> Result<Compaction> {
        self.recording.barrier(false)?;
        ensure!(
            limit > 0 && limit <= 100_000,
            "compaction limit must be 1..=100000"
        );
        let mut c = self.lock()?;
        let tx = c.transaction()?;
        let mut records = Vec::new();
        let mut hashes = Vec::new();
        {
            let mut q=tx.prepare("SELECT l.seq,l.actor,loom_json(c.bytes),l.handler_seq,l.ts,l.event_hash FROM log l JOIN cas c ON c.hash=l.event_hash WHERE l.seq<=? AND NOT EXISTS(SELECT 1 FROM archive_segments a WHERE l.seq BETWEEN a.first_seq AND a.last_seq) ORDER BY l.seq LIMIT ?")?;
            let mut rows = q.query(params![through_seq, limit as i64])?;
            while let Some(r) = rows.next()? {
                records.push(Event {
                    seq: r.get(0)?,
                    actor: r.get(1)?,
                    event: serde_json::from_slice(&r.get::<_, Vec<u8>>(2)?)?,
                    handler_seq: r.get(3)?,
                    ts: r.get(4)?,
                });
                hashes.push(r.get::<_, String>(5)?);
            }
        }
        let Some(first) = records.first() else {
            return Ok(Compaction::default());
        };
        let first_seq = first.seq;
        let last_seq = records.last().context("missing last event")?.seq;
        let encoded = encode(&records)?;
        let compressed = zstd::stream::encode_all(encoded.as_slice(), 3)?;
        let decoded = zstd::stream::decode_all(compressed.as_slice())?;
        ensure!(decoded == encoded, "archive verification failed");
        let verified: Vec<Event> = decode(&decoded)?;
        ensure!(verified.len() == records.len(), "archive count mismatch");
        let before_bytes: i64 =
            tx.query_row("SELECT coalesce(sum(length(bytes)),0) FROM cas", [], |r| {
                r.get(0)
            })?;
        let hash = put(&tx, "event_archive", &compressed)?;
        tx.execute(
            "INSERT INTO archive_segments VALUES (?,?,?,?)",
            params![hash, first_seq, last_seq, records.len() as i64],
        )?;
        for (index, record) in records.iter().enumerate() {
            tx.execute(
                "INSERT OR IGNORE INTO archive_entries VALUES (?,?)",
                params![hashes[index], hash],
            )?;
            tx.execute(
                "UPDATE log SET actor='',event_hash=?,handler_seq=0,ts=0 WHERE seq=?",
                params![hash, record.seq],
            )?;
        }
        for old in hashes {
            tx.execute("DELETE FROM cas WHERE kind='event' AND hash=? AND NOT EXISTS(SELECT 1 FROM cas_codecs WHERE cas_codecs.hash=cas.hash AND codec=85) AND NOT EXISTS(SELECT 1 FROM log WHERE event_hash=cas.hash) AND NOT EXISTS(SELECT 1 FROM defs WHERE source_hash=cas.hash OR component_hash=cas.hash) AND NOT EXISTS(SELECT 1 FROM snapshots WHERE state_hash=cas.hash) AND NOT EXISTS(SELECT 1 FROM effect_results WHERE result_hash=cas.hash) AND NOT EXISTS(SELECT 1 FROM message_keys WHERE msg_hash=cas.hash) AND NOT EXISTS(SELECT 1 FROM archive_segments WHERE hash=cas.hash)",[old])?;
        }
        let after_bytes: i64 =
            tx.query_row("SELECT coalesce(sum(length(bytes)),0) FROM cas", [], |r| {
                r.get(0)
            })?;
        tx.commit()?;
        Ok(Compaction {
            events: records.len(),
            archive_hash: Some(hash),
            before_bytes: before_bytes as u64,
            after_bytes: after_bytes as u64,
        })
    }
    pub fn definitions(&self) -> Result<Vec<Def>> {
        self.recording.barrier(false)?;
        let c = self.lock()?;
        let mut q = c.prepare("SELECT hash FROM defs ORDER BY hash")?;
        let hashes: Vec<String> = q
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        hashes
            .iter()
            .map(|h| definition(&c, h)?.context("definition disappeared"))
            .collect()
    }
    pub fn session(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .lock()?
            .query_row("SELECT actor FROM sessions WHERE id=?", [id], |r| r.get(0))
            .optional()?)
    }
    pub fn create_session(&self, id: &str, actor: &str, owner: &str) -> Result<()> {
        self.recording.barrier(false)?;
        let mut c = self.lock()?;
        let tx = c.transaction()?;
        append(
            &tx,
            "system",
            &serde_json::json!({"type":"session_created","id":id,"actor":actor,"owner":owner}),
            0,
        )?;
        tx.execute(
            "INSERT INTO sessions VALUES (?,?,?)",
            params![id, actor, owner],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn source(&self, hash: &str) -> Result<Option<String>> {
        Ok(self.lock()?.query_row("SELECT CAST(c.bytes AS TEXT) FROM defs d JOIN cas c ON c.hash=d.source_hash WHERE d.hash=?",[hash],|r|r.get(0)).optional()?)
    }
    pub fn dependencies(&self, hash: &str) -> Result<Vec<String>> {
        let connection = self.lock()?;
        let mut q = connection
            .prepare("SELECT dep_hash FROM def_deps WHERE def_hash=? ORDER BY dep_hash")?;
        Ok(q.query_map([hash], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }
    pub fn dependents(&self, hash: &str) -> Result<Vec<String>> {
        let c = self.lock()?;
        let mut q =
            c.prepare("SELECT def_hash FROM def_deps WHERE dep_hash=? ORDER BY def_hash")?;
        Ok(q.query_map([hash], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }
    pub fn definition_deps(&self, hash: &str) -> Result<BTreeMap<String, String>> {
        let c = self.lock()?;
        let bytes:Vec<u8>=c.query_row("SELECT bytes FROM events WHERE actor='system' AND json_extract(bytes,'$.type')='defined' AND json_extract(bytes,'$.def.hash')=? ORDER BY seq DESC LIMIT 1",[hash],|r|r.get(0))?;
        let event: Value = serde_json::from_slice(&bytes)?;
        Ok(serde_json::from_value(event["deps"].clone())?)
    }
    pub fn name_history(&self, name: &str) -> Result<Vec<loom_proto::NameRevision>> {
        let c = self.lock()?;
        let mut q =
            c.prepare("SELECT name,hash,since_seq FROM names WHERE name=? ORDER BY since_seq")?;
        Ok(q.query_map([name], |r| {
            Ok(loom_proto::NameRevision {
                name: r.get(0)?,
                hash: r.get(1)?,
                since_seq: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
    }
    pub fn who_runs(&self, behavior: &str) -> Result<Vec<Actor>> {
        Ok(self
            .actors()?
            .into_iter()
            .filter(|a| a.behavior_hash == behavior)
            .collect())
    }
    pub fn create_initialized_actor(&self, actor: &Actor, initial: &Value) -> Result<Actor> {
        self.recording.barrier(false)?;
        let mut c = self.lock()?;
        let tx = c.transaction()?;
        let created_seq = append(
            &tx,
            "system",
            &serde_json::json!({"type":"actor_created","actor":actor}),
            0,
        )?;
        tx.execute(
            "INSERT INTO actors VALUES (?,?,?,?,?,?,?)",
            params![
                actor.id,
                actor.behavior_hash,
                actor.lang.as_str(),
                actor.component_hash,
                created_seq,
                created_seq,
                actor.parent
            ],
        )?;
        let last_seq = append(
            &tx,
            &actor.id,
            &serde_json::json!({"__loom_init":initial}),
            0,
        )?;
        tx.execute(
            "UPDATE actors SET last_seq=? WHERE id=?",
            params![last_seq, actor.id],
        )?;
        let state_hash = put_value(&tx, "state", initial)?;
        tx.execute(
            "INSERT INTO snapshots VALUES (?,?,?,?)",
            params![actor.id, actor.behavior_hash, last_seq, state_hash],
        )?;
        tx.commit()?;
        Ok(Actor {
            last_seq,
            created_seq,
            ..actor.clone()
        })
    }
    pub fn create_actor(&self, actor: &Actor) -> Result<i64> {
        self.recording.barrier(false)?;
        let mut connection = self.lock()?;
        let tx = connection.transaction()?;
        let seq = append(
            &tx,
            "system",
            &serde_json::json!({"type":"actor_created","actor":actor}),
            0,
        )?;
        tx.execute(
            "INSERT INTO actors VALUES (?,?,?,?,?,?,?)",
            params![
                actor.id,
                actor.behavior_hash,
                actor.lang.as_str(),
                actor.component_hash,
                seq,
                seq,
                actor.parent
            ],
        )?;
        tx.commit()?;
        Ok(seq)
    }
    pub fn actor(&self, id: &str) -> Result<Option<Actor>> {
        let connection = self.lock()?;
        let bytes:Option<String>=connection.query_row("SELECT json_object('id',id,'behavior_hash',behavior_hash,'lang',lang,'component_hash',component_hash,'last_seq',last_seq,'created_seq',created_seq,'parent',parent) FROM actors WHERE id=?",[id],|r|r.get(0)).optional()?;
        bytes
            .map(|b| serde_json::from_str(&b).map_err(Into::into))
            .transpose()
    }
    pub fn actors(&self) -> Result<Vec<Actor>> {
        let ids: Vec<String> = {
            let c = self.lock()?;
            let mut q = c.prepare("SELECT id FROM actors ORDER BY created_seq")?;
            q.query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        ids.iter()
            .map(|id| self.actor(id)?.context("actor disappeared"))
            .collect()
    }
    pub fn append(&self, actor: &str, event: &Value, handler_seq: i64) -> Result<i64> {
        self.recording.barrier(false)?;
        let mut connection = self.lock()?;
        let tx = connection.transaction()?;
        ensure!(
            actor == "system"
                || tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM actors WHERE id=?)",
                    [actor],
                    |r| r.get::<_, bool>(0)
                )?,
            "unknown actor: {actor}"
        );
        let seq = append(&tx, actor, event, handler_seq)?;
        tx.execute(
            "UPDATE actors SET last_seq=? WHERE id=?",
            params![seq, actor],
        )?;
        tx.commit()?;
        Ok(seq)
    }
    pub fn update_actor(&self, actor: &Actor) -> Result<i64> {
        self.recording.barrier(false)?;
        let mut c = self.lock()?;
        let tx = c.transaction()?;
        let seq = append(
            &tx,
            "system",
            &serde_json::json!({"type":"actor_updated","actor":actor}),
            0,
        )?;
        let changed = tx.execute(
            "UPDATE actors SET behavior_hash=?,lang=?,component_hash=?,last_seq=? WHERE id=?",
            params![
                actor.behavior_hash,
                actor.lang.as_str(),
                actor.component_hash,
                seq,
                actor.id
            ],
        )?;
        ensure!(changed == 1, "unknown actor: {}", actor.id);
        tx.commit()?;
        Ok(seq)
    }
    pub fn append_batch(&self, actor: &str, events: &[Value], handler_seq: i64) -> Result<i64> {
        self.recording.barrier(false)?;
        let mut c = self.lock()?;
        let tx = c.transaction()?;
        let mut seq: i64 = tx
            .query_row("SELECT last_seq FROM actors WHERE id=?", [actor], |r| {
                r.get(0)
            })
            .optional()?
            .context("unknown actor")?;
        for event in events {
            seq = append(&tx, actor, event, handler_seq)?;
        }
        tx.execute(
            "UPDATE actors SET last_seq=? WHERE id=?",
            params![seq, actor],
        )?;
        tx.commit()?;
        Ok(seq)
    }
    pub fn events(&self, actor: Option<&str>, after: i64, limit: usize) -> Result<Vec<Event>> {
        self.recording.barrier(false)?;
        let connection = self.lock()?;
        let mut q=connection.prepare("SELECT seq,actor,bytes,handler_seq,ts FROM events WHERE seq>? AND (? IS NULL OR actor=?) ORDER BY seq LIMIT ?")?;
        let mut rows = q.query(params![after, actor, actor, limit.min(1000) as i64])?;
        let mut events = Vec::new();
        while let Some(r) = rows.next()? {
            events.push(Event {
                seq: r.get(0)?,
                actor: r.get(1)?,
                event: serde_json::from_slice(&r.get::<_, Vec<u8>>(2)?)?,
                handler_seq: r.get(3)?,
                ts: r.get(4)?,
            });
        }
        Ok(events)
    }
    pub fn snapshot(&self, actor: &str, fold_hash: &str, seq: i64, state: &Value) -> Result<()> {
        self.recording.barrier(false)?;
        let mut c = self.lock()?;
        let tx = c.transaction()?;
        let hash = put_value(&tx, "state", state)?;
        tx.execute(
            "INSERT OR REPLACE INTO snapshots VALUES (?,?,?,?)",
            params![actor, fold_hash, seq, hash],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn latest_snapshot(&self, actor: &str, fold_hash: &str) -> Result<Option<Snapshot>> {
        let c = self.lock()?;
        let mut q=c.prepare("SELECT s.seq,c.bytes FROM snapshots s JOIN cas c ON c.hash=s.state_hash WHERE s.actor=? AND s.fold_hash=? ORDER BY s.seq DESC LIMIT 1")?;
        let mut rows = q.query(params![actor, fold_hash])?;
        match rows.next()? {
            Some(r) => Ok(Some(Snapshot {
                seq: r.get(0)?,
                state: decode(&r.get::<_, Vec<u8>>(1)?)?,
            })),
            None => Ok(None),
        }
    }
    /// Drain recording and synchronize the WAL and database before an external reply.
    pub fn flush(&self) -> Result<()> {
        self.recording.barrier(true)
    }
    pub fn recording_timings(&self) -> RecordingTimings {
        self.recording.timings()
    }
    pub fn recording_commit_count(&self) -> u64 {
        self.recording.commits()
    }
    pub fn enqueue_recording(&self, event: &Value) -> Result<()> {
        self.recording.event(event)
    }
    pub fn enqueue_value<T: serde::Serialize>(&self, kind: &str, value: &T) -> Result<String> {
        self.recording.value(kind, value)
    }
    pub fn enqueue_effect(
        &self,
        desc_hash: &str,
        scope: &str,
        occurrence: i64,
        result: &Value,
    ) -> Result<String> {
        self.recording.effect(
            &self.connection,
            recording::EffectKey {
                desc_hash: desc_hash.into(),
                scope: scope.into(),
                occurrence,
            },
            result,
        )
    }
    pub fn effect_get(
        &self,
        desc_hash: &str,
        scope: &str,
        occurrence: i64,
    ) -> Result<Option<Value>> {
        let key = recording::EffectKey {
            desc_hash: desc_hash.into(),
            scope: scope.into(),
            occurrence,
        };
        if let Some(value) = self.recording.pending(&key)? {
            return Ok(Some(value));
        }
        let c = self.lock()?;
        let bytes: Option<Vec<u8>> = c.query_row(
            "SELECT c.bytes FROM effect_results e JOIN cas c ON c.hash=e.result_hash WHERE e.desc_hash=? AND e.scope=? AND e.occurrence=?",
            params![desc_hash, scope, occurrence], |row| row.get(0),
        ).optional()?;
        bytes.map(|b| decode(&b)).transpose()
    }
    pub fn effect_put(
        &self,
        desc_hash: &str,
        scope: &str,
        occurrence: i64,
        result: &Value,
    ) -> Result<()> {
        let _publication = self.recording.publication()?;
        self.recording.barrier(false)?;
        let mut c = self.lock()?;
        let tx = c.transaction()?;
        let hash = put_value(&tx, "result", result)?;
        let existing: Option<String> = tx.query_row(
            "SELECT result_hash FROM effect_results WHERE desc_hash=? AND scope=? AND occurrence=?",
            params![desc_hash, scope, occurrence], |row| row.get(0),
        ).optional()?;
        ensure!(
            existing.as_ref().is_none_or(|h| h == &hash),
            "effect cache result conflict"
        );
        append(
            &tx,
            "system",
            &serde_json::json!({"type":"effect_recorded","desc_hash":desc_hash,"scope":scope,"occurrence":occurrence,"result_hash":hash}),
            0,
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO effect_results VALUES (?,?,?,?)",
            params![desc_hash, scope, occurrence, hash],
        )?;
        tx.commit()?;
        self.recording.effects_changed();
        Ok(())
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
fn append(c: &Connection, actor: &str, event: &Value, handler_seq: i64) -> Result<i64> {
    let hash = put_value(c, "event", event)?;
    c.execute(
        "INSERT INTO log(actor,event_hash,handler_seq,ts) VALUES (?,?,?,unixepoch())",
        params![actor, hash, handler_seq],
    )?;
    let seq = c.last_insert_rowid();
    if actor == "system" && event["type"] == "effect_invoked" {
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

#[cfg(test)]
mod tests {
    use super::*;
    use loom_proto::Lang;
    use serde_json::json;
    fn identity(lang: Lang, source: &str, deps: &BTreeMap<String, String>) -> String {
        blake3::hash(&loom_proto::definition_identity(lang, source, deps, None).unwrap())
            .to_hex()
            .to_string()
    }
    fn actor() -> Actor {
        Actor {
            id: "a".into(),
            behavior_hash: "b".into(),
            lang: Lang::Rust,
            component_hash: None,
            last_seq: 0,
            created_seq: 0,
            parent: None,
        }
    }
    #[test]
    fn restart_preserves_events_names_snapshots_and_effects() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("loom.sqlite");
        let final_seq;
        {
            let store = Store::open(&path)?;
            let hash = store.put("blob", b"same")?;
            assert_eq!(hash, store.put("blob", b"same")?);
            let def = Def {
                hash: identity(Lang::Rust, "source", &BTreeMap::new()),
                lang: Lang::Rust,
                component_hash: None,
                sig: Default::default(),
                allowed_effects: None,
                observed_effects: Vec::new(),
            };
            store.define(&def, Some("counter"), "source", &BTreeMap::new())?;
            assert_eq!(
                store.get(&def.hash)?,
                Some(loom_proto::definition_identity(
                    def.lang,
                    "source",
                    &BTreeMap::new(),
                    None
                )?)
            );
            let mut invalid = def.clone();
            invalid.hash = "incorrect".into();
            assert!(
                store
                    .define(&invalid, None, "source", &BTreeMap::new())
                    .is_err()
            );
            store.create_actor(&actor())?;
            final_seq = store.append_batch("a", &[json!(1), json!(2)], 7)?;
            store.snapshot("a", "fold1", final_seq, &json!(3))?;
            store.effect_put("effect", "global", 0, &json!(42))?;
            store.create_session("s", "a", "owner")?;
        }
        let store = Store::open(path)?;
        assert_eq!(
            store.resolve("counter")?.unwrap().hash,
            identity(Lang::Rust, "source", &BTreeMap::new())
        );
        assert_eq!(
            store
                .source(&identity(Lang::Rust, "source", &BTreeMap::new()))?
                .as_deref(),
            Some("source")
        );
        assert_eq!(store.events(Some("a"), 0, 100)?.len(), 2);
        assert_eq!(store.actor("a")?.unwrap().last_seq, final_seq);
        assert_eq!(
            store.latest_snapshot("a", "fold1")?.unwrap().state,
            json!(3)
        );
        assert!(store.latest_snapshot("a", "fold2")?.is_none());
        assert_eq!(store.effect_get("effect", "global", 0)?, Some(json!(42)));
        assert!(store.effect_get("effect", "global", 1)?.is_none());
        assert_eq!(store.session("s")?.as_deref(), Some("a"));
        let seq = store.latest_seq()?;
        store.rebuild_views()?;
        assert_eq!(store.latest_seq()?, seq);
        assert_eq!(
            store.resolve("counter")?.unwrap().hash,
            identity(Lang::Rust, "source", &BTreeMap::new())
        );
        assert_eq!(store.actor("a")?.unwrap().last_seq, final_seq);
        assert_eq!(store.effect_get("effect", "global", 0)?, Some(json!(42)));
        assert_eq!(store.session("s")?.as_deref(), Some("a"));
        Ok(())
    }
    #[test]
    fn failed_transactions_leave_no_events_or_cas_results() -> Result<()> {
        let store = Store::memory()?;
        store.create_actor(&actor())?;
        let seq = store.latest_seq()?;
        assert!(store.create_actor(&actor()).is_err());
        assert_eq!(store.latest_seq()?, seq);
        assert!(store.append_batch("missing", &[json!(1)], 0).is_err());
        assert_eq!(store.latest_seq()?, seq);
        store.effect_put("e", "s", 0, &json!(1))?;
        let seq = store.latest_seq()?;
        assert!(store.effect_put("e", "s", 0, &json!(2)).is_err());
        assert_eq!(store.latest_seq()?, seq);
        assert_eq!(store.effect_get("e", "s", 0)?, Some(json!(1)));
        Ok(())
    }
    #[test]
    fn initialized_actor_is_atomic_and_recoverable() -> Result<()> {
        let store = Store::memory()?;
        let created = store.create_initialized_actor(&actor(), &json!({"count":3}))?;
        assert_eq!(store.events(Some("a"), 0, 100)?.len(), 1);
        assert_eq!(
            store.latest_snapshot("a", "b")?.unwrap().state,
            json!({"count":3})
        );
        let seq = store.latest_seq()?;
        assert!(
            store
                .create_initialized_actor(&actor(), &json!("wrong"))
                .is_err()
        );
        assert_eq!(store.latest_seq()?, seq);
        store.rebuild_views()?;
        assert_eq!(store.actor("a")?.unwrap().created_seq, created.created_seq);
        assert_eq!(store.actor("a")?.unwrap().last_seq, created.last_seq);
        assert_eq!(
            store.events(Some("a"), 0, 100)?[0].event,
            json!({"__loom_init":{"count":3}})
        );
        Ok(())
    }
    #[test]
    fn mailbox_migrates_and_delivers_duplicate_messages_in_order() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("mailbox.sqlite");
        let store = Store::open(&path)?;
        store.create_actor(&actor())?;
        let first = store.enqueue("a", &json!(1))?;
        store.with_connection(|c| {c.execute_batch("CREATE TABLE inbox_old(actor TEXT PRIMARY KEY REFERENCES actors(id),handler_seq INTEGER NOT NULL REFERENCES log(seq),msg TEXT NOT NULL); INSERT INTO inbox_old SELECT * FROM inbox; DROP TABLE inbox; ALTER TABLE inbox_old RENAME TO inbox;")?;Ok(())})?;
        drop(store);
        let store = Store::open(path)?;
        let second = store.enqueue("a", &json!(1))?;
        assert!(second.handler_seq > first.handler_seq);
        assert_eq!(store.pending_messages()?.len(), 2);
        assert!(
            store
                .complete_message("a", second.handler_seq, &[json!("bad")])
                .is_err()
        );
        store.rebuild_views()?;
        assert_eq!(store.pending("a")?.unwrap().handler_seq, first.handler_seq);
        store.complete_message("a", first.handler_seq, &[json!(1)])?;
        assert!(!store.message_pending("a", first.handler_seq)?);
        assert!(store.message_pending("a", second.handler_seq)?);
        store.rebuild_views()?;
        assert_eq!(store.pending("a")?.unwrap().handler_seq, second.handler_seq);
        store.complete_message("a", second.handler_seq, &[json!(2)])?;
        assert!(store.pending_messages()?.is_empty());
        let receipt = store.enqueue_once("a", &json!(9), "sender:effect:0")?;
        assert_eq!(
            store
                .enqueue_once("a", &json!(9), "sender:effect:0")?
                .handler_seq,
            receipt.handler_seq
        );
        store.complete_message("a", receipt.handler_seq, &[])?;
        store.compact_log(store.latest_seq()?, 1000)?;
        store.rebuild_views()?;
        assert_eq!(
            store
                .enqueue_once("a", &json!(9), "sender:effect:0")?
                .handler_seq,
            receipt.handler_seq
        );
        assert!(!store.message_pending("a", receipt.handler_seq)?);
        assert!(
            store
                .enqueue_once("a", &json!(10), "sender:effect:0")
                .is_err()
        );
        Ok(())
    }
    #[test]
    fn legacy_signatures_migrate_without_changing_historical_events() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("legacy.sqlite");
        let store = Store::open(&path)?;
        let legacy = serde_json::json!({"exports":[{"name":"main","params":[{"name":"left","type":"number"},{"name":"items","type":"unknown[]"}],"returns":"number"}]});
        let source = store.put("source_bundle", b"source")?;
        let hash = identity(Lang::Ts, "source", &BTreeMap::new());
        let event = json!({"type":"defined","def":{"hash":hash,"lang":"ts","component_hash":null,"sig":legacy},"name":"legacy","source_hash":source,"deps":{}});
        let seq = store.append("system", &event, 0)?;
        store.with_connection(|c| {
            c.execute(
                "INSERT INTO defs(hash,lang,name_hint,type_sig,component_hash,source_hash) VALUES (?,'ts','legacy',?,NULL,?)",
                params![hash, serde_json::to_string(&legacy)?, source],
            )?;
            c.execute(
                "INSERT INTO names VALUES ('legacy',?,?)",
                params![hash, seq],
            )?;
            Ok(())
        })?;
        store.create_actor(&actor())?;
        store.append("a", &json!(3), 0)?;
        store.create_session("session", "a", "owner")?;
        let old_event_hash = blake3::hash(&encode(&event)?).to_hex().to_string();
        drop(store);
        let store = Store::open(&path)?;
        assert_eq!(
            store.definition(&hash)?.unwrap().sig.exports[0].returns,
            loom_proto::ValueShape::Number
        );
        assert_eq!(store.get_value::<Value>(&old_event_hash)?, Some(event));
        assert_eq!(
            store.get(&hash)?,
            Some(loom_proto::definition_identity(
                Lang::Ts,
                "source",
                &BTreeMap::new(),
                None
            )?)
        );
        let seq = store.latest_seq()?;
        store.rebuild_views()?;
        assert_eq!(
            store.resolve("legacy")?.unwrap().sig.exports[0].params[0].shape,
            loom_proto::ValueShape::Number
        );
        assert_eq!(store.session("session")?.as_deref(), Some("a"));
        assert_eq!(
            store.events(Some("a"), 0, 100)?.last().unwrap().event,
            json!(3)
        );
        drop(store);
        let store = Store::open(path)?;
        assert_eq!(store.latest_seq()?, seq);
        Ok(())
    }
    #[test]
    fn archive_compaction_preserves_replay_and_pending_delivery() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("compact.sqlite");
        let store = Store::open(&path)?;
        store.define(
            &Def {
                hash: identity(Lang::Ts, "source", &BTreeMap::new()),
                lang: Lang::Ts,
                component_hash: None,
                sig: Default::default(),
                allowed_effects: None,
                observed_effects: Vec::new(),
            },
            Some("d"),
            "source",
            &BTreeMap::new(),
        )?;
        store.create_actor(&actor())?;
        for n in 0..100 {
            store.append(
                "a",
                &json!({"n":n,"data":"repeat this text to compress"}),
                0,
            )?;
        }
        store.effect_put("desc", "global", 0, &json!(123))?;
        let message = store.enqueue("a", &json!({"msg":1}))?;
        let before = serde_json::to_value(store.events(None, 0, 1000)?)?;
        let event_bytes = encode(&json!({"n":0,"data":"repeat this text to compress"}))?;
        let event_hash = blake3::hash(&event_bytes).to_hex().to_string();
        let first = store.compact_log(50, 1000)?;
        assert!(first.events > 0);
        let second = store.compact_log(store.latest_seq()?, 1000)?;
        assert!(second.events > 0);
        assert!(second.after_bytes < second.before_bytes);
        assert_eq!(serde_json::to_value(store.events(None, 0, 1000)?)?, before);
        assert_eq!(store.compact_log(store.latest_seq()?, 1000)?.events, 0);
        assert_eq!(store.get(&event_hash)?, Some(event_bytes));
        drop(store);
        let store = Store::open(&path)?;
        assert_eq!(serde_json::to_value(store.events(None, 0, 1000)?)?, before);
        store.rebuild_views()?;
        assert_eq!(
            store.resolve("d")?.unwrap().hash,
            identity(Lang::Ts, "source", &BTreeMap::new())
        );
        assert_eq!(
            store.pending("a")?.unwrap().handler_seq,
            message.handler_seq
        );
        store.with_connection(|c| {
            c.execute("DELETE FROM effect_results", [])?;
            Ok(())
        })?;
        drop(store);
        let store = Store::open(path)?;
        assert_eq!(store.effect_get("desc", "global", 0)?, Some(json!(123)));
        assert!(store.effect_put("desc", "global", 0, &json!(124)).is_err());
        store.complete_message("a", message.handler_seq, &[json!("done")])?;
        assert!(store.pending("a")?.is_none());
        store.rebuild_views()?;
        assert!(store.pending("a")?.is_none());
        assert_eq!(store.events(Some("a"), 0, 1000)?.len(), 101);
        Ok(())
    }
    #[test]
    fn name_history_keeps_old_definition_and_dependencies() -> Result<()> {
        let store = Store::memory()?;
        let deps = BTreeMap::from_iter([("dep".into(), "target".into())]);
        let old = identity(Lang::Ts, "old", &deps);
        let new = identity(Lang::Ts, "new", &deps);
        for source in ["old", "new"] {
            let hash = identity(Lang::Ts, source, &deps);
            store.define(
                &Def {
                    hash,
                    lang: Lang::Ts,
                    component_hash: None,
                    sig: Default::default(),
                    allowed_effects: None,
                    observed_effects: Vec::new(),
                },
                Some("name"),
                source,
                &deps,
            )?;
        }
        assert_eq!(store.resolve("name")?.unwrap().hash, new);
        assert!(store.definition(&old)?.is_some());
        assert_eq!(store.dependencies(&new)?, vec!["target"]);
        let mut expected = vec![old, new.clone()];
        expected.sort();
        assert_eq!(store.dependents("target")?, expected);
        assert_eq!(store.definition_deps(&new)?["dep"], "target");
        assert_eq!(store.name_history("name")?.len(), 2);
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PendingMessage {
    pub actor: String,
    pub handler_seq: i64,
    pub msg: Value,
}
fn pending(c: &Connection, actor: &str) -> Result<Option<PendingMessage>> {
    let mut q =
        c.prepare("SELECT handler_seq,msg FROM inbox WHERE actor=? ORDER BY handler_seq LIMIT 1")?;
    let mut rows = q.query([actor])?;
    match rows.next()? {
        Some(r) => Ok(Some(PendingMessage {
            actor: actor.into(),
            handler_seq: r.get(0)?,
            msg: serde_json::from_str(&r.get::<_, String>(1)?)?,
        })),
        None => Ok(None),
    }
}

fn encode<T: serde::Serialize>(value: &T) -> Result<Vec<u8>> {
    loom_proto::encode(value).map_err(anyhow::Error::msg)
}
fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    loom_proto::decode(bytes).map_err(anyhow::Error::msg)
}
