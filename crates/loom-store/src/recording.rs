use super::{append, encode};
use anyhow::{Result, anyhow, ensure};
use loom_proto::Value;
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

/// Cumulative completed recording stages; sample after flush for a reply boundary.
/// Transaction time includes BEGIN, record insertion, encoding, and COMMIT,
/// including any SQLite automatic checkpoint inside COMMIT. It excludes waiting
/// to acquire the connection. Explicit checkpoint time includes failed attempts.
#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct RecordingTimings {
    pub committed_transactions: u64,
    pub transaction_nanos: u64,
    pub checkpoint_attempts: u64,
    pub checkpoint_nanos: u64,
    /// Encoded trace submission bytes in committed batches, including idempotent repeats.
    pub trace_bytes: u64,
    /// Trace submissions in committed batches, including idempotent repeats.
    pub traces: u64,
}

#[derive(Clone, Copy)]
pub(crate) enum Durability {
    Wal,
    Ephemeral,
}
impl Durability {
    pub fn verify(self, connection: &Connection) -> Result<()> {
        let journal: String = connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
        match self {
            Self::Wal => {
                ensure!(
                    journal == "wal",
                    "durable store requires WAL journal mode, got {journal}"
                );
                let synchronous: i64 =
                    connection.query_row("PRAGMA synchronous", [], |row| row.get(0))?;
                ensure!(
                    synchronous == 1,
                    "durable store requires NORMAL synchronous mode, got {synchronous}"
                );
            }
            // In-memory stores have no persistent durability claim.
            Self::Ephemeral => ensure!(
                journal == "memory",
                "ephemeral store requires memory journal mode"
            ),
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct EffectKey {
    pub desc_hash: String,
    pub scope: String,
    pub occurrence: i64,
}

struct ObservedEffect {
    hash: Option<String>,
    generation: u64,
}

enum Record {
    Trace {
        bundle: loom_proto::TraceBundle,
        hash: String,
        bytes: Vec<u8>,
    },
    Event {
        event: Value,
    },
    Value {
        kind: String,
        bytes: Vec<u8>,
        hash: String,
    },
    Effect {
        key: EffectKey,
        bytes: Vec<u8>,
        hash: String,
    },
}
enum Message {
    Record {
        record: Box<Record>,
    },
    Barrier {
        durable: bool,
        reply: mpsc::Sender<Result<(), String>>,
    },
}
#[derive(Default)]
struct Shared {
    pending: Mutex<BTreeMap<EffectKey, Value>>,
    submission: Mutex<()>,
    error: Mutex<Option<String>>,
    commits: AtomicU64,
    transaction_nanos: AtomicU64,
    checkpoint_attempts: AtomicU64,
    checkpoint_nanos: AtomicU64,
    trace_bytes: AtomicU64,
    traces: AtomicU64,
    effects_generation: AtomicU64,
}
pub(crate) struct Writer {
    sender: Option<mpsc::SyncSender<Message>>,
    shared: Arc<Shared>,
    thread: Option<thread::JoinHandle<()>>,
    durability: Durability,
}
impl Writer {
    pub fn new(connection: Arc<Mutex<Connection>>, durability: Durability) -> Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(8192);
        let shared = Arc::new(Shared::default());
        let worker_shared = shared.clone();
        let thread = thread::Builder::new()
            .name("loom-recording".into())
            .spawn(move || run(connection, receiver, worker_shared, durability))?;
        Ok(Self {
            sender: Some(sender),
            shared,
            thread: Some(thread),
            durability,
        })
    }
    pub fn verify_connection(&self, connection: &Connection) -> Result<()> {
        if let Err(error) = self.durability.verify(connection) {
            *self
                .shared
                .error
                .lock()
                .map_err(|_| anyhow!("recording error lock poisoned"))? =
                Some(format!("{error:#}"));
            return Err(error);
        }
        Ok(())
    }
    pub fn check(&self) -> Result<()> {
        ensure!(
            self.thread
                .as_ref()
                .is_some_and(|thread| !thread.is_finished()),
            "recording writer stopped"
        );
        let error = self
            .shared
            .error
            .lock()
            .map_err(|_| anyhow!("recording error lock poisoned"))?;
        ensure!(
            error.is_none(),
            "recording writer failed: {}",
            error.as_deref().unwrap_or_default()
        );
        Ok(())
    }
    fn send(&self, record: Record) -> Result<()> {
        self.check()?;
        self.sender
            .as_ref()
            .ok_or_else(|| anyhow!("recording writer stopped"))?
            .send(Message::Record {
                record: Box::new(record),
            })
            .map_err(|_| anyhow!("recording writer disconnected"))?;
        self.check()
    }
    pub fn trace(&self, bundle: &loom_proto::TraceBundle) -> Result<String> {
        self.check()?;
        let total_bytes = bundle.blobs.iter().try_fold(0usize, |total, blob| {
            total
                .checked_add(blob.bytes.len())
                .ok_or_else(|| anyhow!("trace byte count overflow"))
        })?;
        ensure!(
            total_bytes <= loom_proto::TRACE_MAX_BLOB_BYTES,
            "trace blob byte limit exceeded"
        );
        ensure!(
            bundle.observations.len() <= loom_proto::TRACE_MAX_ENTRIES
                && bundle.memos.len() <= loom_proto::TRACE_MAX_ENTRIES,
            "trace summary entry limit exceeded"
        );
        let summary_bytes = bundle
            .observations
            .iter()
            .fold(0usize, |total, observation| {
                total
                    .saturating_add(observation.definition_hash.len())
                    .saturating_add(observation.op.len())
            });
        let summary_bytes = bundle.memos.iter().fold(summary_bytes, |total, memo| {
            total
                .saturating_add(memo.descriptor_hash.len())
                .saturating_add(memo.scope.len())
                .saturating_add(memo.result_hash.len())
        });
        ensure!(
            summary_bytes <= loom_proto::TRACE_MAX_METADATA_BYTES,
            "trace summary metadata limit exceeded"
        );
        let bytes = loom_proto::encode_call_trace(&bundle.trace).map_err(anyhow::Error::msg)?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        self.send(Record::Trace {
            bundle: bundle.clone(),
            hash: hash.clone(),
            bytes,
        })?;
        Ok(hash)
    }
    pub fn event(&self, event: &Value) -> Result<()> {
        ensure!(
            matches!(
                event["type"].as_str(),
                Some("effect_invoked" | "effect_denied" | "effect_completed")
            ),
            "queued recording requires an effect audit event"
        );
        self.send(Record::Event {
            event: event.clone(),
        })
    }
    pub fn value<T: serde::Serialize>(&self, kind: &str, value: &T) -> Result<String> {
        let bytes = encode(value)?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        self.send(Record::Value {
            kind: kind.into(),
            bytes,
            hash: hash.clone(),
        })?;
        Ok(hash)
    }
    pub fn pending(&self, key: &EffectKey) -> Result<Option<Value>> {
        let _submission = self.publication()?;
        self.check()?;
        Ok(self
            .shared
            .pending
            .lock()
            .map_err(|_| anyhow!("pending effects lock poisoned"))?
            .get(key)
            .cloned())
    }
    pub fn effect(
        &self,
        connection: &Mutex<Connection>,
        key: EffectKey,
        result: &Value,
    ) -> Result<String> {
        self.check()?;
        let bytes = encode(result)?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        loop {
            // Never hold publication or pending locks while waiting on SQLite:
            // a checkpoint may own that connection for the entire durable sync.
            let observed = {
                let connection = connection
                    .lock()
                    .map_err(|_| anyhow!("store lock poisoned"))?;
                let hash = connection.query_row("SELECT result_hash FROM effect_results WHERE desc_hash=? AND scope=? AND occurrence=?", params![key.desc_hash, key.scope, key.occurrence], |row| row.get(0)).optional()?;
                ObservedEffect {
                    hash,
                    generation: self.shared.effects_generation.load(Ordering::Acquire),
                }
            };
            let _submission = self.publication()?;
            self.check()?;
            let mut pending = self
                .shared
                .pending
                .lock()
                .map_err(|_| anyhow!("pending effects lock poisoned"))?;
            if let Some(existing) = pending.get(&key) {
                ensure!(existing == result, "effect cache result conflict");
                return Ok(hash);
            }
            // If the writer committed and removed a pending value after our
            // query, repeat that query before publishing a potentially conflicting
            // result. Every projection writer advances the generation under SQLite
            // ownership; the queue writer does so before clearing pending.
            if self.shared.effects_generation.load(Ordering::Acquire) != observed.generation {
                continue;
            }
            if let Some(existing) = observed.hash {
                ensure!(existing == hash, "effect cache result conflict");
                return Ok(hash);
            }
            pending.insert(key.clone(), result.clone());
            // The bounded send may wait, but the writer can still clear pending.
            // Keep submission until send completes so a duplicate cannot return
            // before the original record has entered the barrier-ordered queue.
            drop(pending);
            self.send(Record::Effect {
                key,
                bytes,
                hash: hash.clone(),
            })?;
            return Ok(hash);
        }
    }
    pub fn barrier(&self, durable: bool) -> Result<()> {
        self.check()?;
        let (reply, response) = mpsc::channel();
        self.sender
            .as_ref()
            .ok_or_else(|| anyhow!("recording writer stopped"))?
            .send(Message::Barrier { durable, reply })
            .map_err(|_| anyhow!("recording writer disconnected"))?;
        response
            .recv()
            .map_err(|_| anyhow!("recording writer disconnected"))?
            .map_err(anyhow::Error::msg)
    }
    pub fn publication(&self) -> Result<std::sync::MutexGuard<'_, ()>> {
        self.shared
            .submission
            .lock()
            .map_err(|_| anyhow!("effect submission lock poisoned"))
    }
    pub fn effects_changed(&self) {
        self.shared
            .effects_generation
            .fetch_add(1, Ordering::Release);
    }
    pub fn timings(&self) -> RecordingTimings {
        RecordingTimings {
            committed_transactions: self.shared.commits.load(Ordering::Relaxed),
            transaction_nanos: self.shared.transaction_nanos.load(Ordering::Relaxed),
            checkpoint_attempts: self.shared.checkpoint_attempts.load(Ordering::Relaxed),
            checkpoint_nanos: self.shared.checkpoint_nanos.load(Ordering::Relaxed),
            trace_bytes: self.shared.trace_bytes.load(Ordering::Relaxed),
            traces: self.shared.traces.load(Ordering::Relaxed),
        }
    }
    pub fn commits(&self) -> u64 {
        self.shared.commits.load(Ordering::Relaxed)
    }
}
impl Drop for Writer {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            eprintln!("recording writer panicked during shutdown");
        }
        if let Ok(error) = self.shared.error.lock()
            && let Some(error) = error.as_ref()
        {
            eprintln!("recording writer shutdown failed: {error}");
        }
    }
}
fn elapsed_nanos(started: Instant) -> u64 {
    // Saturation bounds the public counter representation for durations over 584 years.
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
}
fn commit(
    connection: &Mutex<Connection>,
    records: &mut Vec<Record>,
    shared: &Shared,
    durable: bool,
    durability: Durability,
) -> Result<()> {
    let mut connection = connection
        .lock()
        .map_err(|_| anyhow!("store lock poisoned"))?;
    if durable || !records.is_empty() {
        durability.verify(&connection)?;
    }
    if !records.is_empty() {
        let transaction_started = Instant::now();
        let transaction = connection.transaction()?;
        for record in records.iter() {
            match record {
                Record::Trace {
                    bundle,
                    hash,
                    bytes,
                } => {
                    crate::trace::persist(&transaction, bundle, hash, bytes)?;
                }
                Record::Event { event } => {
                    append(&transaction, "system", event, 0)?;
                }
                Record::Value { kind, bytes, hash } => {
                    transaction.execute("INSERT OR IGNORE INTO cas(hash,kind,bytes,created_at,codec) VALUES (?,?,?,unixepoch(),113)", params![hash,kind,bytes])?;
                    transaction
                        .execute("INSERT OR IGNORE INTO cas_codecs VALUES (?,113)", [hash])?;
                }
                Record::Effect { key, bytes, hash } => {
                    transaction.execute("INSERT OR IGNORE INTO cas(hash,kind,bytes,created_at,codec) VALUES (?,'result',?,unixepoch(),113)", params![hash,bytes])?;
                    transaction
                        .execute("INSERT OR IGNORE INTO cas_codecs VALUES (?,113)", [hash])?;
                    let existing: Option<String> = transaction.query_row("SELECT result_hash FROM effect_results WHERE desc_hash=? AND scope=? AND occurrence=?", params![key.desc_hash, key.scope, key.occurrence], |row| row.get(0)).optional()?;
                    ensure!(
                        existing.as_ref().is_none_or(|existing| existing == hash),
                        "effect cache result conflict"
                    );
                    if existing.is_none() {
                        append(
                            &transaction,
                            "system",
                            &serde_json::json!({"type":"effect_recorded", "desc_hash":key.desc_hash, "scope":key.scope, "occurrence":key.occurrence, "result_hash":hash}),
                            0,
                        )?;
                        transaction.execute(
                            "INSERT INTO effect_results VALUES (?,?,?,?)",
                            params![key.desc_hash, key.scope, key.occurrence, hash],
                        )?;
                    }
                }
            }
        }
        transaction.commit()?;
        shared
            .transaction_nanos
            .fetch_add(elapsed_nanos(transaction_started), Ordering::Relaxed);
        for record in records.iter() {
            if let Record::Trace { bytes, .. } = record {
                shared
                    .trace_bytes
                    .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                shared.traces.fetch_add(1, Ordering::Relaxed);
            }
        }
        shared.commits.fetch_add(1, Ordering::Relaxed);
        shared.effects_generation.fetch_add(1, Ordering::Release);
    }
    if durable {
        // NORMAL mode synchronizes the WAL and database during checkpointing.
        // A busy checkpoint cannot acknowledge the requested durability barrier.
        let checkpoint_started = Instant::now();
        let checkpoint = connection.query_row("PRAGMA wal_checkpoint(FULL)", [], |row| {
            row.get::<_, i64>(0)
        });
        shared
            .checkpoint_nanos
            .fetch_add(elapsed_nanos(checkpoint_started), Ordering::Relaxed);
        shared.checkpoint_attempts.fetch_add(1, Ordering::Relaxed);
        let busy = checkpoint?;
        ensure!(busy == 0, "recording durability checkpoint is busy");
    }
    // Release SQLite before taking pending: producers inspect committed results
    // while holding pending, and must never wait on the opposite lock order.
    drop(connection);
    let mut pending = shared
        .pending
        .lock()
        .map_err(|_| anyhow!("pending effects lock poisoned"))?;
    for record in records.drain(..) {
        if let Record::Effect { key, .. } = record {
            pending.remove(&key);
        }
    }
    Ok(())
}
fn run(
    connection: Arc<Mutex<Connection>>,
    receiver: mpsc::Receiver<Message>,
    shared: Arc<Shared>,
    durability: Durability,
) {
    let mut records = Vec::new();
    let mut deadline = Instant::now() + Duration::from_millis(100);
    loop {
        let message = if records.is_empty() {
            receiver
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        } else {
            receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        };
        let mut reply = None;
        let mut stop = false;
        let mut durable = false;
        match message {
            Ok(Message::Record { record }) => {
                if records.is_empty() {
                    deadline = Instant::now() + Duration::from_millis(100);
                }
                records.push(*record);
                if records.len() < 4096 {
                    continue;
                }
            }
            Ok(Message::Barrier {
                durable: requested,
                reply: channel,
            }) => {
                durable = requested;
                reply = Some(channel);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                durable = true;
                stop = true;
            }
        }
        let existing_error = shared.error.lock().expect("recording error lock").clone();
        let result = if let Some(error) = existing_error {
            Err(error)
        } else {
            commit(&connection, &mut records, &shared, durable, durability)
                .map_err(|error| format!("{error:#}"))
        };
        if let Err(error) = &result {
            *shared.error.lock().expect("recording error lock") = Some(error.clone());
            records.clear();
        }
        if let Some(reply) = reply {
            let _ = reply.send(result);
        }
        if stop {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    #[test]
    fn pending_reader_waits_until_a_full_queue_accepts_the_effect() -> Result<()> {
        let store = Store::memory()?;
        let connection = store.connection.lock().unwrap();
        let bytes = encode(&Value::Null)?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        // One batch waits on SQLite; the bounded channel is then completely full.
        for _ in 0..4096 + 8192 {
            store.recording.send(Record::Value {
                kind: "desc".into(),
                bytes: bytes.clone(),
                hash: hash.clone(),
            })?;
        }
        let (published, publication) = mpsc::channel();
        let (observed, observation) = mpsc::channel();
        let (completed, completion) = mpsc::channel();
        thread::scope(|scope| -> Result<()> {
            let producer = scope.spawn(|| -> Result<()> {
                let writer = &store.recording;
                let _submission = writer.publication()?;
                let key = EffectKey {
                    desc_hash: "full".into(),
                    scope: "scope".into(),
                    occurrence: 0,
                };
                writer
                    .shared
                    .pending
                    .lock()
                    .unwrap()
                    .insert(key.clone(), Value::Null);
                published.send(())?;
                writer.send(Record::Effect { key, bytes, hash })
            });
            publication.recv()?;
            let reader = scope.spawn(|| -> Result<()> {
                let value = store.effect_get("full", "scope", 0)?;
                observed.send(())?;
                store.flush()?;
                completed.send(value)?;
                Ok(())
            });
            let early = observation.recv_timeout(Duration::from_millis(20));
            drop(connection);
            producer.join().expect("producer panicked")?;
            reader.join().expect("reader panicked")?;
            assert!(matches!(early, Err(mpsc::RecvTimeoutError::Timeout)));
            assert_eq!(completion.recv()?, Some(Value::Null));
            Ok(())
        })?;
        assert_eq!(store.effect_get("full", "scope", 0)?, Some(Value::Null));
        Ok(())
    }
}
