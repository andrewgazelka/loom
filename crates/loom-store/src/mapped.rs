//! A stored object as memory, without copying a spilled file on filesystems that can clone.
//!
//! Values of `SPILL_BYTES` and up are immutable files. [`Store::map_object`] verifies one (once per stamp),
//! clones it to a private file with no path, and maps that read-only. The mapping is therefore of an inode
//! nothing else can open: a same-uid process cannot rewrite or truncate it in place, and the store's own
//! object can be replaced or unlinked while it is mapped (an unlink is always safe for a mapping; a
//! truncate of the store's file would not be, and nothing here does one, so a future sweeper must only
//! unlink). On APFS the clone is a metadata operation; where it fails the object is copied once, and
//! [`MappedObject::is_cloned`] says which happened.
//!
//! A file mapped from offset 0 starts on a page boundary, and its last page reads as zero, which is what
//! `MTLDevice.newBufferWithBytesNoCopy` needs ([`MappedObject::page_aligned_len`]). Values below the
//! threshold live in SQLite and come back as an owned copy. No file descriptor is held per mapping.
//!
//! Residual risk: an I/O error or an ejected volume while pages are faulted in raises SIGBUS, which the
//! process cannot catch; only a private clone on the same volume narrows that, it does not remove it.
use super::*;
use std::ops::Deref;

pub struct MappedObject {
    hash: String,
    backing: Backing,
}

enum Backing {
    /// A read-only mapping of a private, unlinked copy. `cloned` is false when bytes were really copied.
    File { map: memmap2::Mmap, cloned: bool },
    Inline(Vec<u8>),
}

impl MappedObject {
    pub fn hash(&self) -> &str {
        &self.hash
    }
    pub fn as_slice(&self) -> &[u8] {
        match &self.backing {
            Backing::File { map, .. } => map,
            Backing::Inline(bytes) => bytes,
        }
    }
    /// Whether the bytes are a mapping of a file (true) or a copy read from the index (false).
    pub fn is_file_backed(&self) -> bool {
        matches!(self.backing, Backing::File { .. })
    }
    /// Whether mapping was free: a clone, not a copy of the bytes. `false` for an inline value and for a
    /// filesystem that cannot clone.
    pub fn is_cloned(&self) -> bool {
        matches!(self.backing, Backing::File { cloned: true, .. })
    }
    /// Start of the bytes. Page-aligned when [`Self::is_file_backed`].
    pub fn as_ptr(&self) -> *const u8 {
        self.as_slice().as_ptr()
    }
    /// The length rounded up to whole pages, the length a no-copy GPU buffer wants. The mapping covers
    /// that many bytes (the tail of the last page reads as zero). `None` for an inline value, which is
    /// not page-aligned memory.
    pub fn page_aligned_len(&self) -> Option<usize> {
        if !self.is_file_backed() {
            return None;
        }
        let page = page_size();
        Some(self.as_slice().len().div_ceil(page) * page)
    }
}

#[cfg(unix)]
fn page_size() -> usize {
    match unsafe { libc::sysconf(libc::_SC_PAGESIZE) } {
        size if size > 0 => size as usize,
        _ => 16384,
    }
}
#[cfg(not(unix))]
fn page_size() -> usize {
    4096
}

impl Deref for MappedObject {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl Store {
    /// [`Self::map_object`] for an object stored with `kind`; `None` for a missing hash and for one stored
    /// as anything else, so a caller cannot tell the two apart (the rule `get_of_kind` has).
    pub fn map_object_of_kind(&self, hash: &str, kind: &str) -> Result<Option<MappedObject>> {
        self.recording.barrier(false)?;
        let stored: Option<String> = self
            .lock()?
            .query_row("SELECT kind FROM cas WHERE hash=?", [hash], |row| row.get(0))
            .optional()?;
        if stored.as_deref() != Some(kind) {
            return Ok(None);
        }
        self.map_verified(hash)
    }

    /// Map the object a reference names, refusing one whose stored length differs from `reference.len`.
    /// The length in a reference comes from whoever produced it; this is the check that ties it to the bytes.
    pub fn map_store_ref(
        &self,
        reference: &loom_proto::StoreRef,
        kind: Option<&str>,
    ) -> Result<Option<MappedObject>> {
        let hash = reference.hex();
        let mapped = match kind {
            Some(kind) => self.map_object_of_kind(&hash, kind)?,
            None => self.map_object(&hash)?,
        };
        if let Some(mapped) = &mapped {
            ensure!(
                mapped.len() as u64 == reference.len,
                "store reference names {} bytes but the object has {}",
                reference.len,
                mapped.len()
            );
        }
        Ok(mapped)
    }

    /// The bytes at `hash` as memory: a read-only mapping of a private clone for a spilled file
    /// (verified once per stamp), an owned copy for an inline value. `None` when nothing is stored there.
    /// A missing or corrupt object file is the typed [`ObjectError`].
    pub fn map_object(&self, hash: &str) -> Result<Option<MappedObject>> {
        self.recording.barrier(false)?;
        self.map_verified(hash)
    }

    fn map_verified(&self, hash: &str) -> Result<Option<MappedObject>> {
        let hash = objects::bare_hash(hash)?;
        let hash = hash.as_str();
        let stored = {
            let connection = self.lock()?;
            blobs::stored(&connection, hash)?
        };
        let backing = match stored {
            Some(blobs::Stored::External { size }) => {
                let spill = self
                    .spill
                    .as_deref()
                    .context("external CAS object in a store that has no objects directory")?;
                let (map, cloned) = spill.map(hash, size)?;
                Backing::File { map, cloned }
            }
            Some(blobs::Stored::Inline(bytes)) => {
                if blake3::hash(&bytes).to_hex().as_str() != hash {
                    return Err(ObjectError::mismatch(hash));
                }
                Backing::Inline(bytes)
            }
            None => match self.transient.blob(hash) {
                Some(bytes) => Backing::Inline(bytes),
                None => return Ok(None),
            },
        };
        Ok(Some(MappedObject {
            hash: hash.to_owned(),
            backing,
        }))
    }
}
