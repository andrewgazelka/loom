#[cfg(test)]
mod tests;
mod worker;
use worker::run;

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
