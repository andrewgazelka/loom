//! Stream large native artifacts through the existing SQLite CAS blob column.
use super::*;
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

const CHUNK_BYTES: usize = 64 * 1024;

impl Store {
    /// Hash once, then copy into a transaction-owned incremental SQLite blob.
    /// A second hash refuses source mutation between the discovery and copy passes.
    pub fn put_file(&self, kind: &str, path: &Path) -> Result<String> {
        ensure!(
            !matches!(
                kind,
                "event" | "result" | "state" | "message" | "tree" | "desc"
            ),
            "structured CAS kind requires put_value"
        );
        let mut input = File::open(path)?;
        ensure!(
            input.metadata()?.is_file(),
            "CAS source must be a regular file"
        );
        let length = input.metadata()?.len();
        ensure!(
            length <= i32::MAX as u64,
            "CAS source exceeds SQLite blob limit"
        );
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
        let rowid: Option<i64> = connection.query_row("SELECT cas.rowid FROM cas JOIN cas_codecs USING(hash) WHERE cas.hash=? AND cas_codecs.codec=85", [&address.hash], |row| row.get(0)).optional()?;
        let rowid = rowid.context("CAS file not found in this tenant")?;
        let mut blob =
            connection.blob_open(rusqlite::DatabaseName::Main, "cas", "bytes", rowid, true)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination)?;
        let result = (|| -> Result<()> {
            let mut hash = blake3::Hasher::new();
            let mut buffer = vec![0; CHUNK_BYTES];
            loop {
                check_cancelled()?;
                let n = blob.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                output.write_all(&buffer[..n])?;
                hash.update(&buffer[..n]);
            }
            ensure!(
                hash.finalize().to_hex().as_str() == address.hash,
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
}
