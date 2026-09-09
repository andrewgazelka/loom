use anyhow::Result;
use rusqlite::Connection;

/// Restore the complete lookup projection once when opening durable storage.
/// Effect misses must never decode historical events on the execution path.
pub(super) fn rebuild(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction()?;
    transaction.execute("DELETE FROM effect_results", [])?;
    transaction.execute(
        "INSERT INTO effect_results(desc_hash,scope,occurrence,result_hash)
         SELECT json_extract(bytes,'$.desc_hash'),json_extract(bytes,'$.scope'),
                json_extract(bytes,'$.occurrence'),json_extract(bytes,'$.result_hash')
         FROM effects WHERE true ORDER BY seq
         ON CONFLICT(desc_hash,scope,occurrence) DO NOTHING",
        [],
    )?;
    transaction.commit()?;
    Ok(())
}
