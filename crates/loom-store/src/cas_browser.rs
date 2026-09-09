use crate::Store;
use anyhow::{Context, Result, ensure};
use loom_proto::{CasCodec, CasEntry, CasListRequest, CasPage, DAG_CBOR_CODEC, RAW_CODEC};
use rusqlite::{Connection, OptionalExtension, params};

impl Store {
    pub fn cas_list(&self, request: &CasListRequest) -> Result<CasPage> {
        ensure!(
            (1..=1000).contains(&request.limit),
            "CAS list limit must be 1..=1000"
        );
        if let Some(after) = &request.after {
            ensure!(
                is_hash(after),
                "CAS cursor must be a 64-character lowercase hexadecimal hash"
            );
        }
        if let Some(prefix) = &request.q {
            ensure!(
                prefix.len() <= 64 && prefix.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "CAS hash prefix must contain at most 64 hexadecimal characters"
            );
        }
        if let Some(kind) = &request.kind {
            ensure!(
                !kind.is_empty() && kind.len() <= 128,
                "CAS kind must contain 1..=128 bytes"
            );
        }
        let prefix = request.q.as_ref().map(|prefix| prefix.to_ascii_lowercase());
        let connection = self.lock()?;
        let mut query=connection.prepare("SELECT hash,kind,length(bytes),created_at FROM cas WHERE hash>coalesce(?1,'') AND (?2 IS NULL OR kind=?2) AND (?3 IS NULL OR hash LIKE ?3 || '%') ORDER BY hash LIMIT ?4")?;
        let mut entries = query
            .query_map(
                params![request.after, request.kind, prefix, request.limit + 1],
                row_entry,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let has_more = entries.len() > request.limit;
        if has_more {
            entries.pop();
        }
        for entry in &mut entries {
            entry.codecs = codecs(&connection, &entry.hash)?;
        }
        let next_cursor = if has_more {
            entries.last().map(|entry| entry.hash.clone())
        } else {
            None
        };
        Ok(CasPage {
            items: entries,
            next_cursor,
        })
    }
    pub fn cas_entry(&self, address: &str) -> Result<Option<CasEntry>> {
        // Validate a supplied CID registration before exposing its metadata.
        if self.codec(address)?.is_none() {
            return Ok(None);
        }
        let hash = address_hash(address)?;
        let connection = self.lock()?;
        let mut entry = connection
            .query_row(
                "SELECT hash,kind,length(bytes),created_at FROM cas WHERE hash=?",
                [&hash],
                row_entry,
            )
            .optional()?;
        if let Some(entry) = &mut entry {
            entry.codecs = codecs(&connection, &hash)?;
        }
        Ok(entry)
    }
    pub fn cas_prefix(&self, address: &str, limit: usize) -> Result<Option<Vec<u8>>> {
        ensure!(
            (1..=1024 * 1024).contains(&limit),
            "CAS preview limit must be 1..=1048576 bytes"
        );
        if self.codec(address)?.is_none() {
            return Ok(None);
        }
        let hash = address_hash(address)?;
        Ok(self
            .lock()?
            .query_row(
                "SELECT substr(bytes,1,?) FROM cas WHERE hash=?",
                params![limit, hash],
                |row| row.get(0),
            )
            .optional()?)
    }
}
fn is_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
fn address_hash(address: &str) -> Result<String> {
    if is_hash(address) {
        Ok(address.into())
    } else {
        Ok(loom_proto::parse_reference(address)
            .map_err(anyhow::Error::msg)?
            .hash)
    }
}
fn row_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<CasEntry> {
    Ok(CasEntry {
        hash: row.get(0)?,
        kind: row.get(1)?,
        size: row.get(2)?,
        created_at: row.get(3)?,
        codecs: Vec::new(),
    })
}
fn codecs(connection: &Connection, hash: &str) -> Result<Vec<CasCodec>> {
    let mut query =
        connection.prepare("SELECT codec FROM cas_codecs WHERE hash=? ORDER BY codec")?;
    let codes = query
        .query_map([hash], |row| row.get::<_, u64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    codes
        .into_iter()
        .map(|code| {
            Ok(CasCodec {
                code,
                name: match code {
                    DAG_CBOR_CODEC => "dag-cbor",
                    RAW_CODEC => "raw",
                    _ => anyhow::bail!("unsupported registered CAS codec"),
                }
                .into(),
                cid: loom_proto::cid_for_hash(hash, code)
                    .map_err(anyhow::Error::msg)
                    .context("invalid stored CAS address")?,
            })
        })
        .collect()
}
