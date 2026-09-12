use super::{connect, inject, set_meta};
use crate::Behavior;
use anyhow::{Context, Result};
use std::path::Path;
use turso::Connection;

pub(crate) async fn initialize(
    path: &Path,
    id: &str,
    parent: &str,
    behavior: &dyn Behavior,
    msg: &[u8],
    io: crate::Io,
    durability: crate::Durability,
) -> Result<Connection> {
    let staging = path.with_extension(format!("creating-{}", ulid::Ulid::new()));
    let mut conn = connect(&staging, io).await?;
    let tx = conn.transaction().await?;
    tx.execute_batch(crate::SCHEMA).await?;
    set_meta(&tx, "id", id).await?;
    set_meta(&tx, "parent", parent).await?;
    set_meta(&tx, "node_root", if parent.is_empty() { "true" } else { "false" }).await?;
    set_meta(&tx, "ready", if parent.is_empty() { "true" } else { "false" }).await?;
    set_meta(&tx, "cursor", "0").await?;
    set_meta(&tx, "durability", durability.name()).await?;
    set_meta(&tx, "durability_seq", "0").await?;
    set_meta(&tx, "commit_epoch", "0").await?;
    set_meta(&tx, "boundary:0", "0").await?;
    set_meta(&tx, "hook_counter", "0").await?;
    set_meta(&tx, "memory_max", "0").await?;
    set_meta(&tx, "fuel", "0").await?;
    set_meta(&tx, "status", "running").await?;
    set_meta(&tx, "strategy", "park").await?;
    set_meta(&tx, "trap_exit", "false").await?;
    set_meta(&tx, "generation", "0").await?;
    set_meta(&tx, "event_counter", "0").await?;
    set_meta(&tx, "reason", "").await?;
    set_meta(&tx, "init", &serde_json::to_string(msg)?).await?;
    tx.execute_batch(behavior.schema()).await?;
    tx.execute(
        "INSERT INTO code_changes(seq,behavior_hash,author,rationale,schema_sql) VALUES (0,?,'runtime','spawn',?)",
        [behavior.hash(), behavior.schema()],
    )
    .await?;
    if !msg.is_empty() {
        inject(&tx, "init", parent, msg).await?;
    }
    tx.commit().await?;
    if io == crate::Io::Memory {
        return Ok(conn);
    }
    let ready = staging.with_extension("ready");
    let ready_str = ready.to_str().context("actor path is not UTF-8")?;
    conn.execute(format!("VACUUM INTO '{}'", ready_str.replace('\'', "''")), ()).await?;
    std::fs::rename(&ready, path)?;
    drop(conn);
    std::fs::remove_file(&staging)?;
    let wal = format!("{}-wal", staging.display());
    if Path::new(&wal).exists() {
        std::fs::remove_file(wal)?;
    }
    connect(path, io).await
}
