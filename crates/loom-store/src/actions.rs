//! Results of hermetic actions (`loom-action`): a pure function of the action's key, so the
//! newest recording for a key replaces the old one and readers never see a partial result.
use super::*;

impl Store {
    /// The result object recorded for `action_key`, when one is recorded and still stored.
    pub fn action_result(&self, action_key: &str) -> Result<Option<String>> {
        self.recording.barrier(false)?;
        Ok(self
            .lock()?
            .query_row(
                "SELECT result_hash FROM action_results WHERE action_key=?",
                [action_key],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Record that `action_key` produced the stored object `result_hash`.
    pub fn record_action_result(&self, action_key: &str, result_hash: &str) -> Result<()> {
        self.recording.barrier(false)?;
        self.lock()?.execute(
            "INSERT OR REPLACE INTO action_results(action_key,result_hash,created_at) VALUES (?1,?2,unixepoch())",
            params![action_key, result_hash],
        )?;
        Ok(())
    }

    /// Forget every recorded action result (the objects stay in the CAS). Returns how many.
    pub fn clear_action_results(&self) -> Result<usize> {
        self.recording.barrier(false)?;
        Ok(self.lock()?.execute("DELETE FROM action_results", [])?)
    }
}
