use super::*;

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
pub(super) fn run(
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
