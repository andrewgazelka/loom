use anyhow::Result;
use loom_store::Store;
use rusqlite::Connection;

#[test]
fn unsupported_store_language_is_rejected_before_migration() -> Result<()> {
    for table in ["defs", "actors"] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("store.db");
        let connection = Connection::open(&path)?;
        connection.execute_batch(&format!(
            "CREATE TABLE {table}(lang TEXT NOT NULL); INSERT INTO {table} VALUES ('ts');"
        ))?;
        let error = Store::open(&path)
            .err()
            .expect("unsupported store accepted");
        assert!(error.to_string().contains(&format!("store table {table}")));
        assert!(error.to_string().contains("only rust is supported"));
        let tables: i64 = connection.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(tables, 1, "rejected store was modified");
    }
    Ok(())
}
