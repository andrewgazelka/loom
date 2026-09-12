use anyhow::Result;
use loom_store::Store;
use rusqlite::Connection;

#[test]
fn unsupported_store_language_is_rejected_before_migration() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("store.db");
    let connection = Connection::open(&path)?;
    connection.execute_batch(
        "CREATE TABLE defs(hash TEXT NOT NULL, lang TEXT NOT NULL); \
         INSERT INTO defs VALUES ('offending-row', 'ts');",
    )?;
    let error = Store::open(&path)
        .err()
        .expect("unsupported store accepted");
    assert!(error.to_string().contains("store table defs"));
    assert!(error.to_string().contains("only rust is supported"));
    assert!(
        error.to_string().contains("offending-row"),
        "error should name the offending row: {error}"
    );
    let tables: i64 = connection.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(tables, 1, "rejected store was modified");
    Ok(())
}

#[test]
fn retired_actor_tables_are_rejected_by_name_before_modification() -> Result<()> {
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
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("old.db");
        let connection = Connection::open(&path)?;
        connection.execute_batch(&format!(
            "CREATE TABLE {table}(id TEXT); INSERT INTO {table} VALUES ('untouched');"
        ))?;
        let error = Store::open(&path)
            .err()
            .expect("retired actor store accepted");
        assert!(
            error.to_string().contains(&format!("store table {table}")),
            "{error}"
        );
        let count: i64 = connection.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(count, 1);
        let value: String =
            connection.query_row(&format!("SELECT id FROM {table}"), [], |row| row.get(0))?;
        assert_eq!(value, "untouched");
    }
    Ok(())
}
