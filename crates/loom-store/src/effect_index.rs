use anyhow::Result;
use rusqlite::Connection;

/// Upgrade the lookup projection transactionally once. Explicit rebuild_views
/// remains the repair operation; opening an already migrated store is O(1).
pub(super) fn migrate(connection: &mut Connection) -> Result<()> {
    connection
        .execute_batch("CREATE TABLE IF NOT EXISTS store_migrations(name TEXT PRIMARY KEY)")?;
    let applied: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM store_migrations WHERE name='call_trace_v2')",
        [],
        |row| row.get(0),
    )?;
    if applied {
        return Ok(());
    }
    let transaction = connection.transaction()?;
    transaction.execute("DELETE FROM effect_results", [])?;
    transaction.execute(
        "INSERT INTO effect_results(desc_hash,scope,occurrence,result_hash)
         SELECT json_extract(bytes,'$.desc_hash'),json_extract(bytes,'$.scope'),
                json_extract(bytes,'$.occurrence'),json_extract(bytes,'$.result_hash')
         FROM events WHERE json_extract(bytes,'$.type')='effect_recorded' ORDER BY seq
         ON CONFLICT(desc_hash,scope,occurrence) DO NOTHING",
        [],
    )?;
    crate::trace::rebuild(&transaction)?;
    crate::trace::migrate_legacy(&transaction)?;
    transaction.execute("INSERT INTO store_migrations VALUES ('call_trace_v2')", [])?;
    transaction.commit()?;
    Ok(())
}
