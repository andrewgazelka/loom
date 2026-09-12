use super::*;

impl Store {
    pub fn latest_seq(&self) -> Result<i64> {
        self.recording.barrier(false)?;
        Ok(self
            .lock()?
            .query_row("SELECT coalesce(max(seq),0) FROM log", [], |r| r.get(0))?)
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
}
