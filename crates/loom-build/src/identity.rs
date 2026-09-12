//! Compiler identity and verified ingestion of hash-rustc side outputs.
use crate::BuildError;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use tokio::process::Command;

#[derive(serde::Deserialize)]
struct Document {
    toolchain: String,
    items: BTreeMap<String, Item>,
    entry: BTreeMap<String, String>,
}
#[derive(serde::Deserialize)]
struct Item {
    hash: String,
    refs: Vec<String>,
    cycle: Option<Vec<String>>,
}

pub(crate) struct Driver {
    pub path: PathBuf,
    pub toolchain_hash: String,
}
fn rejected(error: impl std::fmt::Display) -> BuildError {
    BuildError::Rejected(error.to_string())
}
impl Driver {
    #[cfg(test)]
    pub async fn prepare(root: &Path, cache: &Path) -> Result<Self, BuildError> {
        Self::prepare_with_path(root, cache, None).await
    }
    pub async fn prepare_with_path(
        root: &Path,
        cache: &Path,
        selected: Option<&Path>,
    ) -> Result<Self, BuildError> {
        if let Some(path) = selected {
            let probe = Command::new(path)
                .arg("-vV")
                .output()
                .await
                .map_err(|error| {
                    rejected(format!(
                        "hash-rustc driver unavailable: {}: {error}",
                        path.display()
                    ))
                })?;
            if !probe.status.success() {
                return Err(rejected(format!(
                    "hash-rustc driver unavailable: {}: {}",
                    path.display(),
                    String::from_utf8_lossy(&probe.stderr)
                )));
            }
        }
        let source = root.join("tools/hash-rustc");
        let manifest = source.join("Cargo.toml");
        if selected.is_none() && !manifest.is_file() {
            return Err(rejected(format!(
                "hash-rustc driver unavailable: {}",
                manifest.display()
            )));
        }
        let toolchain = crate::resolve_guest_toolchain_with_driver(root, selected).await?;
        let guest_version = toolchain.version.as_bytes();
        let target = cache
            .join("hash-rustc")
            .join(blake3::hash(guest_version).to_hex().as_str());
        let path = selected
            .map(Path::to_owned)
            .unwrap_or_else(|| target.join("release/hash-rustc"));
        if selected.is_none() {
            // Cargo's freshness check covers changes in the independently pinned driver.
            let output = Command::new(&toolchain.cargo)
                .current_dir(&source)
                .env("RUSTC", toolchain.sysroot.join("bin/rustc"))
                .env_remove("RUSTC_WRAPPER")
                .env(
                    "RUSTUP_TOOLCHAIN",
                    toolchain
                        .channel
                        .as_deref()
                        .expect("source driver uses pinned toolchain"),
                )
                .env_remove("RUSTFLAGS")
                .env_remove("CARGO_ENCODED_RUSTFLAGS")
                .env("CARGO_TARGET_DIR", &target)
                .env("CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER", "/usr/bin/cc")
                .args(["build", "--release", "--locked", "--bin", "hash-rustc"])
                .output()
                .await
                .map_err(|error| {
                    rejected(format!("hash-rustc driver {}: {error}", path.display()))
                })?;
            if !output.status.success() || !path.is_file() {
                return Err(rejected(format!(
                    "hash-rustc driver unavailable: {}\n{}",
                    path.display(),
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
        }
        let path = std::fs::canonicalize(&path)?;
        let version = Command::new(&path)
            .arg("-vV")
            .output()
            .await
            .map_err(|error| rejected(format!("hash-rustc driver {}: {error}", path.display())))?;
        if !version.status.success() {
            return Err(rejected(format!(
                "hash-rustc driver {}: {}",
                path.display(),
                String::from_utf8_lossy(&version.stderr)
            )));
        }
        if guest_version != version.stdout {
            return Err(rejected(format!(
                "guest rustc is incompatible with hash-rustc driver {}: guest {}driver {}; select the matching guest compiler explicitly with RUSTC",
                path.display(),
                String::from_utf8_lossy(guest_version),
                String::from_utf8_lossy(&version.stdout)
            )));
        }
        let mut hasher = blake3::Hasher::new();
        hasher.update(guest_version);
        hasher.update(&version.stdout);
        // The compiler version alone cannot distinguish encoder revisions.
        hasher.update(blake3::hash(&std::fs::read(&path)?).as_bytes());
        Ok(Self {
            path,
            toolchain_hash: hasher.finalize().to_hex().to_string(),
        })
    }
    pub fn configure(&self, command: &mut Command, directory: &Path) {
        command
            .env("RUSTC", &self.path)
            .env("LOOM_ITEM_HASHES", directory.join("items.json"))
            .env("LOOM_ITEM_PREIMAGES", directory.join("item-preimages"));
    }
    pub fn ingest(
        &self,
        store: &loom_store::Store,
        directory: &Path,
        definition: &loom_check::CheckedDef,
        wasm: &[u8],
    ) -> Result<loom_proto::BuildIdentity, BuildError> {
        let path = directory.join("items.json");
        let bytes = std::fs::read(&path)
            .map_err(|error| rejected(format!("{}: {error}", path.display())))?;
        let document: Document = serde_json::from_slice(&bytes).map_err(rejected)?;
        if document.toolchain.is_empty() {
            return Err(rejected("hash-rustc document has no toolchain"));
        }
        if document.entry.is_empty() {
            return Err(rejected("definition has no entry export"));
        }
        for entry in &definition.sig.exports {
            if !document.entry.contains_key(&entry.name) {
                return Err(rejected(format!(
                    "hash-rustc document missing entry {}",
                    entry.name
                )));
            }
        }
        for (name, hash) in &document.entry {
            if document
                .items
                .get(name)
                .is_none_or(|item| &item.hash != hash)
            {
                return Err(rejected(format!(
                    "hash-rustc entry {name} disagrees with item table"
                )));
            }
        }
        let root_preimage = loom_proto::entry_identity_preimage(&document.entry);
        let behavior_hash = blake3::hash(&root_preimage).to_hex().to_string();
        let preimages = directory.join("item-preimages");
        for item in document.items.values() {
            let data = ingest_object(store, &preimages, &item.hash)?;
            if let Some(members) = &item.cycle {
                if members.is_empty() || data.len() != 40 {
                    return Err(rejected("invalid hash-rustc cycle preimage"));
                }
                let cycle = blake3::Hash::from_bytes(data[..32].try_into().map_err(rejected)?)
                    .to_hex()
                    .to_string();
                ingest_object(store, &preimages.join("cycles"), &cycle)?;
            }
            if item.refs.iter().any(String::is_empty) {
                return Err(rejected("empty hash-rustc item reference"));
            }
        }
        store.put("entry-root", &root_preimage).map_err(rejected)?;
        Ok(loom_proto::BuildIdentity {
            behavior_hash,
            wasm_hash: blake3::hash(wasm).to_hex().to_string(),
            toolchain_hash: self.toolchain_hash.clone(),
            item_hashes_ref: store.put("item-hashes", &bytes).map_err(rejected)?,
        })
    }
}
fn ingest_object(
    store: &loom_store::Store,
    directory: &Path,
    hash: &str,
) -> Result<Vec<u8>, BuildError> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(rejected(format!("invalid item hash {hash}")));
    }
    let path = directory.join(hash);
    let bytes =
        std::fs::read(&path).map_err(|error| rejected(format!("{}: {error}", path.display())))?;
    if blake3::hash(&bytes).to_hex().as_str() != hash {
        return Err(rejected(format!(
            "corrupt item preimage {}",
            path.display()
        )));
    }
    store.put("item-preimage", &bytes).map_err(rejected)?;
    Ok(bytes)
}

/// Move successful compiler side outputs out of the sandbox's writable target.
pub(crate) fn publish(source: &Path, destination: &Path) -> Result<(), BuildError> {
    fn copy_objects(source: &Path, destination: &Path) -> Result<(), BuildError> {
        std::fs::create_dir_all(destination)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            let target = destination.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                copy_objects(&entry.path(), &target)?;
            } else if entry.file_type()?.is_file() {
                let bytes = std::fs::read(entry.path())?;
                if target.exists() && std::fs::read(&target)? != bytes {
                    return Err(rejected(format!(
                        "conflicting item preimage {}",
                        target.display()
                    )));
                }
                std::fs::write(target, bytes)?;
            } else {
                return Err(rejected(format!(
                    "invalid item preimage {}",
                    entry.path().display()
                )));
            }
        }
        Ok(())
    }
    copy_objects(
        &source.join("item-preimages"),
        &destination.join("item-preimages"),
    )?;
    std::fs::copy(source.join("items.json"), destination.join("items.json"))?;
    Ok(())
}
