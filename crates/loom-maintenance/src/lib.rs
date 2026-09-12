//! Maintenance preserves CAS and definition history. Only derived indexes are collected.
use anyhow::{Result, ensure};
use loom_store::Store;
use rusqlite::{Connection, params};
use serde::Serialize;
use std::path::Path;

mod build_cache;
pub use build_cache::{CacheEviction, CachePolicy, evict_build_cache};

#[derive(Debug, Serialize)]
pub struct Stats {
    pub cas_objects: u64,
    pub cas_bytes: u64,
    pub events: u64,
    pub latest_seq: i64,
    pub definitions: u64,
    pub effect_index_entries: u64,
    pub database_bytes: u64,
    pub reusable_database_bytes: u64,
}

pub fn stats(store: &Store) -> Result<Stats> {
    store.with_connection(|connection| {
        let scalar =
            |sql: &str| -> Result<u64> { Ok(connection.query_row(sql, [], |row| row.get(0))?) };
        let page_size = scalar("PRAGMA page_size")?;
        Ok(Stats {
            cas_objects: scalar("SELECT count(*) FROM cas")?,
            cas_bytes: scalar("SELECT coalesce(sum(length(bytes)),0) FROM cas")?,
            events: scalar("SELECT count(*) FROM definition_records")?,
            latest_seq: connection.query_row(
                "SELECT coalesce(max(seq),0) FROM definition_records",
                [],
                |row| row.get(0),
            )?,
            definitions: scalar("SELECT count(*) FROM defs")?,
            effect_index_entries: scalar("SELECT count(*) FROM effect_results")?,
            database_bytes: scalar("PRAGMA page_count")? * page_size,
            reusable_database_bytes: scalar("PRAGMA freelist_count")? * page_size,
        })
    })
}

#[derive(Debug, Serialize)]
pub struct Collection {
    pub removed_index_entries: usize,
}

/// Remove at most `limit` orphan lookup rows. Recorded keys remain indexed for
/// replay; deleting their index rows does not reclaim any result or event bytes.
pub fn collect_effect_index(store: &Store, limit: usize) -> Result<Collection> {
    ensure!(
        (1..=1000).contains(&limit),
        "collection limit must be 1..=1000"
    );
    store.with_connection(|connection| {
        let removed_index_entries = connection.execute(
            "WITH recorded AS MATERIALIZED (
                SELECT json_extract(bytes,'$.desc_hash') AS desc_hash,
                       json_extract(bytes,'$.scope') AS scope,
                       json_extract(bytes,'$.occurrence') AS occurrence FROM effects
             ) DELETE FROM effect_results WHERE rowid IN (
                SELECT e.rowid FROM effect_results e WHERE NOT EXISTS (
                    SELECT 1 FROM recorded r WHERE r.desc_hash=e.desc_hash
                    AND r.scope=e.scope AND r.occurrence=e.occurrence
                ) ORDER BY e.rowid LIMIT ?
             )",
            [limit as i64],
        )?;
        Ok(Collection {
            removed_index_entries,
        })
    })
}

#[derive(Debug, Serialize)]
pub struct Backup {
    pub bytes: u64,
    pub latest_seq: i64,
}

/// SQLite creates a consistent snapshot including WAL contents. Refuse existing
/// targets, validate SQLite integrity, and return the snapshot's own sequence.
pub fn backup(store: &Store, destination: &Path) -> Result<Backup> {
    ensure!(!destination.exists(), "backup destination already exists");
    let destination_text = destination
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("backup path must be UTF-8"))?;
    store.with_connection(|connection| {
        connection.execute("VACUUM INTO ?", params![destination_text])?;
        Ok(())
    })?;
    let snapshot =
        Connection::open_with_flags(destination, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let integrity: String = snapshot.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    ensure!(
        integrity == "ok",
        "backup integrity check failed: {integrity}"
    );
    let latest_seq = snapshot.query_row(
        "SELECT coalesce(max(seq),0) FROM definition_records",
        [],
        |row| row.get(0),
    )?;
    Ok(Backup {
        bytes: destination.metadata()?.len(),
        latest_seq,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn collection_preserves_replay_and_is_bounded() -> Result<()> {
        let store = Store::memory()?;
        store.effect_put("a", "global", 0, &json!(11))?;
        store.effect_put("b", "global", 1, &json!(22))?;
        store.with_connection(|connection| {
            connection.execute("INSERT INTO effect_results SELECT 'orphan-a',scope,occurrence,result_hash FROM effect_results WHERE desc_hash='a'", [])?;
            connection.execute("INSERT INTO effect_results SELECT 'orphan-b',scope,occurrence,result_hash FROM effect_results WHERE desc_hash='b'", [])?;
            Ok(())
        })?;
        let before = stats(&store)?;
        assert_eq!(collect_effect_index(&store, 1)?.removed_index_entries, 1);
        let after = stats(&store)?;
        assert_eq!(after.effect_index_entries, 3);
        assert_eq!(before.cas_objects, after.cas_objects);
        assert_eq!(before.events, after.events);
        assert_eq!(store.effect_get("a", "global", 0)?, Some(json!(11)));
        assert_eq!(store.effect_get("b", "global", 1)?, Some(json!(22)));
        assert_eq!(store.effect_get("a", "global", 1)?, None);
        assert!(store.effect_put("a", "global", 0, &json!(999)).is_err());
        assert!(collect_effect_index(&store, 1001).is_err());
        Ok(())
    }

    #[test]
    fn backup_captures_wal_and_refuses_overwrite() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let store = Store::open(directory.path().join("live.sqlite"))?;
        store.effect_put("a", "global", 0, &json!(11))?;
        let destination = directory.path().join("backup.sqlite");
        let result = backup(&store, &destination)?;
        assert!(result.bytes > 0);
        assert_eq!(result.latest_seq, store.latest_seq()?);
        store.effect_put("b", "global", 0, &json!(22))?;
        let restored = Store::open(&destination)?;
        assert_eq!(restored.effect_get("a", "global", 0)?, Some(json!(11)));
        assert_eq!(restored.effect_get("b", "global", 0)?, None);
        assert!(backup(&store, &destination).is_err());
        Ok(())
    }

    #[test]
    fn backup_preserves_effect_replay_after_index_collection() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let store = Store::memory()?;
        for occurrence in 0..20 {
            store.effect_put(
                "effect",
                "global",
                occurrence,
                &json!({"padding": "repeated data ".repeat(100), "occurrence": occurrence}),
            )?;
        }
        collect_effect_index(&store, 1000)?;
        let destination = directory.path().join("effects.sqlite");
        backup(&store, &destination)?;
        let restored = Store::open(&destination)?;
        assert_eq!(restored.definition_events(0, 1000)?.len(), 20);
        for occurrence in 0..20 {
            assert_eq!(
                restored
                    .effect_get("effect", "global", occurrence)?
                    .unwrap()["occurrence"],
                json!(occurrence)
            );
        }
        assert!(
            restored
                .effect_put("effect", "global", 0, &json!("conflict"))
                .is_err()
        );
        Ok(())
    }
}
