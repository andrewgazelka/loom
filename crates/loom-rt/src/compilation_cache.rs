//! Native compiler output is trusted local data, addressed separately from its input key.
use anyhow::{Context, Result, ensure};
use loom_store::Store;
use rusqlite::{OptionalExtension, params};
use std::{borrow::Cow, sync::Mutex};
use wasmtime::{CacheStore, Config};

/// Candidate hits count CAS reads; Cranelift can still reject a serialized value.
#[derive(Clone, Debug, Default)]
pub struct CompilationCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub inserts: u64,
    pub errors: u64,
    pub storage_errors: u64,
    pub missing_blobs: u64,
    pub corrupt_blobs: u64,
    pub conflicts: u64,
    pub last_error: Option<String>,
}

/// A persistent function cache used by the runtime's core Wasm engine.
/// Index rows retain CAS blobs until `clear` removes this backend's mappings.
pub struct LoomCompilationCache {
    store: Store,
    namespace: String,
    stats: Mutex<CompilationCacheStats>,
}

impl std::fmt::Debug for LoomCompilationCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoomCompilationCache")
            .field("namespace", &self.namespace)
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
enum CacheFailure {
    Missing(String),
    Corrupt(String),
    Conflict(String),
}
impl std::fmt::Display for CacheFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(hash) => write!(f, "missing cache blob {hash}"),
            Self::Corrupt(hash) => write!(f, "corrupt cache blob {hash}"),
            Self::Conflict(hash) => write!(f, "conflicting cache blob {hash}"),
        }
    }
}
impl std::error::Error for CacheFailure {}

/// Wasmtime states engine compatibility as a `std::hash::Hash`; BLAKE3 turns it
/// into the wide digest a namespace column wants. `finish` is never the stored
/// value: the namespace is the whole digest, read from the hasher directly.
struct NamespaceHasher(blake3::Hasher);

impl std::hash::Hasher for NamespaceHasher {
    fn write(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }
    fn finish(&self) -> u64 {
        let digest = self.0.finalize();
        let (head, _) = digest.as_bytes().split_at(8);
        u64::from_le_bytes(head.try_into().expect("a BLAKE3 digest is 32 bytes"))
    }
}

/// Serialized Cranelift output is only valid for an engine that would compile it
/// the same way, which is exactly what `Engine::precompile_compatibility_hash`
/// covers: the Wasmtime version, the target triple, and the compiler's flags and
/// tunables. It is reachable only from an `Engine`, while the cache has to exist
/// before a configuration can install it, so the namespace comes from a probe
/// engine built from that same configuration without the cache; a cache store is
/// not one of the hashed inputs. The probe costs one extra `Engine::new` per
/// cache, once per runtime.
///
/// It reads Wasmtime's version STRING, not its sources, and Cranelift's own
/// `VersionMarker` is a version string too: a local patch to either that keeps
/// the version is invisible to both, and stale machine code would be replayed.
/// Patch them only with a version bump.
fn namespace(config: &Config) -> Result<String> {
    use std::hash::Hash;
    let engine = crate::wasm_engine::create(config)
        .context("probe the compilation backend for its cache namespace")?;
    let mut hasher = NamespaceHasher(blake3::Hasher::new());
    engine.precompile_compatibility_hash().hash(&mut hasher);
    Ok(hasher.0.finalize().to_hex().to_string())
}

impl LoomCompilationCache {
    /// `config` is the engine configuration this cache will serve, minus the cache
    /// itself; it names the backend whose output the mappings may be replayed to.
    pub fn new(store: Store, config: &Config) -> Result<Self> {
        store
            .with_connection(|connection| {
                connection.execute_batch(
                    "CREATE TABLE IF NOT EXISTS runtime_compilation_cache (
                    backend_namespace TEXT NOT NULL,
                    compiler_key BLOB NOT NULL,
                    blob_hash TEXT NOT NULL REFERENCES cas(hash) ON DELETE RESTRICT,
                    PRIMARY KEY (backend_namespace, compiler_key)
                ) WITHOUT ROWID",
                )?;
                Ok(())
            })
            .context("initialize runtime_compilation_cache")?;
        Ok(Self {
            store,
            namespace: namespace(config)?,
            stats: Mutex::new(CompilationCacheStats::default()),
        })
    }

    pub fn stats(&self) -> CompilationCacheStats {
        self.stats.lock().unwrap().clone()
    }

    /// Expire this backend's index entries before their CAS blobs may be collected.
    /// Concurrent compilation may publish new entries after this operation.
    pub fn clear(&self) -> Result<usize> {
        self.store
            .with_connection(|connection| {
                Ok(connection.execute(
                    "DELETE FROM runtime_compilation_cache WHERE backend_namespace=?",
                    [self.namespace.as_str()],
                )?)
            })
            .context("expire runtime_compilation_cache")
    }

    /// Wasmtime's cache trait cannot return storage errors. Observe the complete
    /// compile window, including parallel workers, and make failures retryable.
    /// Overlapping compiles conservatively share failures; a later retry starts
    /// a fresh window and can succeed after the underlying store is repaired.
    pub(crate) fn compile<T>(&self, compile: impl FnOnce() -> Result<T>) -> Result<T> {
        let before = self.stats().errors;
        let output = compile();
        let after = self.stats();
        ensure!(
            after.errors == before,
            "native compilation cache: {}",
            after
                .last_error
                .as_deref()
                .unwrap_or("missing cache diagnostic")
        );
        output
    }

    fn lookup(&self, key: &[u8]) -> Result<Option<String>> {
        self.store.with_connection(|connection| {
            Ok(connection.query_row(
                "SELECT blob_hash FROM runtime_compilation_cache WHERE backend_namespace=? AND compiler_key=?",
                params![self.namespace, key],
                |row| row.get(0),
            ).optional()?)
        })
    }

    fn read_blob(&self, hash: &str) -> Result<Vec<u8>> {
        let bytes = self
            .store
            .get(hash)?
            .ok_or_else(|| CacheFailure::Missing(hash.into()))?;
        if blake3::hash(&bytes).to_hex().as_str() != hash {
            return Err(CacheFailure::Corrupt(hash.into()).into());
        }
        Ok(bytes)
    }

    fn publish(&self, key: &[u8], bytes: &[u8]) -> Result<()> {
        // Never replace an existing mapping, even when its bytes are corrupt.
        // Equivalent concurrent publishers converge on the same content hash.
        if let Some(hash) = self.lookup(key)? {
            if self.read_blob(&hash)? != bytes {
                return Err(CacheFailure::Conflict(hash).into());
            }
            return Ok(());
        }
        let hash = self.store.put("cranelift-function", bytes)?;
        self.store.with_connection(|connection| {
            connection.execute(
                "INSERT INTO runtime_compilation_cache VALUES (?,?,?) ON CONFLICT DO NOTHING",
                params![self.namespace, key, hash],
            )?;
            Ok(())
        })?;
        let published = self
            .lookup(key)?
            .context("cache mapping disappeared during publication")?;
        if self.read_blob(&published)? != bytes {
            return Err(CacheFailure::Conflict(published).into());
        }
        Ok(())
    }

    fn failure(&self, operation: &str, key: &[u8], error: anyhow::Error) {
        let mut stats = self.stats.lock().unwrap();
        stats.errors += 1;
        match error.downcast_ref::<CacheFailure>() {
            Some(CacheFailure::Missing(_)) => stats.missing_blobs += 1,
            Some(CacheFailure::Corrupt(_)) => stats.corrupt_blobs += 1,
            Some(CacheFailure::Conflict(_)) => stats.conflicts += 1,
            None => stats.storage_errors += 1,
        }
        stats.last_error = Some(format!(
            "{operation} backend {} key {key:02x?}: {error:#}",
            self.namespace
        ));
    }
}

impl CacheStore for LoomCompilationCache {
    fn get(&self, key: &[u8]) -> Option<Cow<'_, [u8]>> {
        match self
            .lookup(key)
            .and_then(|hash| hash.map(|hash| self.read_blob(&hash)).transpose())
        {
            Ok(Some(bytes)) => {
                self.stats.lock().unwrap().hits += 1;
                Some(Cow::Owned(bytes))
            }
            Ok(None) => {
                self.stats.lock().unwrap().misses += 1;
                None
            }
            Err(error) => {
                self.failure("get", key, error);
                None
            }
        }
    }

    fn insert(&self, key: &[u8], value: Vec<u8>) -> bool {
        match self.publish(key, &value) {
            Ok(()) => {
                self.stats.lock().unwrap().inserts += 1;
                true
            }
            Err(error) => {
                self.failure("insert", key, error);
                false
            }
        }
    }
}

#[cfg(test)]
mod tests;
