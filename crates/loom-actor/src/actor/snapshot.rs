use super::query;
use anyhow::{Context, Result};
use std::path::Path;
use turso::Connection;

pub(crate) async fn snapshot(conn: &Connection, path: &Path, seq: i64) -> Result<()> {
    let known = query(conn, "SELECT path FROM snapshots WHERE seq=?", [seq]).await?;
    if !known.rows.is_empty() {
        return Ok(());
    }
    replace_snapshot(conn, path, seq).await
}

/// The image file only: the caller decides what record points at it.
pub(crate) async fn write_snapshot_file(conn: &Connection, path: &Path) -> Result<()> {
    compact_cdc(conn).await?;
    let path = path.to_str().context("snapshot path is not UTF-8")?;
    let staging = format!("{path}.pending");
    if Path::new(&staging).exists() {
        std::fs::remove_file(&staging)?;
    }
    conn.execute(format!("VACUUM INTO '{}'", staging.replace('\'', "''")), ()).await?;
    std::fs::rename(&staging, path)?;
    Ok(())
}

pub(crate) async fn replace_snapshot(conn: &Connection, path: &Path, seq: i64) -> Result<()> {
    compact_cdc(conn).await?;
    let path = path.to_str().context("snapshot path is not UTF-8")?;
    let staging = format!("{path}.pending");
    // A previous crash can leave this unreferenced staging copy.
    if Path::new(&staging).exists() {
        std::fs::remove_file(&staging)?;
    }
    conn.execute(format!("VACUUM INTO '{}'", staging.replace('\'', "''")), ()).await?;
    std::fs::rename(&staging, path)?;
    conn.execute(
        "INSERT INTO snapshots(seq,path) VALUES (?,?) ON CONFLICT(seq) DO UPDATE SET path=excluded.path",
        turso::params![seq, path],
    )
    .await?;
    Ok(())
}

pub(crate) async fn compact_cdc(conn: &Connection) -> Result<()> {
    anyhow::ensure!(
        conn.is_autocommit()?,
        "actor {} seq {}: CDC snapshot requires a committed connection",
        super::meta(conn, "id").await?,
        super::cursor(conn).await?
    );
    // The next snapshot replaces cdc_floor, deletes the CDC prefix, and removes
    // record_origin metadata once its transaction has no retained CDC row.
    // Freeze the floor before subscriber bookkeeping itself emits runtime CDC rows.
    let floor = crate::cdc::high_water(conn).await?;
    let result = conn
        .execute_batch(format!(
            "BEGIN; UPDATE subscribers SET after_change_id={floor} \
         WHERE after_change_id>=CAST((SELECT value FROM meta WHERE key='cdc_floor') AS INTEGER) \
         AND NOT EXISTS (SELECT 1 FROM turso_cdc WHERE table_name=subscribers.\"table\" \
         AND change_id>subscribers.after_change_id AND change_type!=2); \
         INSERT INTO meta(key,value) VALUES ('cdc_floor',{floor}) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value; \
         DELETE FROM turso_cdc WHERE change_id<CAST((SELECT value FROM meta WHERE key='cdc_floor') AS INTEGER); \
         DELETE FROM meta WHERE key LIKE 'cdc_origin:%' AND CAST(substr(key,12) AS INTEGER) NOT IN \
         (SELECT DISTINCT change_txn_id FROM turso_cdc WHERE change_txn_id IS NOT NULL); COMMIT;",
        ))
        .await;
    if let Err(error) = result {
        if !conn.is_autocommit()? {
            conn.execute("ROLLBACK", ()).await?;
        }
        return Err(error.into());
    }
    Ok(())
}
