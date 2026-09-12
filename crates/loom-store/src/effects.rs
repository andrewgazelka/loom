use super::*;

impl Store {
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
        record_definition_event(
            &tx,
            &serde_json::json!({"type":"effect_recorded","desc_hash":desc_hash,"scope":scope,"occurrence":occurrence,"result_hash":hash}),
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
