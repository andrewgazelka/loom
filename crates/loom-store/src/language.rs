use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension};

/// Reject unsupported persisted languages before any migration changes the store.
pub(super) fn validate(connection: &Connection) -> Result<()> {
    for table in ["defs", "actors"] {
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?)",
            [table],
            |row| row.get(0),
        )?;
        if !exists {
            continue;
        }
        let language: Option<String> = connection
            .query_row(
                &format!("SELECT lang FROM {table} WHERE lang != 'rust' LIMIT 1"),
                [],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(language) = language {
            bail!(
                "unsupported guest language {language:?} in store table {table}; only rust is supported; open a new Rust-only store"
            );
        }
    }
    Ok(())
}
