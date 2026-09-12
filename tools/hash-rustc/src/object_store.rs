//! Immutable objects. Metadata is published last and acts as the commit marker.
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

pub struct Store {
    directory: PathBuf,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub key: String,
    pub object: PathBuf,
    pub symbols: Vec<String>,
    pub bytes: u64,
}

#[derive(Serialize, Deserialize)]
struct Metadata {
    key: String,
    item_count: usize,
    bytes: u64,
    digest: String,
    symbols: Vec<String>,
}

#[derive(Serialize)]
struct IndexLine<'a> {
    key: &'a str,
    item_count: usize,
    bytes: u64,
    created: u64,
}

struct TemporaryFile {
    path: PathBuf,
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

impl Store {
    pub fn new(directory: PathBuf) -> io::Result<Self> {
        crate::cache_metrics::store_call();
        fs::create_dir_all(&directory)?;
        Ok(Self { directory })
    }

    fn validate_key(key: &str) -> io::Result<()> {
        if key.len() != 64
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(invalid(
                "object cache key must be a lowercase BLAKE3 hex digest",
            ));
        }
        Ok(())
    }

    pub fn lookup(&self, key: &str) -> io::Result<Option<Entry>> {
        crate::cache_metrics::lookup_call();
        let _timer = crate::cache_metrics::Timer::start(crate::cache_metrics::Phase::Lookup);
        Self::validate_key(key)?;
        let metadata_bytes = match fs::read(self.directory.join(format!("{key}.json"))) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let metadata: Metadata = serde_json::from_slice(&metadata_bytes).map_err(invalid_json)?;
        if metadata.key != key {
            return Err(invalid(format!(
                "object cache metadata key mismatch: {key}"
            )));
        }
        let object = self.directory.join(format!("{key}.o"));
        let bytes = fs::read(&object)?;
        if bytes.len() as u64 != metadata.bytes
            || blake3::hash(&bytes).to_hex().as_str() != metadata.digest
        {
            return Err(invalid(format!(
                "object cache object digest/size mismatch: {key}"
            )));
        }
        Ok(Some(Entry {
            key: key.to_owned(),
            object,
            symbols: metadata.symbols,
            bytes: metadata.bytes,
        }))
    }

    pub fn publish(
        &self,
        key: &str,
        item_count: usize,
        object: &Path,
        symbols: &[String],
    ) -> io::Result<Entry> {
        crate::cache_metrics::publish_call();
        Self::validate_key(key)?;
        let bytes = fs::read(object)?;
        if bytes.is_empty() {
            return Err(invalid("refusing to publish an empty object"));
        }
        let metadata = Metadata {
            key: key.to_owned(),
            item_count,
            bytes: bytes.len() as u64,
            digest: blake3::hash(&bytes).to_hex().to_string(),
            symbols: symbols.to_vec(),
        };
        let temporary = self.write_temporary(key, &bytes)?;
        if std::env::var_os("LOOM_OBJECT_CACHE_FAIL_PUBLISH").is_some_and(|value| value == "1") {
            return Err(io::Error::other(
                "injected object cache publication failure before final object publication",
            ));
        }
        self.publish_immutable(&temporary, &self.directory.join(format!("{key}.o")), &bytes)?;

        let index = IndexLine {
            key,
            item_count,
            bytes: metadata.bytes,
            created: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_secs(),
        };
        let mut index_bytes = serde_json::to_vec(&index).map_err(invalid_json)?;
        index_bytes.push(b'\n');
        let index_temporary = self.write_temporary(key, &index_bytes)?;
        // The first writer owns the timestamp; every writer has already checked the object bytes.
        match fs::hard_link(
            &index_temporary.path,
            self.directory.join(format!("{key}.index.jsonl")),
        ) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let metadata_bytes = serde_json::to_vec(&metadata).map_err(invalid_json)?;
        let metadata_temporary = self.write_temporary(key, &metadata_bytes)?;
        self.publish_immutable(
            &metadata_temporary,
            &self.directory.join(format!("{key}.json")),
            &metadata_bytes,
        )?;
        // Persist directory entries after all final names exist.
        fs::File::open(&self.directory)?.sync_all()?;
        self.lookup(key)?
            .ok_or_else(|| invalid("object cache metadata disappeared after publication"))
    }

    fn write_temporary(&self, key: &str, bytes: &[u8]) -> io::Result<TemporaryFile> {
        loop {
            let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path =
                self.directory
                    .join(format!(".{key}.{}.{}.tmp", std::process::id(), sequence));
            let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            };
            let temporary = TemporaryFile { path };
            file.write_all(bytes)?;
            file.sync_all()?;
            return Ok(temporary);
        }
    }

    fn publish_immutable(
        &self,
        temporary: &TemporaryFile,
        final_path: &Path,
        expected: &[u8],
    ) -> io::Result<()> {
        // rename() overwrites another writer's bytes. A same-filesystem hard link publishes
        // the complete inode atomically with no replacement and needs no crash-prone lock.
        match fs::hard_link(&temporary.path, final_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if fs::read(final_path)? == expected {
                    Ok(())
                } else {
                    Err(invalid(format!(
                        "object cache deterministic publication conflict: {}",
                        final_path.display()
                    )))
                }
            }
            Err(error) => Err(error),
        }
    }
}

fn invalid_json(error: serde_json::Error) -> io::Error {
    invalid(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_round_trip_and_corruption_rejection() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::new(directory.path().join("cache")).unwrap();
        let source = directory.path().join("input.o");
        fs::write(&source, b"complete object").unwrap();
        let key = blake3::hash(b"codegen unit").to_hex().to_string();
        assert!(store.lookup(&key).unwrap().is_none());
        let entry = store
            .publish(&key, 1, &source, &["symbol".to_owned()])
            .unwrap();
        assert_eq!(entry.key, key);
        assert_eq!(entry.symbols, ["symbol"]);
        assert_eq!(entry.bytes, 15);
        fs::write(entry.object, b"corrupt! object").unwrap();
        assert_eq!(
            store.lookup(&key).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn competing_writers_accept_identical_objects_and_reject_conflicts() {
        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join("cache");
        let source = directory.path().join("input.o");
        fs::write(&source, b"complete object").unwrap();
        let key = blake3::hash(b"codegen unit").to_hex().to_string();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    Store::new(cache.clone())
                        .unwrap()
                        .publish(&key, 1, &source, &[])
                        .unwrap();
                });
            }
        });
        fs::write(&source, b"different object").unwrap();
        let store = Store::new(cache).unwrap();
        assert_eq!(
            store.publish(&key, 1, &source, &[]).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            fs::read(store.lookup(&key).unwrap().unwrap().object).unwrap(),
            b"complete object"
        );
        assert_eq!(
            fs::read_to_string(store.directory.join(format!("{key}.index.jsonl")))
                .unwrap()
                .lines()
                .count(),
            1
        );
    }
}
