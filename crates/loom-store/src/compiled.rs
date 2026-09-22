//! The text a component was compiled from, recorded once per artifact. The
//! builder hands back the exact bytes the compiler saw (the materialized
//! source, the wrapper marker, the generated entry wrappers); it is a CAS
//! object (kind `source-compiled`) that `compiled_sources` maps the component
//! hash to, so a module's DWARF line numbers always have the text they index,
//! whatever the definition's source has since become.
use super::*;

impl Store {
    /// The text `component_hash` was compiled from, if recorded.
    pub fn compiled_source(&self, component_hash: &str) -> Result<Option<String>> {
        Ok(self
            .lock()?
            .query_row(
                "SELECT CAST(c.bytes AS TEXT) FROM compiled_sources s JOIN cas c ON c.hash=s.compiled_hash WHERE s.component_hash=?",
                [component_hash],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Record `text` as what `component_hash` was compiled from and return the
    /// text's CAS hash. The component object must already be stored; a second
    /// recording for the same component is ignored (one artifact, one text).
    pub fn record_compiled_source(&self, component_hash: &str, text: &str) -> Result<String> {
        self.recording.barrier(false)?;
        let connection = self.lock()?;
        let hash = put(&connection, "source-compiled", text.as_bytes())?;
        connection.execute(
            "INSERT OR IGNORE INTO compiled_sources(component_hash,compiled_hash) VALUES (?1,?2)",
            params![component_hash, hash],
        )?;
        Ok(hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiled_text_is_keyed_by_component_and_recorded_once() -> Result<()> {
        let store = Store::memory()?;
        let component = store.put("component", b"\0asm")?;
        assert_eq!(store.compiled_source(&component)?, None);
        let recorded = store.record_compiled_source(&component, "pub fn f() {}\n// marker\n")?;
        assert_eq!(recorded, content_hash(b"pub fn f() {}\n// marker\n"));
        assert_eq!(
            store.compiled_source(&component)?.as_deref(),
            Some("pub fn f() {}\n// marker\n")
        );
        store.record_compiled_source(&component, "other")?;
        assert_eq!(
            store.compiled_source(&component)?.as_deref(),
            Some("pub fn f() {}\n// marker\n"),
            "one artifact, one text"
        );
        assert!(
            store.record_compiled_source(&"f".repeat(64), "x").is_err(),
            "an unknown component is refused by the foreign key"
        );
        Ok(())
    }
}
