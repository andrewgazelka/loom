//! Raw blobs of `SPILL_BYTES` and up are immutable files, not SQLite rows.
//!
//! Layout under the store directory: `objects/<first hex char>/<64-hex hash>`
//! and `objects/tmp/`. A file is written to `objects/tmp/`, `sync_all`ed,
//! marked read-only and renamed into place; only then may the caller index it,
//! so an index row never names a missing file. An orphan file after a crash is
//! harmless. Directory syncs are not done per blob: `sync_dirs` runs at the
//! recording writer's durability barrier (`Store::flush`), before the WAL
//! checkpoint that makes the index rows durable.
//!
//! This module knows files only; the SQL side is in `blobs.rs`.
use anyhow::{Context, Result, anyhow, ensure};
use std::{
    collections::{BTreeSet, HashSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
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
    matches!(kind, "component" | "rust-artifact" | "blob" | "kernel-blob" | "uploaded_file")
}
/// Hashes whose file content was already verified in this process.
const VERIFIED_HASHES: usize = 4096;
/// A temp file older than this belongs to a writer that died. Younger ones may
/// belong to another process using the same store directory, so they stay.
const STALE_TEMP: Duration = Duration::from_secs(60 * 60);
const CHUNK_BYTES: usize = 1 << 20;
const SHARDS: &str = "0123456789abcdef";

static SIBLING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct Verified {
    hashes: HashSet<String>,
    order: VecDeque<String>,
}

pub(crate) struct Spill {
    root: PathBuf,
    sequence: AtomicU64,
    /// Directories whose entries changed since the last `sync_dirs`.
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
            let age = entry
                .metadata()?
                .modified()?
                .elapsed()
                .unwrap_or(Duration::ZERO);
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

    /// Whether the object file exists with exactly `size` bytes.
    pub fn has(&self, hash: &str, size: u64) -> Result<bool> {
        match fs::metadata(self.path(hash)?) {
            Ok(metadata) => Ok(metadata.len() == size),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Write `bytes` as the object `hash` unless a file of that size is already there.
    pub fn ensure(&self, hash: &str, bytes: &[u8]) -> Result<()> {
        if self.has(hash, bytes.len() as u64)? {
            return Ok(());
        }
        self.write_with(hash, |file| Ok(file.write_all(bytes)?))
    }

    /// Stream `length` bytes of `input` into the object `hash`, hashing while
    /// copying, unless a file of that size is already there.
    pub fn ingest(&self, hash: &str, input: &mut impl Read, length: u64) -> Result<()> {
        if self.has(hash, length)? {
            return Ok(());
        }
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
                "CAS source changed during import"
            );
            Ok(())
        })
    }

    /// Temp file, `fill`, `sync_all`, read-only, rename. The caller indexes afterwards.
    fn write_with(&self, hash: &str, fill: impl FnOnce(&mut File) -> Result<()>) -> Result<()> {
        let destination = self.path(hash)?;
        let temp = self.temp_path(hash);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .with_context(|| format!("create temp object {}", temp.display()))?;
        let outcome = (|| -> Result<()> {
            fill(&mut file)?;
            file.sync_all()?;
            let mut permissions = file.metadata()?.permissions();
            permissions.set_readonly(true);
            file.set_permissions(permissions)?;
            fs::rename(&temp, &destination)
                .with_context(|| format!("place object {}", destination.display()))?;
            Ok(())
        })();
        if outcome.is_err() {
            // The original error is the one to report; a leftover temp is swept later.
            let _ = fs::remove_file(&temp);
        }
        outcome?;
        self.mark_dirty(destination.parent().context("object path has no shard")?);
        Ok(())
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

    /// Make every rename since the last call durable. Called at the store's
    /// durability barrier, before the WAL checkpoint that persists the rows.
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
                    "spilled object {hash} is missing from {}: the objects directory does not match the database",
                    self.root.display()
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

    /// The whole object. Its BLAKE3 is checked the first time this process reads it.
    pub fn read(&self, hash: &str, size: u64) -> Result<Vec<u8>> {
        let mut file = self.open_file(hash, size)?;
        let mut bytes = Vec::with_capacity(usize::try_from(size)?);
        file.read_to_end(&mut bytes)
            .with_context(|| format!("read spilled object {hash}"))?;
        ensure!(
            bytes.len() as u64 == size,
            "spilled object {hash} changed while being read"
        );
        if !self.is_verified(hash) {
            ensure!(
                blake3::hash(&bytes).to_hex().as_str() == hash,
                "CAS content hash mismatch"
            );
            self.mark_verified(hash);
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
    /// atomically: clone (macOS), else hard link, else copy, into a temp name
    /// beside `destination`, then rename. The result may share storage with
    /// the store, so it is read-only and callers must not write through it.
    pub fn restore(&self, hash: &str, size: u64, destination: &Path) -> Result<()> {
        let source = self.path(hash)?;
        drop(self.open_file(hash, size)?);
        self.verify_file_once(hash, &source)?;
        let temp = sibling_temp(destination)?;
        let placed = clone_file(&source, &temp)
            .or_else(|_| fs::hard_link(&source, &temp))
            .or_else(|_| fs::copy(&source, &temp).map(|_| ()));
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

    /// Bring the object `hash` of another store's directory into this one:
    /// hard link (no data copied), else a hashed copy. Nothing to do when the
    /// file is already here, which is the case when both stores share a directory.
    pub fn adopt(&self, from: &Spill, hash: &str, size: u64) -> Result<()> {
        if self.has(hash, size)? {
            return Ok(());
        }
        let mut source = from.open_file(hash, size)?;
        let destination = self.path(hash)?;
        match fs::hard_link(from.path(hash)?, &destination) {
            Ok(()) => {
                self.mark_dirty(destination.parent().context("object path has no shard")?);
                Ok(())
            }
            // Another filesystem, one without links, or a stale wrong-size file
            // in the way: stream a verified copy and rename it over.
            Err(_) => self.ingest(hash, &mut source, size),
        }
    }

    fn is_verified(&self, hash: &str) -> bool {
        self.verified
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .hashes
            .contains(hash)
    }

    pub fn mark_verified(&self, hash: &str) {
        let mut verified = self.verified.lock().unwrap_or_else(|e| e.into_inner());
        if !verified.hashes.insert(hash.to_owned()) {
            return;
        }
        verified.order.push_back(hash.to_owned());
        while verified.order.len() > VERIFIED_HASHES {
            if let Some(oldest) = verified.order.pop_front() {
                verified.hashes.remove(&oldest);
            }
        }
    }

    fn verify_file_once(&self, hash: &str, path: &Path) -> Result<()> {
        if self.is_verified(hash) {
            return Ok(());
        }
        let mut hasher = blake3::Hasher::new();
        hasher
            .update_reader(File::open(path)?)
            .with_context(|| format!("hash spilled object {hash}"))?;
        ensure!(
            hasher.finalize().to_hex().as_str() == hash,
            "CAS content hash mismatch"
        );
        self.mark_verified(hash);
        Ok(())
    }
}

/// Write `bytes` to `destination` through a temp name beside it, then rename.
pub(crate) fn write_atomic(destination: &Path, bytes: &[u8]) -> Result<()> {
    let temp = sibling_temp(destination)?;
    let outcome = fs::write(&temp, bytes)
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
