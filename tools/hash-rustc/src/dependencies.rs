//! Stored item hashes of Loom definition dependencies, for content-linked
//! cross-crate references.
//!
//! `LOOM_DEP_ITEMS=<directory>` names one item document per dependency crate:
//! `<directory>/<crate_name>.json` is the `items.json` the dependency's own
//! build recorded. A crate with a document here is a Loom definition; a
//! reference into it contributes the referent's stored item hash. A crate
//! without one (std, the SDK, crates.io) keeps the whole-crate rule in
//! `graph::crate_reference`. Unset means no crate is content-linked.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct Dependencies {
    crates: BTreeMap<String, Crate>,
}

struct Crate {
    document: PathBuf,
    items: BTreeMap<String, blake3::Hash>,
}

#[derive(serde::Deserialize)]
struct Document {
    items: BTreeMap<String, Item>,
}

#[derive(serde::Deserialize)]
struct Item {
    hash: String,
}

impl Dependencies {
    /// Read every `<crate_name>.json` under `LOOM_DEP_ITEMS`. Any other entry,
    /// an unreadable document, or a malformed hash is an error naming the file.
    pub fn load() -> Result<Self, String> {
        let Some(directory) = std::env::var_os("LOOM_DEP_ITEMS") else {
            return Ok(Self::default());
        };
        Self::read(Path::new(&directory))
    }

    fn read(directory: &Path) -> Result<Self, String> {
        let entries = std::fs::read_dir(directory)
            .map_err(|error| format!("LOOM_DEP_ITEMS {}: {error}", directory.display()))?;
        let mut crates = BTreeMap::new();
        for entry in entries {
            let path = entry
                .map_err(|error| format!("LOOM_DEP_ITEMS {}: {error}", directory.display()))?
                .path();
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_suffix(".json"))
                .ok_or_else(|| {
                    format!(
                        "LOOM_DEP_ITEMS entry {} is not <crate_name>.json",
                        path.display()
                    )
                })?
                .to_owned();
            crates.insert(name, Crate::read(path)?);
        }
        Ok(Self { crates })
    }

    /// The stored hash of `path` inside dependency crate `crate_name`.
    /// `Ok(None)` when the crate is not a Loom definition dependency. A crate
    /// that is one but never recorded `path` is an error: the SVH rule is not
    /// a fallback.
    pub fn hash(&self, crate_name: &str, path: &str) -> Result<Option<blake3::Hash>, String> {
        let Some(dependency) = self.crates.get(crate_name) else {
            return Ok(None);
        };
        dependency
            .items
            .get(path)
            .copied()
            .map(Some)
            .ok_or_else(|| {
                format!(
                    "dependency crate {crate_name} has no stored item {path} in {}",
                    dependency.document.display()
                )
            })
    }
}

impl Crate {
    fn read(document: PathBuf) -> Result<Self, String> {
        let bytes = std::fs::read(&document)
            .map_err(|error| format!("dependency items {}: {error}", document.display()))?;
        let parsed: Document = serde_json::from_slice(&bytes)
            .map_err(|error| format!("dependency items {}: {error}", document.display()))?;
        let mut items = BTreeMap::new();
        for (path, item) in parsed.items {
            let hash = blake3::Hash::from_hex(&item.hash).map_err(|error| {
                format!(
                    "dependency items {}: item {path}: {error}",
                    document.display()
                )
            })?;
            items.insert(path, hash);
        }
        Ok(Self { document, items })
    }
}
