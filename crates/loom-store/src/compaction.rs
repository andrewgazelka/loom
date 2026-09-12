use super::*;

impl Store {
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
}
