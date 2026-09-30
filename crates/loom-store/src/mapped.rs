//! A stored object as memory, without copying a spilled file.
//!
//! Values of `SPILL_BYTES` and up are immutable files; [`Store::map_object`] maps one read-only and hands
//! back its bytes in place. A file mapped from offset 0 starts on a page boundary, which is what
//! `MTLDevice.newBufferWithBytesNoCopy` (and any DMA-style handoff) needs, and its last page is readable
//! zero fill, so [`MappedObject::page_aligned_len`] is a valid buffer length. Values below the threshold
//! live in SQLite and are returned as an owned copy (`is_file_backed` is false).
//!
//! Integrity: the BLAKE3 of a file is checked once per stamp (inode, size, mtime, ctime), as for `get`.
//! The store never rewrites an object in place, so a mapping of a replaced file keeps the old bytes. A
//! same-uid process that chmods and rewrites the file in place can change mapped pages; holders that care
//! call [`MappedObject::still_intact`].
use super::*;
use spill::Stamp;
use std::{fs::File, ops::Deref};

pub struct MappedObject {
    hash: String,
    backing: Backing,
}

enum Backing {
    File {
        map: memmap2::Mmap,
        file: File,
        stamp: Stamp,
    },
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
    /// Whether the bytes are a mapping of the object file (true) or a copy read from the index (false).
    pub fn is_file_backed(&self) -> bool {
        matches!(self.backing, Backing::File { .. })
    }
    /// Start of the bytes. Page-aligned when [`Self::is_file_backed`].
    pub fn as_ptr(&self) -> *const u8 {
        self.as_slice().as_ptr()
    }
    /// The length rounded up to a whole number of pages, for APIs that want a page-multiple length. Only
    /// meaningful for a file-backed object, whose mapping covers that many bytes (the tail reads as zero);
    /// an inline copy returns its own length.
    pub fn page_aligned_len(&self) -> usize {
        let length = self.as_slice().len();
        if !self.is_file_backed() {
            return length;
        }
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as usize;
        length.div_ceil(page) * page
    }
    /// Whether the object file still has the stamp it was verified under. `true` for an inline copy.
    pub fn still_intact(&self) -> bool {
        match &self.backing {
            Backing::File { file, stamp, .. } => Stamp::of(file).is_ok_and(|now| now == *stamp),
            Backing::Inline(_) => true,
        }
    }
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
        self.map_object(hash)
    }

    /// The bytes at `hash` as memory: a read-only mapping for a spilled file (verified once per stamp),
    /// an owned copy for an inline value. `None` when nothing is stored there. A missing or corrupt object
    /// file is the typed [`ObjectError`].
    pub fn map_object(&self, hash: &str) -> Result<Option<MappedObject>> {
        self.recording.barrier(false)?;
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
                let (map, file, stamp) = spill.map(hash, size)?;
                Backing::File { map, file, stamp }
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
