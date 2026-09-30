//! Stream large native artifacts into the CAS: object files from `SPILL_BYTES` up,
//! the SQLite blob column below that or in a store without an objects directory.
use super::*;
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

const CHUNK_BYTES: usize = 64 * 1024;

impl Store {
    /// Hash once, then copy: into an object file when the source is `SPILL_BYTES`
    /// or more and the store has an objects directory, else into a
    /// transaction-owned incremental SQLite blob. A second hash refuses source
    /// mutation between the discovery and copy passes.
    pub fn put_file(&self, kind: &str, path: &Path) -> Result<String> {
        self.put_open_file(kind, File::open(path)?)
    }

    /// `put_file` from an already opened handle, so a caller that vetted the file through the
    /// handle (not the path) stores exactly what it vetted.
    pub fn put_open_file(&self, kind: &str, mut input: File) -> Result<String> {
        ensure!(
            !matches!(
                kind,
                "event" | "result" | "state" | "message" | "tree" | "desc"
            ),
            "structured CAS kind requires put_value"
        );
        ensure!(
            input.metadata()?.is_file(),
            "CAS source must be a regular file"
        );
        let length = input.metadata()?.len();
        let mut hasher = blake3::Hasher::new();
        let mut buffer = vec![0; CHUNK_BYTES];
        loop {
            let n = input.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }
        let hash = hasher.finalize().to_hex().to_string();
        input.seek(SeekFrom::Start(0))?;
        self.recording.barrier(false)?;
        if let Some(spill) = self
            .spill
            .as_deref()
            .filter(|_| length >= spill::SPILL_BYTES as u64 && spill::spillable_kind(kind))
        {
            let existing: Option<bool> = self
                .lock()?
                .query_row("SELECT external FROM cas WHERE hash=?", [&hash], |row| {
                    row.get(0)
                })
                .optional()?;
            // The copy runs without the connection. An existing inline row (a
            // large object stored before spilling) keeps its bytes and needs no file.
            if existing != Some(false) {
                spill.ingest(&hash, &mut input, length)?;
            }
            let mut connection = self.lock()?;
            let transaction = connection.transaction()?;
            blobs::insert_external(
                &transaction,
                &hash,
                kind,
                loom_proto::RAW_CODEC,
                length,
                None,
            )?;
            transaction.execute("INSERT OR IGNORE INTO cas_codecs VALUES (?,85)", [&hash])?;
            transaction.commit()?;
            return Ok(hash);
        }
        ensure!(
            length <= i32::MAX as u64,
            "CAS source exceeds SQLite blob limit"
        );
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let existing: Option<i64> = transaction
            .query_row("SELECT rowid FROM cas WHERE hash=?", [&hash], |row| {
                row.get(0)
            })
            .optional()?;
        if let Some(rowid) = existing {
            let mut blob =
                transaction.blob_open(rusqlite::DatabaseName::Main, "cas", "bytes", rowid, true)?;
            let mut existing_hash = blake3::Hasher::new();
            loop {
                let n = blob.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                existing_hash.update(&buffer[..n]);
            }
            ensure!(
                existing_hash.finalize().to_hex().as_str() == hash,
                "CAS content hash mismatch"
            );
        } else {
            transaction.execute("INSERT INTO cas(hash,kind,bytes,created_at,codec) VALUES (?,?,zeroblob(?),unixepoch(),85)", params![hash,kind,length])?;
            let rowid = transaction.last_insert_rowid();
            let mut blob = transaction.blob_open(
                rusqlite::DatabaseName::Main,
                "cas",
                "bytes",
                rowid,
                false,
            )?;
            let mut copied = blake3::Hasher::new();
            let mut total = 0u64;
            loop {
                let n = input.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                blob.write_all(&buffer[..n])?;
                copied.update(&buffer[..n]);
                total += n as u64;
            }
            ensure!(
                total == length && copied.finalize().to_hex().as_str() == hash,
                "CAS source changed during import"
            );
            blob.close()?;
        }
        transaction.execute("INSERT OR IGNORE INTO cas_codecs VALUES (?,85)", [&hash])?;
        transaction.commit()?;
        Ok(hash)
    }

    /// Materialize a raw object into a new file without overwriting user data.
    /// The caller can rename the verified file into its owned runtime directory.
    pub fn export_file(&self, reference: &str, destination: &Path) -> Result<()> {
        self.export_file_controlled(reference, destination, None)
    }

    pub fn export_file_cancellable(
        &self,
        reference: &str,
        destination: &Path,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<()> {
        self.export_file_controlled(reference, destination, Some(cancelled))
    }

    fn export_file_controlled(
        &self,
        reference: &str,
        destination: &Path,
        cancelled: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<()> {
        let check_cancelled = || -> Result<()> {
            ensure!(
                !cancelled.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire)),
                "CAS export cancelled"
            );
            Ok(())
        };
        check_cancelled()?;
        let address = loom_proto::parse_reference(reference).map_err(anyhow::Error::msg)?;
        ensure!(
            address.codec == loom_proto::RAW_CODEC,
            "CAS file reference must use raw codec"
        );
        self.recording.barrier(false)?;
        let connection = self.lock()?;
        let row: Option<(i64, bool, Option<u64>)> = connection.query_row("SELECT cas.rowid,cas.external,cas.size FROM cas JOIN cas_codecs USING(hash) WHERE cas.hash=? AND cas_codecs.codec=85", [&address.hash], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).optional()?;
        let (rowid, external, size) = row.context("CAS file not found in this tenant")?;
        if external {
            let size = size.context("external CAS row has no size")?;
            // Stream the object file without holding the connection.
            drop(connection);
            let spill = self
                .spill
                .as_deref()
                .context("external CAS object in a store that has no objects directory")?;
            let mut input = spill.open_file(&address.hash, size)?;
            let stamp = spill::Stamp::of(&input)?;
            copy_verified(&mut input, &address.hash, destination, &check_cancelled)?;
            spill.mark_file_verified(&address.hash, &input, stamp)?;
            return Ok(());
        }
        let mut blob =
            connection.blob_open(rusqlite::DatabaseName::Main, "cas", "bytes", rowid, true)?;
        copy_verified(&mut blob, &address.hash, destination, &check_cancelled)
    }
}

/// Copy `input` into a new file at `destination`, refusing content that does not
/// hash to `expected`; a failed copy leaves no file behind.
fn copy_verified(
    input: &mut impl Read,
    expected: &str,
    destination: &Path,
    check_cancelled: &dyn Fn() -> Result<()>,
) -> Result<()> {
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let result = (|| -> Result<()> {
        let mut hash = blake3::Hasher::new();
        let mut buffer = vec![0; CHUNK_BYTES];
        loop {
            check_cancelled()?;
            let n = input.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            output.write_all(&buffer[..n])?;
            hash.update(&buffer[..n]);
        }
        ensure!(
            hash.finalize().to_hex().as_str() == expected,
            "CAS content hash mismatch"
        );
        check_cancelled()?;
        output.sync_all()?;
        Ok(())
    })();
    drop(output);
    if result.is_err() {
        std::fs::remove_file(destination).context("remove incomplete CAS export")?;
    }
    result
}
