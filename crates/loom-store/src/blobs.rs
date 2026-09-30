//! The SQL side of spilled blobs: which `cas` rows are external, and how to
//! read one back. Rows of raw objects at or above `SPILL_BYTES` in a
//! file-backed store have `external=1`, an empty `bytes` and the byte count in
//! `size`; `size` is NULL for inline rows, so `coalesce(size,length(bytes))` is
//! the size of any object. Only raw (codec 85) rows are ever external.
use crate::spill::{SPILL_BYTES, Spill, spillable_kind};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};

/// Add the external-blob columns to a store created before they existed.
/// Existing rows stay inline; nothing is rewritten.
pub(crate) fn migrate_schema(connection: &Connection) -> Result<()> {
    let present: i64 = connection.query_row(
        "SELECT count(*) FROM pragma_table_info('cas') WHERE name IN ('external','size')",
        [],
        |row| row.get(0),
    )?;
    match present {
        2 => Ok(()),
        0 => Ok(connection.execute_batch(
            "BEGIN;
             ALTER TABLE cas ADD COLUMN external INTEGER NOT NULL DEFAULT 0 CHECK(external IN (0,1));
             ALTER TABLE cas ADD COLUMN size INTEGER;
             COMMIT;",
        )?),
        _ => bail!(
            "unsupported store schema: table cas has only one of the columns external and size; open a new store"
        ),
    }
}

/// How the bytes of a `cas` row are held.
pub(crate) enum Stored {
    Inline(Vec<u8>),
    External { size: u64 },
}

impl Stored {
    /// The bytes, reading the object file for an external row. Inline bytes are
    /// returned as stored; verifying them is the caller's policy.
    pub fn load(self, spill: Option<&Spill>, hash: &str) -> Result<Vec<u8>> {
        match self {
            Self::Inline(bytes) => Ok(bytes),
            Self::External { size } => spill
                .context("external CAS object in a store that has no objects directory")?
                .read(hash, size),
        }
    }
}

/// The row for `hash`, without touching any object file.
pub(crate) fn stored(connection: &Connection, hash: &str) -> Result<Option<Stored>> {
    let row: Option<(Vec<u8>, bool, Option<u64>)> = connection
        .query_row(
            "SELECT bytes,external,size FROM cas WHERE hash=?",
            [hash],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    row.map(|(bytes, external, size)| {
        if external {
            let size = size.context("external CAS row has no size")?;
            Ok(Stored::External { size })
        } else {
            Ok(Stored::Inline(bytes))
        }
    })
    .transpose()
}

/// The bytes of the object at `hash`, inline or external.
pub(crate) fn cas_bytes(
    connection: &Connection,
    spill: Option<&Spill>,
    hash: &str,
) -> Result<Option<Vec<u8>>> {
    stored(connection, hash)?
        .map(|stored| stored.load(spill, hash))
        .transpose()
}

/// `INSERT OR IGNORE` the object. A raw object of `SPILL_BYTES` or more goes to
/// a file first (when the store has an objects directory), then gets an
/// external row. An existing row wins, including an inline row of a large
/// object written before spilling existed. `created_at` defaults to now.
pub(crate) fn insert_object(
    connection: &Connection,
    spill: Option<&Spill>,
    hash: &str,
    kind: &str,
    codec: u64,
    bytes: &[u8],
    created_at: Option<i64>,
) -> Result<()> {
    let spill = spill.filter(|_| {
        codec == loom_proto::RAW_CODEC && bytes.len() >= SPILL_BYTES && spillable_kind(kind)
    });
    let Some(spill) = spill else {
        connection.execute(
            "INSERT OR IGNORE INTO cas(hash,kind,bytes,created_at,codec) VALUES (?,?,?,coalesce(?,unixepoch()),?)",
            params![hash, kind, bytes, created_at, codec],
        )?;
        return Ok(());
    };
    let existing: Option<bool> = connection
        .query_row("SELECT external FROM cas WHERE hash=?", [hash], |row| {
            row.get(0)
        })
        .optional()?;
    // An external row whose file is missing, the wrong size or corrupt is healed by rewriting the
    // file (`ensure` hashes a file this process has not verified).
    if existing != Some(false) {
        spill.ensure(hash, bytes)?;
    }
    insert_external(
        connection,
        hash,
        kind,
        codec,
        bytes.len() as u64,
        created_at,
    )
}

/// Index an object whose file is already in place.
pub(crate) fn insert_external(
    connection: &Connection,
    hash: &str,
    kind: &str,
    codec: u64,
    size: u64,
    created_at: Option<i64>,
) -> Result<()> {
    connection.execute(
        "INSERT OR IGNORE INTO cas(hash,kind,bytes,created_at,codec,external,size) VALUES (?,?,X'',coalesce(?,unixepoch()),?,1,?)",
        params![hash, kind, created_at, codec, size],
    )?;
    Ok(())
}
