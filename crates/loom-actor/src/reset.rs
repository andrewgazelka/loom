//! Reset builds a complete new incarnation before publishing it over the live path.
use crate::{Node, actor};
use anyhow::{Context, Result};
use std::{collections::BTreeSet, path::Path};
use turso::Connection;

impl Node {
    pub(crate) async fn reset(&self, conn: &mut Connection, id: &str, key: &str) -> Result<()> {
        let generation = actor::meta(conn, "generation").await?.parse::<i64>()?.checked_add(1).context("generation overflow")?;
        let archive = self.dir.join(format!("{id}.reset.{generation}.db"));
        vacuum(conn, &archive).await?;
        let build = self.dir.join(format!("{id}.reset-building"));
        remove_database(&build)?;
        let mut fresh = actor::connect(&build).await?;
        let tx = fresh.transaction().await?;
        tx.execute_batch(crate::SCHEMA).await?;
        actor::set_meta(&tx, "id", id).await?;
        actor::set_meta(&tx, "parent", &actor::meta(conn, "parent").await?).await?;
        actor::set_meta(&tx, "init", &actor::meta(conn, "init").await?).await?;
        actor::set_meta(&tx, "cursor", "0").await?;
        actor::set_meta(&tx, "commit_epoch", "0").await?;
        actor::set_meta(&tx, "boundary:0", "0").await?;
        actor::set_meta(&tx, "hook_counter", "0").await?;
        actor::set_meta(&tx, "memory_max", &actor::meta(conn, "memory_max").await?).await?;
        actor::set_meta(&tx, "fuel", &actor::meta(conn, "fuel").await?).await?;
        actor::set_meta(&tx, "status", "running").await?;
        actor::set_meta(&tx, "node_root", &actor::meta(conn, "node_root").await?).await?;
        actor::set_meta(&tx, "ready", "false").await?;
        actor::set_meta(&tx, "reason", "").await?;
        actor::set_meta(&tx, "strategy", "park").await?;
        actor::set_meta(&tx, "trap_exit", "false").await?;
        actor::set_meta(&tx, "event_counter", "0").await?;
        actor::set_meta(&tx, "generation", &generation.to_string()).await?;
        let receipts = actor::query(conn, "SELECT key,value FROM meta WHERE key LIKE 'applied:%'", ()).await?;
        for row in receipts.rows {
            actor::set_meta(&tx, &row.get::<String>(0)?, &row.get::<String>(1)?).await?;
        }
        actor::set_meta(&tx, &format!("applied:{key}"), "1").await?;
        let changes =
            actor::query(conn, "SELECT seq,behavior_hash,parent_hash,author,rationale,schema_sql FROM code_changes ORDER BY seq", ())
                .await?;
        let mut seen = BTreeSet::new();
        for row in changes.rows {
            let hash: String = row.get(1)?;
            if seen.insert(hash.clone()) {
                tx.execute_batch(actor::behavior(&self.registry, &hash)?.schema()).await?;
            }
            let values = (0..row.column_count()).map(|i| row.get_value(i)).collect::<turso::Result<Vec<_>>>()?;
            tx.execute("INSERT INTO code_changes(seq,behavior_hash,parent_hash,author,rationale,schema_sql) VALUES (?,?,?,?,?,?)", values)
                .await?;
        }
        for table in ["links", "monitors", "monitored_by"] {
            let rows = actor::query(conn, &format!("SELECT * FROM {table} ORDER BY rowid"), ()).await?;
            for row in rows.rows {
                let values = (0..row.column_count()).map(|i| row.get_value(i)).collect::<turso::Result<Vec<_>>>()?;
                let placeholders = vec!["?"; row.column_count()].join(",");
                tx.execute(format!("INSERT INTO {table} VALUES ({placeholders})"), values).await?;
            }
        }
        let init: Vec<u8> = serde_json::from_str(&actor::meta(conn, "init").await?)?;
        actor::inject(&tx, "init", &actor::meta(conn, "parent").await?, &init).await?;
        tx.commit().await?;
        actor::snapshot(&fresh, &self.snapshot_path(id, generation, 0), 0).await?;
        let ready = self.dir.join(format!("{id}.reset-publish"));
        vacuum(&fresh, &ready).await?;
        drop(fresh);
        remove_database(&build)?;
        // Drop the original connection while holding the shared actor mutex. Every
        // Actor handle subsequently observes the replacement connection in this slot.
        let old = std::mem::replace(conn, actor::connect(Path::new(":memory:")).await?);
        drop(old);
        recover(&self.path(id))?;
        *conn = actor::connect(&self.path(id)).await?;
        Ok(())
    }
}

async fn vacuum(conn: &Connection, path: &Path) -> Result<()> {
    let pending = path.with_extension("copy-pending");
    remove_database(&pending)?;
    let sql = format!("VACUUM INTO '{}'", pending.to_str().context("non-UTF8 reset path")?.replace('\'', "''"));
    conn.execute(sql, ()).await?;
    std::fs::rename(&pending, path)?;
    Ok(())
}

fn remove_database(path: &Path) -> Result<()> {
    for path in [path.to_path_buf(), format!("{}-wal", path.display()).into()] {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}

/// The publish filename is created only after VACUUM has completed. A process
/// interrupted between WAL removal and rename rolls forward before opening.
pub(crate) fn recover(path: &Path) -> Result<()> {
    let ready = path.with_extension("reset-publish");
    if ready.exists() {
        let wal = format!("{}-wal", path.display());
        if Path::new(&wal).exists() {
            std::fs::remove_file(wal)?;
        }
        std::fs::rename(ready, path)?;
    }
    Ok(())
}
