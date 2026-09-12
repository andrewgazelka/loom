use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension};

/// Reject unsupported persisted languages before any migration changes the store.
pub(super) fn validate(connection: &Connection) -> Result<()> {
    for (table, id_column) in [("defs", "hash"), ("actors", "id")] {
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?)",
            [table],
            |row| row.get(0),
        )?;
        if !exists {
            continue;
        }
        let row: Option<(String, String)> = connection
            .query_row(
                &format!("SELECT {id_column},lang FROM {table} WHERE lang != 'rust' LIMIT 1"),
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((id, language)) = row {
            bail!(
                "unsupported guest language {language:?} in store table {table} row {id_column}={id:?}; only rust is supported; open a new Rust-only store"
            );
        }
    }
    Ok(())
}
