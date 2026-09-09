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

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct EffectKey {
    pub desc_hash: String,
    pub scope: String,
    pub occurrence: i64,
}

enum Record {
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
        record: Record,
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
}
pub(crate) struct Writer {
    sender: Option<mpsc::SyncSender<Message>>,
    shared: Arc<Shared>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Writer {
    pub fn new(connection: Arc<Mutex<Connection>>) -> Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(8192);
        let shared = Arc::new(Shared::default());
        let worker_shared = shared.clone();
        let thread = thread::Builder::new()
            .name("loom-recording".into())
            .spawn(move || run(connection, receiver, worker_shared))?;
        Ok(Self {
            sender: Some(sender),
            shared,
            thread: Some(thread),
        })
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
            .send(Message::Record { record })
            .map_err(|_| anyhow!("recording writer disconnected"))?;
        self.check()
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
        let _submission = self
            .shared
            .submission
            .lock()
            .map_err(|_| anyhow!("effect submission lock poisoned"))?;
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
        {
            let connection = connection
                .lock()
                .map_err(|_| anyhow!("store lock poisoned"))?;
            let existing: Option<String> = connection.query_row("SELECT result_hash FROM effect_results WHERE desc_hash=? AND scope=? AND occurrence=?", params![key.desc_hash, key.scope, key.occurrence], |row| row.get(0)).optional()?;
            if let Some(existing) = existing {
                ensure!(existing == hash, "effect cache result conflict");
                return Ok(hash);
            }
        }
        pending.insert(key.clone(), result.clone());
        // Do not block a full queue while holding the lock the writer needs.
        drop(pending);
        self.send(Record::Effect {
            key,
            bytes,
            hash: hash.clone(),
        })?;
        Ok(hash)
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
    pub fn commits(&self) -> u64 {
        self.shared.commits.load(Ordering::Relaxed)
    }
}
impl Drop for Writer {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                eprintln!("recording writer panicked during shutdown");
            }
        }
        if let Ok(error) = self.shared.error.lock() {
            if let Some(error) = error.as_ref() {
                eprintln!("recording writer shutdown failed: {error}");
            }
        }
    }
}
fn checkpoint(connection: &Connection) -> Result<()> {
    // SQLite synchronizes the WAL before a FULL checkpoint even in NORMAL mode.
    // A busy checkpoint is not a successful durability barrier.
    let busy: i64 = connection.query_row("PRAGMA wal_checkpoint(FULL)", [], |row| row.get(0))?;
    ensure!(busy == 0, "recording durability checkpoint is busy");
    Ok(())
}
fn commit(
    connection: &Mutex<Connection>,
    records: &mut Vec<Record>,
    shared: &Shared,
    durable: bool,
) -> Result<()> {
    let mut connection = connection
        .lock()
        .map_err(|_| anyhow!("store lock poisoned"))?;
    if !records.is_empty() {
        let transaction = connection.transaction()?;
        for record in records.iter() {
            match record {
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
        shared.commits.fetch_add(1, Ordering::Relaxed);
    }
    if durable {
        checkpoint(&connection)?;
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
fn run(connection: Arc<Mutex<Connection>>, receiver: mpsc::Receiver<Message>, shared: Arc<Shared>) {
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
                records.push(record);
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
            commit(&connection, &mut records, &shared, durable)
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
