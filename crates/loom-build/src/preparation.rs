//! Resolver results are immutable CAS overlays shared by successive definitions.
use crate::{BuildError, registry::CrateRegistry};
use loom_check::SourceFile;
use loom_store::Store;
use std::{collections::BTreeMap, path::Path};

/// Source-bundle key whose text is the BLAKE3 hash of a vendored crate tree.
pub const VENDOR_TREE: &str = "loom.vendor-tree";

fn rejected(error: impl std::fmt::Display) -> BuildError {
    BuildError::Rejected(error.to_string())
}

pub(crate) fn overlay_hash(store: &Store, key: &str) -> Result<Option<String>, BuildError> {
    store.with_connection(|connection| {
        connection.execute_batch("CREATE TABLE IF NOT EXISTS rust_preparations (key TEXT PRIMARY KEY, overlay_hash TEXT NOT NULL)")?;
        let mut statement = connection.prepare("SELECT overlay_hash FROM rust_preparations WHERE key=?")?;
        let mut rows = statement.query_map([key], |row| row.get::<_, String>(0))?;
        Ok(rows.next().transpose()?)
    }).map_err(rejected)
}

pub(crate) fn load(
    store: &Store,
    key: &str,
) -> Result<Option<BTreeMap<String, SourceFile>>, BuildError> {
    let Some(hash) = overlay_hash(store, key)? else {
        return Ok(None);
    };
    store.get_value(&hash).map_err(rejected)
}

pub(crate) fn save(
    store: &Store,
    key: &str,
    files: &BTreeMap<String, SourceFile>,
) -> Result<(), BuildError> {
    let overlay: BTreeMap<_, _> = files
        .iter()
        .filter(|entry| ["Cargo.lock", VENDOR_TREE].contains(&entry.0.as_str()))
        .map(|entry| (entry.0.clone(), entry.1.clone()))
        .collect();
    let hash = store
        .put_value("rust-prepared-dependencies", &overlay)
        .map_err(rejected)?;
    store.with_connection(|connection| {
        connection.execute_batch("CREATE TABLE IF NOT EXISTS rust_preparations (key TEXT PRIMARY KEY, overlay_hash TEXT NOT NULL)")?;
        connection.execute("INSERT INTO rust_preparations VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET overlay_hash=excluded.overlay_hash", [key, &hash])?;
        Ok(())
    }).map_err(rejected)
}

pub(crate) fn materialize_tree(
    store: &Store,
    cache: &Path,
    destination: &Path,
    hash: &str,
) -> Result<(), BuildError> {
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(rejected("invalid source tree hash"));
    }
    let trees = cache.join("source-trees");
    std::fs::create_dir_all(&trees)?;
    let tree = trees.join(hash);
    if !tree.exists() {
        let temporary = trees.join(format!("{hash}.pending-{}", std::process::id()));
        if temporary.exists() {
            std::fs::remove_dir_all(&temporary)?;
        }
        CrateRegistry::new(store.clone())
            .materialize(hash, &temporary)
            .map_err(rejected)?;
        std::fs::rename(temporary, &tree)?;
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Ok(metadata) = std::fs::symlink_metadata(destination) {
        if metadata.file_type().is_symlink() {
            std::fs::remove_file(destination)?;
        } else {
            std::fs::remove_dir_all(destination)?;
        }
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(std::fs::canonicalize(tree)?, destination)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        Err(rejected(
            "Rust build worker requires Unix source-tree links",
        ))
    }
}
