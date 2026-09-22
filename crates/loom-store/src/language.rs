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
            "SELECT hash,lang FROM defs WHERE lang NOT IN ('rust', 'javascript', 'typescript') LIMIT 1",
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
            "unsupported guest language {:?} in store table defs row hash={:?}; supported languages are rust, javascript and typescript",
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
            "unsupported definition identity in store table defs: row hash={} differs from driver export root={}; open a new store",
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
            let bytes = bytes.context("driver item document missing from CAS")?;
            let document: serde_json::Value = serde_json::from_slice(&bytes)?;
            let exports: std::collections::BTreeMap<String, String> =
                serde_json::from_value(document["exports"].clone())
                    .context("driver item document has no exports")?;
            ensure!(!exports.is_empty(), "driver item document has no exports");
            let root = blake3::hash(&loom_proto::export_identity_preimage(&exports));
            ensure!(
                root.to_hex().as_str() == hash,
                "expected Merkle export root {root}"
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
        let exports = std::collections::BTreeMap::from([("main".to_owned(), entry.clone())]);
        let root = store.put(
            "export-root",
            &loom_proto::export_identity_preimage(&exports),
        )?;
        let hash = if old_identity { entry } else { root };
        let document = store.put(
            "item-hashes",
            &serde_json::to_vec(&serde_json::json!({"entry":exports,"exports":exports}))?,
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
        assert!(
            error.to_string().contains("Merkle export root"),
            "{error:#}"
        );
        Ok(())
    }

    #[test]
    fn reopen_accepts_merkle_export_identity() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("store.db");
        let hash = populated_store(&path, false)?;
        let store = Store::open(&path)?;
        assert!(store.definition(&hash)?.is_some());
        Ok(())
    }

    /// A document from the entry-rooted driver carries no `exports`; the row
    /// is named and the store refuses to open rather than recomputing a root
    /// from `entry`.
    #[test]
    fn reopen_rejects_item_document_without_exports() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("store.db");
        let hash = populated_store(&path, false)?;
        {
            let store = Store::open(&path)?;
            let entry = store.put("item-preimage", b"entry")?;
            let entries = std::collections::BTreeMap::from([("main".to_owned(), entry)]);
            let legacy = store.put(
                "item-hashes",
                &serde_json::to_vec(&serde_json::json!({"entry":entries}))?,
            )?;
            store.with_connection(|connection| {
                connection.execute(
                    "UPDATE defs SET item_hashes_ref=? WHERE hash=?",
                    rusqlite::params![legacy, hash],
                )?;
                Ok(())
            })?;
        }
        let error = Store::open(&path)
            .err()
            .context("document without exports accepted")?;
        assert!(error.to_string().contains(&hash), "{error:#}");
        assert!(error.to_string().contains("has no exports"), "{error:#}");
        Ok(())
    }
}
