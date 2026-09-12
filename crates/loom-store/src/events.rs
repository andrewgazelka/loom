use super::*;

impl Store {
    pub fn latest_seq(&self) -> Result<i64> {
        self.recording.barrier(false)?;
        Ok(self.lock()?.query_row(
            "SELECT coalesce(max(seq),0) FROM definition_records",
            [],
            |row| row.get(0),
        )?)
    }
    pub fn record_definition_event(&self, event: &Value) -> Result<i64> {
        self.recording.barrier(false)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let seq = record_definition_event(&transaction, event)?;
        transaction.commit()?;
        Ok(seq)
    }
    pub fn definition_events(&self, after: i64, limit: usize) -> Result<Vec<Event>> {
        self.recording.barrier(false)?;
        let connection = self.lock()?;
        let mut query = connection.prepare(
            "SELECT seq,bytes,ts FROM definition_events WHERE seq>? ORDER BY seq LIMIT ?",
        )?;
        let mut rows = query.query(params![after, limit.min(1000) as i64])?;
        let mut events = Vec::new();
        while let Some(row) = rows.next()? {
            events.push(Event {
                seq: row.get(0)?,
                event: serde_json::from_slice(&row.get::<_, Vec<u8>>(1)?)?,
                ts: row.get(2)?,
            });
        }
        Ok(events)
    }
}
