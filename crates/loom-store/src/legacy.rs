use anyhow::{Result, bail};
use rusqlite::Connection;

/// Reject retired actor stores before schema initialization changes any bytes.
pub(super) fn validate(connection: &Connection) -> Result<()> {
    for table in [
        "actors",
        "inbox",
        concat!("inbox", "_queue"),
        "log",
        "snapshots",
        "message_keys",
        "sessions",
        "archive_segments",
        "archive_entries",
    ] {
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?)",
            [table],
            |row| row.get(0),
        )?;
        if exists {
            bail!(
                "retired actor model in store table {table}; open a new store with per-actor Turso storage"
            );
        }
    }
    Ok(())
}
