use anyhow::{Result, bail};
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
            "unsupported definition identity in store table defs: row hash={} differs from driver entry hash={}; open a new store",
            invalid.hash,
            invalid.behavior_hash
        );
    }
    Ok(())
}
