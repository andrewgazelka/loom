//! rustfmt output recorded once per source revision. The stored source stays
//! byte for byte what the caller submitted; the formatted text is a second CAS
//! object (kind `source-formatted`) that `formatted_sources` maps the revision's
//! source hash to, so formatting runs once per revision, not once per view.
use super::*;

impl Store {
    /// The formatted text recorded for the source revision `source_hash`.
    pub fn formatted_source(&self, source_hash: &str) -> Result<Option<String>> {
        Ok(self
            .lock()?
            .query_row(
                "SELECT CAST(c.bytes AS TEXT) FROM formatted_sources f JOIN cas c ON c.hash=f.formatted_hash WHERE f.source_hash=?",
                [source_hash],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Record `formatted` as the formatting of the source revision `source_hash`
    /// and return the formatted text's CAS hash. The first recording for a
    /// revision wins; the revision's source object must already be stored.
    pub fn record_formatted_source(&self, source_hash: &str, formatted: &str) -> Result<String> {
        self.recording.barrier(false)?;
        let connection = self.lock()?;
        let hash = put(&connection, "source-formatted", formatted.as_bytes())?;
        connection.execute(
            "INSERT OR IGNORE INTO formatted_sources(source_hash,formatted_hash) VALUES (?1,?2)",
            params![source_hash, hash],
        )?;
        Ok(hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting_is_keyed_by_source_hash_and_recorded_once() -> Result<()> {
        let store = Store::memory()?;
        let source = store.put("source", b"pub fn main(){}")?;
        assert_eq!(store.formatted_source(&source)?, None);
        let formatted = store.record_formatted_source(&source, "pub fn main() {}\n")?;
        assert_eq!(formatted, content_hash(b"pub fn main() {}\n"));
        assert_eq!(
            store.formatted_source(&source)?.as_deref(),
            Some("pub fn main() {}\n")
        );
        store.record_formatted_source(&source, "different")?;
        assert_eq!(
            store.formatted_source(&source)?.as_deref(),
            Some("pub fn main() {}\n"),
            "the first recording for a revision wins"
        );
        Ok(())
    }
}
