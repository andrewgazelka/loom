use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension};

struct UnsupportedDefinition {
    hash: String,
    language: String,
}

/// Reject unsupported persisted languages before any migration changes the store.
pub(super) fn validate(connection: &Connection) -> Result<()> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='defs')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(());
    }
    let definition = connection
        .query_row(
            "SELECT hash,lang FROM defs WHERE lang != 'rust' LIMIT 1",
            [],
            |row| {
                Ok(UnsupportedDefinition {
                    hash: row.get(0)?,
                    language: row.get(1)?,
                })
            },
        )
        .optional()?;
    if let Some(definition) = definition {
        bail!(
            "unsupported guest language {:?} in store table defs row hash={:?}; only rust is supported; open a new Rust-only store",
            definition.language,
            definition.hash,
        );
    }
    for column in [
        "behavior_hash",
        "wasm_hash",
        "toolchain_hash",
        "item_hashes_ref",
    ] {
        let present: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('defs') WHERE name=?)",
            [column],
            |row| row.get(0),
        )?;
        if !present {
            bail!("unsupported store schema: table defs missing column {column}; open a new store");
        }
    }
    struct InvalidIdentity {
        hash: String,
        behavior_hash: String,
    }
    let invalid = connection.query_row(
        "SELECT hash,behavior_hash FROM defs WHERE behavior_hash IS NOT NULL AND hash != behavior_hash LIMIT 1",
        [], |row| Ok(InvalidIdentity { hash: row.get(0)?, behavior_hash: row.get(1)? }),
    ).optional()?;
    if let Some(invalid) = invalid {
        bail!(
            "unsupported definition identity in store table defs: row hash={} differs from driver entry root={}; open a new store",
            invalid.hash,
            invalid.behavior_hash
        );
    }
    let mut statement = connection.prepare(
        "SELECT d.hash,c.bytes FROM defs d LEFT JOIN cas c ON c.hash=d.item_hashes_ref WHERE d.behavior_hash IS NOT NULL",
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let hash: String = row.get(0)?;
        let validation = || -> Result<()> {
            let bytes: Option<Vec<u8>> = row.get(1)?;
            let bytes = bytes.context("driver entry document missing from CAS")?;
            let document: serde_json::Value = serde_json::from_slice(&bytes)?;
            let entries: std::collections::BTreeMap<String, String> =
                serde_json::from_value(document["entry"].clone())?;
            ensure!(!entries.is_empty(), "driver entry document has no entries");
            let root = blake3::hash(&loom_proto::entry_identity_preimage(&entries));
            ensure!(
                root.to_hex().as_str() == hash,
                "expected Merkle entry root {root}"
            );
            Ok(())
        };
        if let Err(error) = validation() {
            bail!(
                "unsupported definition identity in store table defs: row hash={hash}: {error:#}; open a new store"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    fn populated_store(path: &std::path::Path, old_identity: bool) -> Result<String> {
        let store = Store::open(path)?;
        let entry = store.put("item-preimage", b"entry")?;
        let entries = std::collections::BTreeMap::from([("main".to_owned(), entry.clone())]);
        let root = store.put("entry-root", &loom_proto::entry_identity_preimage(&entries))?;
        let hash = if old_identity { entry } else { root };
        let document = store.put(
            "item-hashes",
            &serde_json::to_vec(&serde_json::json!({"entry":entries}))?,
        )?;
        let definition = loom_proto::Def {
            hash: hash.clone(),
            lang: loom_proto::Lang::Rust,
            component_hash: None,
            sig: Default::default(),
            allowed_effects: None,
            observed_effects: Vec::new(),
        };
        store.define(
            &definition,
            Some("main"),
            "pub fn main() {}",
            &Default::default(),
        )?;
        store.with_connection(|connection| {
            connection.execute(
                "UPDATE defs SET behavior_hash=?,item_hashes_ref=? WHERE hash=?",
                rusqlite::params![hash, document, hash],
            )?;
            Ok(())
        })?;
        Ok(hash)
    }

    #[test]
    fn reopen_rejects_legacy_entry_identity_by_hash() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("store.db");
        let hash = populated_store(&path, true)?;
        let error = Store::open(&path)
            .err()
            .context("legacy identity accepted")?;
        assert!(error.to_string().contains(&hash), "{error:#}");
        assert!(error.to_string().contains("Merkle entry root"), "{error:#}");
        Ok(())
    }

    #[test]
    fn reopen_accepts_merkle_entry_identity() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("store.db");
        let hash = populated_store(&path, false)?;
        let store = Store::open(&path)?;
        assert!(store.definition(&hash)?.is_some());
        Ok(())
    }
}
