//! Raw blobs of `SPILL_BYTES` and up are immutable files, not SQLite rows.
//!
//! Layout under the store directory: `objects/<first hex char>/<64-hex hash>`
//! and `objects/tmp/`. A file is written to `objects/tmp/` (created exclusively),
//! `sync_all`ed, marked read-only, renamed into place and its shard directory is
//! synced before the write returns; only then may the caller index it, so a
//! durable index row never names a file that a crash can lose, whichever path
//! (`Store::put`, `put_file`, intake, the recording writer) commits the row.
//! The one exception is a file already present when a write is deduplicated: it
//! was placed by an earlier writer, possibly in a process that died between its
//! rename and its directory sync, so its shard is marked dirty and `sync_dirs`
//! (run at the recording writer's durability barrier, `Store::flush`) syncs it.
//! An orphan file after a crash is harmless.
//!
//! Object files are never shared with consumers: a restore is a clone (APFS) or
//! a copy, never a hard link, so nothing outside the store can hold one of its
//! inodes. Whether a file's content was verified is remembered per file, by a
//! stamp of its inode, size and timestamps; any change forces a re-hash.
//!
//! This module knows files only; the SQL side is in `blobs.rs`.
use anyhow::{Context, Result, anyhow, ensure};
use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Values of this many bytes and up are files.
pub(crate) const SPILL_BYTES: usize = 1 << 20;

/// Kinds whose bytes are only ever handled whole, by hash: compiled modules, build artifacts,
/// uploads and opaque blobs. Every other kind may be read inside SQL (`json_each(c.bytes)`,
/// views over events) or parsed as a document, where an external row's empty `bytes` would fail
/// or, worse, read as empty, so those stay inline whatever their size.
pub(crate) fn spillable_kind(kind: &str) -> bool {
    matches!(
        kind,
        "component" | "rust-artifact" | "blob" | "kernel-blob" | "uploaded_file"
    )
}
/// Object files whose content was already verified in this process.
const VERIFIED_HASHES: usize = 4096;
/// A temp file older than this belongs to a writer that died. Younger ones may
/// belong to another process using the same store directory, so they stay.
const STALE_TEMP: Duration = Duration::from_secs(60 * 60);
const CHUNK_BYTES: usize = 1 << 20;
const SHARDS: &str = "0123456789abcdef";

static SIBLING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// What identifies the file a verification was done on. Any write through any
/// link, a replacement, or a metadata change alters at least one field, so a
/// stamp that still matches means the verified bytes are the bytes on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stamp {
    inode: u64,
    size: u64,
    mtime_ns: i128,
    ctime_ns: i128,
}

impl Stamp {
    /// The stamp of the open file, read from its handle (not from a path).
    pub(crate) fn of(file: &File) -> io::Result<Self> {
        let metadata = file.metadata()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let nanos = |seconds: i64, nanoseconds: i64| {
                i128::from(seconds) * 1_000_000_000 + i128::from(nanoseconds)
            };
            Ok(Self {
                inode: metadata.ino(),
                size: metadata.len(),
                mtime_ns: nanos(metadata.mtime(), metadata.mtime_nsec()),
                ctime_ns: nanos(metadata.ctime(), metadata.ctime_nsec()),
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                inode: 0,
                size: metadata.len(),
                mtime_ns: metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |elapsed| elapsed.as_nanos() as i128),
                ctime_ns: 0,
            })
        }
    }
}

#[derive(Default)]
struct Verified {
    stamps: HashMap<String, Stamp>,
    order: VecDeque<String>,
}

pub(crate) struct Spill {
    root: PathBuf,
    sequence: AtomicU64,
    /// Directories to sync at the next `sync_dirs`: the object roots, and the shard of every
    /// file a write found already in place (see the module docs).
    dirty: Mutex<BTreeSet<PathBuf>>,
    verified: Mutex<Verified>,
}

impl Spill {
    /// Own `<directory>/objects`, creating it and sweeping stale temp files.
    pub fn open(directory: &Path) -> Result<Self> {
        let root = directory.join("objects");
        let create = |path: PathBuf| {
            fs::create_dir_all(&path)
                .with_context(|| format!("create object directory {}", path.display()))
        };
        create(root.join("tmp"))?;
        for shard in SHARDS.chars() {
            create(root.join(shard.to_string()))?;
        }
        let spill = Self {
            dirty: Mutex::new(BTreeSet::from([root.clone(), directory.to_path_buf()])),
            root,
            sequence: AtomicU64::new(0),
            verified: Mutex::default(),
        };
        spill.sweep_temp()?;
        Ok(spill)
    }

    fn sweep_temp(&self) -> Result<()> {
        let temp = self.root.join("tmp");
        for entry in fs::read_dir(&temp)? {
            let entry = entry?;
            // Another process using this directory may rename its temp file away at any moment.
            let modified = match entry.metadata().and_then(|metadata| metadata.modified()) {
                Ok(modified) => modified,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            let age = modified.elapsed().unwrap_or(Duration::ZERO);
            if age < STALE_TEMP {
                continue;
            }
            match fs::remove_file(entry.path()) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(anyhow::Error::from(error)
                        .context(format!("sweep stale temp file {}", entry.path().display())));
                }
            }
        }
        Ok(())
    }

    /// `objects/<first hex char>/<hash>`; the hash must be canonical lowercase hex.
    pub fn path(&self, hash: &str) -> Result<PathBuf> {
        ensure!(
            hash.len() == 64 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
            "spilled object hash must be 64 lowercase hexadecimal characters"
        );
        Ok(self.root.join(&hash[..1]).join(hash))
    }

    /// Whether the object file exists with exactly `size` bytes. Metadata only.
    pub fn has(&self, hash: &str, size: u64) -> Result<bool> {
        match fs::metadata(self.path(hash)?) {
            Ok(metadata) => Ok(metadata.len() == size),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Whether the object file exists with `size` bytes that hash to `hash`: a
    /// file this process has not verified is hashed now. False means the file
    /// must be (re)written. A present file's shard is marked for a directory
    /// sync, since its writer may not have completed one.
    fn intact(&self, hash: &str, size: u64) -> Result<bool> {
        let path = self.path(hash)?;
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if file.metadata()?.len() != size || !self.verify(hash, &file)? {
            return Ok(false);
        }
        self.mark_dirty(path.parent().context("object path has no shard")?);
        Ok(true)
    }

    /// Write `bytes` as the object `hash` unless an intact file is already there.
    pub fn ensure(&self, hash: &str, bytes: &[u8]) -> Result<()> {
        if self.intact(hash, bytes.len() as u64)? {
            return Ok(());
        }
        self.write_with(hash, |file| Ok(file.write_all(bytes)?))
    }

    /// Stream `length` bytes of `input` into the object `hash`, hashing while
    /// copying, unless an intact file is already there.
    pub fn ingest(&self, hash: &str, input: &mut impl Read, length: u64) -> Result<()> {
        if self.intact(hash, length)? {
            return Ok(());
        }
        self.write_stream(hash, input, length)
    }

    fn write_stream(&self, hash: &str, input: &mut impl Read, length: u64) -> Result<()> {
        self.write_with(hash, |file| {
            let mut hasher = blake3::Hasher::new();
            let mut buffer = vec![0; CHUNK_BYTES];
            let mut total = 0u64;
            loop {
                let n = input.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                file.write_all(&buffer[..n])?;
                hasher.update(&buffer[..n]);
                total += n as u64;
            }
            ensure!(
                total == length && hasher.finalize().to_hex().as_str() == hash,
                "CAS source is corrupt or changed during import"
            );
            Ok(())
        })
    }

    /// Temp file (exclusive), `fill`, `sync_all`, read-only, rename, sync the
    /// shard directory. The caller indexes afterwards, so a durable row implies
    /// a durable file.
    fn write_with(&self, hash: &str, fill: impl FnOnce(&mut File) -> Result<()>) -> Result<()> {
        let destination = self.path(hash)?;
        let shard = destination.parent().context("object path has no shard")?;
        let temp = self.temp_path(hash);
        let mut file =
            create_new(&temp).with_context(|| format!("create temp object for {hash}"))?;
        let outcome = (|| -> Result<()> {
            fill(&mut file)?;
            file.sync_all()?;
            let mut permissions = file.metadata()?.permissions();
            permissions.set_readonly(true);
            file.set_permissions(permissions)?;
            fs::rename(&temp, &destination).with_context(|| format!("place object {hash}"))?;
            Ok(())
        })();
        if outcome.is_err() {
            // The original error is the one to report; a leftover temp is swept later.
            let _ = fs::remove_file(&temp);
        }
        outcome?;
        sync_directory(shard)
    }

    fn temp_path(&self, hash: &str) -> PathBuf {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        self.root.join("tmp").join(format!(
            "{}-{nanos}-{sequence}-{}",
            std::process::id(),
            &hash[..12]
        ))
    }

    fn mark_dirty(&self, directory: &Path) {
        // A poisoned set only ever loses dedup, never entries; keep going.
        let mut dirty = self.dirty.lock().unwrap_or_else(|e| e.into_inner());
        dirty.insert(directory.to_path_buf());
    }

    /// Sync the directories marked since the last call. Called at the store's durability barrier,
    /// before the WAL checkpoint that persists the rows. Writes sync their own shard directory.
    pub fn sync_dirs(&self) -> Result<()> {
        let directories =
            std::mem::take(&mut *self.dirty.lock().unwrap_or_else(|e| e.into_inner()));
        for directory in &directories {
            if let Err(error) = sync_directory(directory) {
                // Keep the unsynced ones for the next barrier.
                let mut dirty = self.dirty.lock().unwrap_or_else(|e| e.into_inner());
                dirty.extend(directories.iter().cloned());
                return Err(error);
            }
        }
        Ok(())
    }

    /// Open the object file, checking it exists and has the recorded size.
    pub fn open_file(&self, hash: &str, size: u64) -> Result<File> {
        let path = self.path(hash)?;
        let file = File::open(&path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                anyhow!(
                    "spilled object {hash} is missing: the objects directory does not match the database"
                )
            } else {
                anyhow::Error::from(error).context(format!("open spilled object {hash}"))
            }
        })?;
        let length = file.metadata()?.len();
        ensure!(
            length == size,
            "spilled object {hash} has {length} bytes on disk but the index records {size}"
        );
        Ok(file)
    }

    /// The whole object. Its BLAKE3 is checked unless this process already
    /// verified this very file (same stamp).
    pub fn read(&self, hash: &str, size: u64) -> Result<Vec<u8>> {
        let mut file = self.open_file(hash, size)?;
        let before = Stamp::of(&file)?;
        let mut bytes = Vec::with_capacity(usize::try_from(size)?);
        file.read_to_end(&mut bytes)
            .with_context(|| format!("read spilled object {hash}"))?;
        ensure!(
            bytes.len() as u64 == size && Stamp::of(&file)? == before,
            "spilled object {hash} changed while being read"
        );
        if !self.is_verified(hash, &before) {
            ensure!(
                blake3::hash(&bytes).to_hex().as_str() == hash,
                "CAS content hash mismatch"
            );
            self.mark_verified(hash, before);
        }
        Ok(bytes)
    }

    /// The first `limit` bytes, unverified: a preview cannot prove the whole hash.
    pub fn read_prefix(&self, hash: &str, size: u64, limit: u64) -> Result<Vec<u8>> {
        let file = self.open_file(hash, size)?;
        let mut bytes = Vec::new();
        file.take(limit)
            .read_to_end(&mut bytes)
            .with_context(|| format!("read spilled object {hash}"))?;
        Ok(bytes)
    }

    /// Make `destination` hold the object, replacing whatever is there
    /// atomically: clone (macOS), else copy, into a temp name beside
    /// `destination` (created exclusively), then rename. The result is an
    /// independent 0644 file that shares no inode with the store, so a consumer
    /// may write to it. The object is hash-verified first unless this process
    /// already verified this very file.
    pub fn restore(&self, hash: &str, size: u64, destination: &Path) -> Result<()> {
        let source = self.path(hash)?;
        let file = self.open_file(hash, size)?;
        ensure!(self.verify(hash, &file)?, "CAS content hash mismatch");
        let temp = sibling_temp(destination)?;
        let placed = match clone_file(&source, &temp) {
            Ok(()) => make_user_writable(&temp),
            Err(_) => copy_new(&file, &temp, size),
        };
        let outcome = placed
            .with_context(|| format!("place {hash} at {}", temp.display()))
            .and_then(|()| {
                fs::rename(&temp, destination)
                    .with_context(|| format!("replace {}", destination.display()))
            });
        if outcome.is_err() {
            let _ = fs::remove_file(&temp);
        }
        outcome
    }

    /// Bring the object `hash` of another store's directory into this one as a
    /// hashed copy (refused when the source does not hash to `hash`), so the two
    /// directories never share an inode. Nothing to do when an intact file is
    /// already here, which is the case when both stores share a directory.
    pub fn adopt(&self, from: &Spill, hash: &str, size: u64) -> Result<()> {
        if self.intact(hash, size)? {
            return Ok(());
        }
        let mut source = from.open_file(hash, size)?;
        self.write_stream(hash, &mut source, size)
    }

    fn is_verified(&self, hash: &str, stamp: &Stamp) -> bool {
        self.verified
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .stamps
            .get(hash)
            == Some(stamp)
    }

    /// Record that the file stamped `stamp` hashed to `hash`. A file that changed since (a
    /// different stamp on the handle now) is not recorded.
    pub(crate) fn mark_file_verified(&self, hash: &str, file: &File, stamp: Stamp) -> Result<()> {
        if Stamp::of(file)? == stamp {
            self.mark_verified(hash, stamp);
        }
        Ok(())
    }

    fn mark_verified(&self, hash: &str, stamp: Stamp) {
        let mut verified = self.verified.lock().unwrap_or_else(|e| e.into_inner());
        if verified.stamps.insert(hash.to_owned(), stamp).is_none() {
            verified.order.push_back(hash.to_owned());
        }
        while verified.order.len() > VERIFIED_HASHES {
            if let Some(oldest) = verified.order.pop_front() {
                verified.stamps.remove(&oldest);
            }
        }
    }

    /// Whether the content of `file`, a fresh handle on the object `hash`, hashes
    /// to `hash`. Answered from the verified stamp when this very file was
    /// verified before; otherwise hashed, and recorded when it matches. The file
    /// changing while it is hashed is an error.
    fn verify(&self, hash: &str, file: &File) -> Result<bool> {
        let before = Stamp::of(file)?;
        if self.is_verified(hash, &before) {
            return Ok(true);
        }
        let mut hasher = blake3::Hasher::new();
        hasher
            .update_reader(file)
            .with_context(|| format!("hash spilled object {hash}"))?;
        ensure!(
            Stamp::of(file)? == before,
            "spilled object {hash} changed while being verified"
        );
        let matches = hasher.finalize().to_hex().as_str() == hash;
        if matches {
            self.mark_verified(hash, before);
        }
        Ok(matches)
    }
}

/// Copy the `size` bytes of `file` from the start into a new 0644 file at `destination`.
fn copy_new(file: &File, destination: &Path, size: u64) -> io::Result<()> {
    let mut input = file;
    input.seek(SeekFrom::Start(0))?;
    let mut output = create_new(destination)?;
    let copied = io::copy(&mut input, &mut output)?;
    if copied != size {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "spilled object changed while being copied",
        ));
    }
    Ok(())
}

/// A new file that must not exist (`O_EXCL`: a planted file or symlink makes this fail), 0644.
fn create_new(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o644);
    }
    options.open(path)
}

/// Clones keep the store file's read-only mode; a restored file is the consumer's own.
fn make_user_writable(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o644))
    }
    #[cfg(not(unix))]
    {
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_readonly(false);
        fs::set_permissions(path, permissions)
    }
}

/// Write `bytes` to a new 0644 file at `destination` through an exclusive temp name beside it,
/// then rename.
pub(crate) fn write_atomic(destination: &Path, bytes: &[u8]) -> Result<()> {
    let temp = sibling_temp(destination)?;
    let outcome = create_new(&temp)
        .and_then(|mut file| file.write_all(bytes))
        .with_context(|| format!("write {}", temp.display()))
        .and_then(|()| {
            fs::rename(&temp, destination)
                .with_context(|| format!("replace {}", destination.display()))
        });
    if outcome.is_err() {
        let _ = fs::remove_file(&temp);
    }
    outcome
}

/// A unique name in the same directory as `destination`, so a rename stays on one filesystem.
fn sibling_temp(destination: &Path) -> Result<PathBuf> {
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = destination
        .file_name()
        .context("destination has no file name")?;
    let sequence = SIBLING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    Ok(parent.join(format!(
        ".{}.{}-{sequence}.tmp",
        name.to_string_lossy(),
        std::process::id()
    )))
}

#[cfg(unix)]
fn sync_directory(directory: &Path) -> Result<()> {
    File::open(directory)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("sync directory {}", directory.display()))
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Path) -> Result<()> {
    Ok(())
}

/// `clonefile(2)`: a copy-on-write copy on APFS, atomic, fails if `destination` exists.
#[cfg(target_os = "macos")]
fn clone_file(source: &Path, destination: &Path) -> io::Result<()> {
    use std::{
        ffi::{CString, c_char, c_int},
        os::unix::ffi::OsStrExt,
    };
    unsafe extern "C" {
        fn clonefile(source: *const c_char, destination: *const c_char, flags: u32) -> c_int;
    }
    let to_c = |path: &Path| {
        CString::new(path.as_os_str().as_bytes())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
    };
    let (source, destination) = (to_c(source)?, to_c(destination)?);
    // SAFETY: both arguments are NUL-terminated C strings that outlive the call.
    if unsafe { clonefile(source.as_ptr(), destination.as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "macos"))]
fn clone_file(_source: &Path, _destination: &Path) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}
