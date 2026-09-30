use super::*;

impl Store {
    pub fn put(&self, kind: &str, bytes: &[u8]) -> Result<String> {
        self.recording.barrier(false)?;
        put(&*self.lock()?, self.spill.as_deref(), kind, bytes)
    }
    /// The bytes at `hash` when they were stored with `kind`; `None` for a missing hash
    /// and for one stored as anything else, so a caller cannot tell the two apart.
    pub fn get_of_kind(&self, hash: &str, kind: &str) -> Result<Option<Vec<u8>>> {
        self.recording.barrier(false)?;
        let stored: Option<String> = self
            .lock()?
            .query_row("SELECT kind FROM cas WHERE hash=?", [hash], |row| {
                row.get(0)
            })
            .optional()?;
        if stored.as_deref() != Some(kind) {
            return Ok(None);
        }
        self.get(hash)
    }
    pub fn get(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        self.recording.barrier(false)?;
        let address = if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            None
        } else {
            Some(loom_proto::parse_reference(hash).map_err(anyhow::Error::msg)?)
        };
        let hash = address.as_ref().map_or(hash, |a| a.hash.as_str());
        let c = self.lock()?;
        if let Some(address) = &address {
            let exists: bool = c.query_row(
                "SELECT EXISTS(SELECT 1 FROM cas WHERE hash=?)",
                [hash],
                |r| r.get(0),
            )?;
            let registered: bool = c.query_row(
                "SELECT EXISTS(SELECT 1 FROM cas_codecs WHERE hash=? AND codec=?)",
                params![hash, address.codec],
                |r| r.get(0),
            )?;
            ensure!(
                !exists || registered,
                "CID codec is not registered for stored object"
            );
        }
        let stored = blobs::stored(&c, hash)?;
        // Read an object file without holding the connection.
        drop(c);
        if let Some(stored) = stored {
            return match stored {
                // Inline values are verified on every read.
                blobs::Stored::Inline(bytes) => {
                    ensure!(
                        blake3::hash(&bytes).to_hex().as_str() == hash,
                        "CAS content hash mismatch"
                    );
                    Ok(Some(bytes))
                }
                // A spilled file is verified once per process inside `read`.
                external => external.load(self.spill.as_deref(), hash).map(Some),
            };
        }
        // A transient definition's component is held in memory, addressed by
        // its content hash like any CAS blob.
        if address.is_none() {
            return Ok(self.transient.blob(hash));
        }
        Ok(None)
    }
    /// The size in bytes of the stored object at `hash` (a bare hash or a CID),
    /// without reading a spilled file. `None` when nothing is stored there.
    pub fn size_of(&self, hash: &str) -> Result<Option<u64>> {
        self.recording.barrier(false)?;
        let hash = bare_hash(hash)?;
        Ok(self
            .lock()?
            .query_row(
                "SELECT coalesce(size,length(bytes)) FROM cas WHERE hash=?",
                [&hash],
                |row| row.get(0),
            )
            .optional()?)
    }
    /// Whether the object at `hash` (a bare hash or a CID) is stored and, for a
    /// spilled one, its file is present with the recorded size. `size_of` answers
    /// from the index alone; a caller that is about to use the bytes (a cache hit
    /// naming this object) asks this. Content is not hashed here.
    pub fn has_object(&self, hash: &str) -> Result<bool> {
        self.recording.barrier(false)?;
        let hash = bare_hash(hash)?;
        let row: Option<(bool, Option<u64>)> = self
            .lock()?
            .query_row(
                "SELECT external,size FROM cas WHERE hash=?",
                [&hash],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match row {
            None => Ok(false),
            Some((false, _)) => Ok(true),
            Some((true, size)) => self
                .spill
                .as_deref()
                .context("external CAS object in a store that has no objects directory")?
                .has(&hash, size.context("external CAS row has no size")?),
        }
    }
    /// Make the raw object at `hash` (a bare hash or a CID) appear at `dest`,
    /// replacing any existing file atomically (exclusive temp name, then rename).
    /// A spilled object is cloned on APFS, else copied, and its BLAKE3 is checked
    /// unless this process already verified this very file (inode, size and
    /// timestamps unchanged). The result is an independent 0644 file that shares
    /// no inode with the store, for spilled and inline objects alike, so a
    /// consumer may write to it.
    pub fn restore_to(&self, hash: &str, dest: &Path) -> Result<()> {
        self.recording.barrier(false)?;
        let hash = bare_hash(hash)?;
        let (codec, stored) = {
            let c = self.lock()?;
            let codec: Option<u64> = c
                .query_row("SELECT codec FROM cas WHERE hash=?", [&hash], |r| r.get(0))
                .optional()?;
            (codec, blobs::stored(&c, &hash)?)
        };
        let stored = stored.context("CAS object not found")?;
        ensure!(
            codec == Some(loom_proto::RAW_CODEC),
            "CAS restore requires a raw object"
        );
        match stored {
            blobs::Stored::Inline(bytes) => {
                ensure!(
                    blake3::hash(&bytes).to_hex().as_str() == hash,
                    "CAS content hash mismatch"
                );
                spill::write_atomic(dest, &bytes)
            }
            blobs::Stored::External { size } => self
                .spill
                .as_deref()
                .context("external CAS object in a store that has no objects directory")?
                .restore(&hash, size, dest),
        }
    }
    /// Make every spilled object of this store available under `directory/objects/` as a
    /// verified copy (never a link to the live file; a corrupt source is refused), so a database
    /// copied to `directory` opens with all its bytes. Returns how many objects were brought over. A store with no
    /// objects directory has nothing to bring.
    pub fn export_spilled_to(&self, directory: &Path) -> Result<u64> {
        self.recording.barrier(false)?;
        let Some(source) = self.spill.as_deref() else {
            return Ok(0);
        };
        let rows: Vec<(String, u64)> = {
            let c = self.lock()?;
            let mut query = c.prepare("SELECT hash,size FROM cas WHERE external=1")?;
            query
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        let destination = spill::Spill::open(directory)?;
        for (hash, size) in &rows {
            destination.adopt(source, hash, *size)?;
        }
        destination.sync_dirs()?;
        Ok(rows.len() as u64)
    }
    pub fn put_value<T: serde::Serialize>(&self, kind: &str, value: &T) -> Result<String> {
        self.recording.barrier(false)?;
        put_value(&*self.lock()?, kind, value)
    }
    pub fn get_value<T: serde::de::DeserializeOwned>(&self, hash: &str) -> Result<Option<T>> {
        // A typed lookup selects DAG-CBOR for internal hex identities, while an
        // explicit CID must retain its caller-selected codec.
        let dag = if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            Some(loom_proto::cid_for_hash(hash, 113).map_err(anyhow::Error::msg)?)
        } else {
            None
        };
        let hash = dag.as_deref().unwrap_or(hash);
        ensure!(
            self.codec(hash)?.is_none_or(|codec| codec == 113),
            "CAS object is raw bytes, not DAG-CBOR"
        );
        let Some(bytes) = self.get(hash)? else {
            return Ok(None);
        };
        let address = loom_proto::parse_reference(hash).map_err(anyhow::Error::msg)?;
        let kind: Option<String> = self
            .lock()?
            .query_row(
                "SELECT kind FROM cas WHERE hash=?",
                [&address.hash],
                |row| row.get(0),
            )
            .optional()?;
        if kind.as_deref() == Some("trace") {
            let trace = loom_proto::decode_call_trace(&bytes).map_err(anyhow::Error::msg)?;
            return Ok(Some(serde_json::from_value(serde_json::to_value(trace)?)?));
        }
        Ok(Some(decode(&bytes)?))
    }
    pub fn codec(&self, hash: &str) -> Result<Option<u64>> {
        self.recording.barrier(false)?;
        let address = if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            None
        } else {
            Some(loom_proto::parse_reference(hash).map_err(anyhow::Error::msg)?)
        };
        let hash = address.as_ref().map_or(hash, |a| a.hash.as_str());
        let c = self.lock()?;
        let codec: Option<u64> = c
            .query_row("SELECT codec FROM cas WHERE hash=?", params![hash], |r| {
                r.get(0)
            })
            .optional()?;
        if let Some(address) = &address {
            if codec.is_none() {
                return Ok(None);
            }
            let registered: bool = c.query_row(
                "SELECT EXISTS(SELECT 1 FROM cas_codecs WHERE hash=? AND codec=?)",
                params![hash, address.codec],
                |r| r.get(0),
            )?;
            ensure!(registered, "CID codec is not registered for stored object");
            return Ok(Some(address.codec));
        }
        Ok(codec)
    }
    pub fn reference(&self, hash: &str, codec: u64) -> Result<Value> {
        let hash = if hash.len() == 64 {
            hash.to_owned()
        } else {
            loom_proto::parse_reference(hash)
                .map_err(anyhow::Error::msg)?
                .hash
        };
        let reference = loom_proto::reference(&hash, codec).map_err(anyhow::Error::msg)?;
        self.codec(
            reference["$ref"]
                .as_str()
                .context("invalid generated reference")?,
        )?
        .context("CAS object not found")?;
        Ok(reference)
    }
}

/// The bare 64-hex hash of a hash or CID.
fn bare_hash(hash: &str) -> Result<String> {
    if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(hash.to_owned())
    } else {
        Ok(loom_proto::parse_reference(hash)
            .map_err(anyhow::Error::msg)?
            .hash)
    }
}
