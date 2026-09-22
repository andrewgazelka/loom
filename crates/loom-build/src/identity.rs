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
    /// Root `pub fn` items: the wasm ABI surface.
    entry: BTreeMap<String, String>,
    /// Every definition reachable through `pub` visibility from the crate
    /// root: the identity surface. Entries are a subset.
    exports: BTreeMap<String, String>,
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

/// Directory under the identity directory holding one stored item document
/// per Loom definition dependency, named by rustc crate name.
const DEPENDENCY_ITEMS: &str = "dependency-items";

/// The rustc crate name Cargo gives a materialized definition dependency
/// (`materialize.rs` names the package `loom-definition-<hash16>`).
pub(crate) fn dependency_crate_name(hash: &str) -> String {
    format!("loom_definition_{}", &hash[..16])
}

/// Write each direct dependency's stored item document to
/// `<directory>/dependency-items/<crate_name>.json` for the driver's
/// `LOOM_DEP_ITEMS`. A dependency without a stored build identity or
/// document is an error naming it; nothing is written for a partial set.
pub(crate) fn stage_dependency_items(
    store: &loom_store::Store,
    definition: &loom_check::CheckedDef,
    directory: &Path,
) -> Result<(), BuildError> {
    let mut documents = Vec::new();
    for (alias, hash) in &definition.deps {
        let identity = store
            .build_identity(hash)
            .map_err(rejected)?
            .ok_or_else(|| {
                rejected(format!(
                    "dependency {alias} ({hash}) has no stored build identity"
                ))
            })?;
        let bytes = store
            .get(&identity.item_hashes_ref)
            .map_err(rejected)?
            .ok_or_else(|| {
                rejected(format!(
                    "dependency {alias} ({hash}): item document {} missing from CAS",
                    identity.item_hashes_ref
                ))
            })?;
        documents.push((dependency_crate_name(hash), bytes));
    }
    let staged = directory.join(DEPENDENCY_ITEMS);
    if staged.exists() {
        std::fs::remove_dir_all(&staged)?;
    }
    std::fs::create_dir_all(&staged)?;
    for (name, bytes) in documents {
        std::fs::write(staged.join(format!("{name}.json")), bytes)?;
    }
    Ok(())
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
            let mut build = Command::new(&toolchain.cargo);
            crate::direct::host_linker(&mut build);
            let output = build
                .current_dir(&source)
                .env("RUSTC", toolchain.sysroot.join("bin/rustc"))
                .env_remove("RUSTC_WRAPPER")
                .env_remove("RUSTC_WORKSPACE_WRAPPER")
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
            // The driver is a rustc plugin: it answers with the compiler it was
            // linked against, which must be the guest compiler byte for byte.
            return Err(rejected(format!(
                "hash-rustc driver {} is incompatible with the guest compiler{}: guest {}driver {}",
                path.display(),
                toolchain
                    .channel
                    .as_deref()
                    .map(|channel| format!(" pin {channel}"))
                    .unwrap_or_default(),
                String::from_utf8_lossy(guest_version),
                String::from_utf8_lossy(&version.stdout),
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
    /// The driver's side-output contract for a hashing compilation whose
    /// outputs live in `directory`: the item document, the preimage store,
    /// and the staged dependency documents from `stage_dependency_items`.
    pub fn environment(directory: &Path) -> [(String, String); 3] {
        let value = |path: PathBuf| path.to_string_lossy().into_owned();
        [
            ("LOOM_ITEM_HASHES".into(), value(directory.join("items.json"))),
            (
                "LOOM_ITEM_PREIMAGES".into(),
                value(directory.join("item-preimages")),
            ),
            (
                "LOOM_DEP_ITEMS".into(),
                value(directory.join(DEPENDENCY_ITEMS)),
            ),
        ]
    }
    pub fn configure(&self, command: &mut Command, directory: &Path) {
        command
            .env("RUSTC", &self.path)
            .envs(Self::environment(directory));
    }
    /// Verify the driver's document and preimages, import them into the CAS,
    /// and derive the definition identity: the Merkle root over the exported
    /// definitions' path/hash pairs.
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
        let document: Document = serde_json::from_slice(&bytes)
            .map_err(|error| rejected(format!("hash-rustc document {}: {error}", path.display())))?;
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
            if document.exports.get(name) != Some(hash) {
                return Err(rejected(format!(
                    "hash-rustc entry {name} is not among the exports"
                )));
            }
        }
        for (name, hash) in &document.exports {
            if document
                .items
                .get(name)
                .is_none_or(|item| &item.hash != hash)
            {
                return Err(rejected(format!(
                    "hash-rustc export {name} disagrees with item table"
                )));
            }
        }
        let root_preimage = loom_proto::export_identity_preimage(&document.exports);
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
        store.put("export-root", &root_preimage).map_err(rejected)?;
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
/// Staged dependency documents are build inputs, not outputs, and stay behind.
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
