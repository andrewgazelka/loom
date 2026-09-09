//! Network intake is explicit; builds consume the verified tree identity.
use anyhow::{Context, Result, ensure};
use loom_proto::{Tree, TreeEntry};
use loom_store::Store;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Component, Path},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrateInfo {
    pub name: String,
    pub version: String,
    pub hash: String,
    pub checksum: String,
    pub features_available: Vec<String>,
}
pub struct CrateRegistry {
    store: Store,
}
struct SourceEntry {
    bytes: Vec<u8>,
    executable: bool,
}
impl CrateRegistry {
    pub fn new(store: Store) -> Self {
        Self { store }
    }
    pub async fn add(&self, name: &str, version: &str) -> Result<CrateInfo> {
        validate_coordinate(name, version)?;
        let client = reqwest::Client::builder()
            .user_agent("loom-crate-intake/0.1")
            .build()?;
        let metadata: serde_json::Value = client
            .get(format!("https://crates.io/api/v1/crates/{name}/{version}"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let checksum = metadata["version"]["checksum"]
            .as_str()
            .context("registry checksum missing")?;
        let mut response = client
            .get(format!(
                "https://static.crates.io/crates/{name}/{name}-{version}.crate"
            ))
            .send()
            .await?
            .error_for_status()?;
        ensure!(
            response.content_length().unwrap_or(0) <= 64 * 1024 * 1024,
            "crate archive too large"
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes.len().saturating_add(chunk.len()) <= 64 * 1024 * 1024,
                "crate archive too large"
            );
            bytes.extend_from_slice(&chunk);
        }
        self.ingest(name, version, checksum, &bytes)
    }
    pub fn ingest(
        &self,
        name: &str,
        version: &str,
        checksum: &str,
        archive: &[u8],
    ) -> Result<CrateInfo> {
        validate_coordinate(name, version)?;
        ensure!(archive.len() <= 64 * 1024 * 1024, "crate archive too large");
        ensure!(
            format!("{:x}", Sha256::digest(archive)) == checksum,
            "registry checksum mismatch"
        );
        let decoder = flate2::read::GzDecoder::new(archive);
        let mut tar = tar::Archive::new(decoder);
        let mut files = BTreeMap::new();
        let prefix = format!("{name}-{version}");
        let mut total = 0_u64;
        for entry in tar.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            ensure!(
                path.components()
                    .all(|part| matches!(part, Component::Normal(_))),
                "unsafe crate path"
            );
            ensure!(
                path.components().count() <= 128,
                "crate tree depth exceeded"
            );
            let relative = path
                .strip_prefix(&prefix)
                .context("unexpected crate archive root")?;
            if entry.header().entry_type().is_dir() {
                continue;
            }
            ensure!(
                entry.header().entry_type().is_file(),
                "crate archive links and special files are forbidden"
            );
            let path = relative
                .to_str()
                .context("non UTF-8 crate path")?
                .to_owned();
            ensure!(
                !path.is_empty() && !path.contains('\\'),
                "invalid crate path"
            );
            total = total
                .checked_add(entry.size())
                .context("crate size overflow")?;
            ensure!(
                total <= 256 * 1024 * 1024 && files.len() < 100_000,
                "expanded crate too large"
            );
            let executable = entry.header().mode()? & 0o111 != 0;
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            ensure!(
                files
                    .insert(path, SourceEntry { bytes, executable })
                    .is_none(),
                "duplicate crate path"
            );
        }
        let manifest = files.get("Cargo.toml").context("crate manifest missing")?;
        let manifest: toml::Value = std::str::from_utf8(&manifest.bytes)?.parse()?;
        ensure!(
            manifest
                .get("package")
                .and_then(|package| package.get("name"))
                .and_then(toml::Value::as_str)
                == Some(name)
                && manifest
                    .get("package")
                    .and_then(|package| package.get("version"))
                    .and_then(toml::Value::as_str)
                    == Some(version),
            "crate manifest identity mismatch"
        );
        let features_available = manifest
            .get("features")
            .and_then(toml::Value::as_table)
            .map(|table| table.keys().cloned().collect())
            .unwrap_or_default();
        let hash = store_tree(&self.store, &files, "")?;
        let info = CrateInfo {
            name: name.into(),
            version: version.into(),
            hash,
            checksum: checksum.into(),
            features_available,
        };
        self.store.append(
            "system",
            &serde_json::json!({"type":"crate_added","crate":info}),
            0,
        )?;
        Ok(info)
    }
    pub fn materialize(&self, hash: &str, path: &Path) -> Result<()> {
        std::fs::create_dir_all(path)?;
        materialize_tree(&self.store, hash, path, 0)
    }
}
fn validate_coordinate(name: &str, version: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
        "invalid crate name"
    );
    ensure!(
        !version.is_empty()
            && version
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b".-+".contains(&byte)),
        "invalid crate version"
    );
    Ok(())
}
/// Capture immutable prepared sources using the same tree format as crate intake.
pub(crate) fn snapshot_directory(store: &Store, path: &Path) -> Result<String> {
    struct Snapshot {
        files: BTreeMap<String, SourceEntry>,
        bytes: u64,
    }
    fn visit(root: &Path, path: &Path, depth: usize, snapshot: &mut Snapshot) -> Result<()> {
        ensure!(depth < 128, "source tree depth exceeded");
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            ensure!(!kind.is_symlink(), "source tree symlink rejected");
            let path = entry.path();
            if kind.is_dir() {
                visit(root, &path, depth + 1, snapshot)?;
            } else {
                ensure!(kind.is_file(), "source tree special file rejected");
                let metadata = entry.metadata()?;
                snapshot.bytes = snapshot
                    .bytes
                    .checked_add(metadata.len())
                    .context("source tree size overflow")?;
                ensure!(
                    snapshot.bytes <= 256 * 1024 * 1024 && snapshot.files.len() < 100_000,
                    "source tree too large"
                );
                let name = path
                    .strip_prefix(root)?
                    .to_str()
                    .context("non UTF-8 source path")?
                    .to_owned();
                ensure!(!name.contains('\\'), "invalid source tree path");
                #[cfg(unix)]
                let executable = {
                    use std::os::unix::fs::PermissionsExt;
                    metadata.permissions().mode() & 0o111 != 0
                };
                #[cfg(not(unix))]
                let executable = false;
                snapshot.files.insert(
                    name,
                    SourceEntry {
                        bytes: std::fs::read(path)?,
                        executable,
                    },
                );
            }
        }
        Ok(())
    }
    ensure!(
        std::fs::symlink_metadata(path)?.file_type().is_dir(),
        "source root must be a directory"
    );
    let mut snapshot = Snapshot {
        files: BTreeMap::new(),
        bytes: 0,
    };
    visit(path, path, 0, &mut snapshot)?;
    store_tree(store, &snapshot.files, "")
}
fn store_tree(
    store: &Store,
    files: &BTreeMap<String, SourceEntry>,
    prefix: &str,
) -> Result<String> {
    let mut children = BTreeMap::new();
    for path in files.keys().filter_map(|path| path.strip_prefix(prefix)) {
        let name = path.split('/').next().context("empty path")?;
        let directory = path.contains('/');
        if let Some(previous) = children.insert(name.to_owned(), directory) {
            ensure!(previous == directory, "file/directory collision");
        }
    }
    let mut entries = Vec::new();
    for (name, directory) in children {
        let full = format!("{prefix}{name}");
        let hash = if directory {
            store_tree(store, files, &format!("{full}/"))?
        } else {
            store.put("blob", &files[&full].bytes)?
        };
        let reference = store.reference(
            &hash,
            if directory {
                loom_proto::DAG_CBOR_CODEC
            } else {
                loom_proto::RAW_CODEC
            },
        )?;
        entries.push(TreeEntry {
            executable: !directory && files[&full].executable,
            name,
            reference,
            directory,
        });
    }
    store.put_value("tree", &Tree { entries })
}
fn materialize_tree(store: &Store, hash: &str, path: &Path, depth: usize) -> Result<()> {
    ensure!(depth < 128, "crate tree depth exceeded");
    let tree: Tree = store.get_value(hash)?.context("crate tree missing")?;
    for entry in tree.entries {
        ensure!(
            !entry.name.is_empty()
                && entry.name != "."
                && entry.name != ".."
                && !entry.name.contains('/')
                && !entry.name.contains('\\'),
            "unsafe tree name"
        );
        let target = path.join(entry.name);
        ensure!(
            !target.exists() && std::fs::symlink_metadata(&target).is_err(),
            "materialization target exists"
        );
        let hash = entry.reference["$ref"]
            .as_str()
            .context("tree reference missing")?;
        if entry.directory {
            std::fs::create_dir(&target)?;
            materialize_tree(store, hash, &target, depth + 1)?;
        } else {
            std::fs::write(&target, store.get(hash)?.context("crate blob missing")?)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(
                    &target,
                    std::fs::Permissions::from_mode(if entry.executable { 0o755 } else { 0o644 }),
                )?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn archive() -> Vec<u8> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut archive = tar::Builder::new(encoder);
        let bytes = b"[package]\nname='sample'\nversion='1.2.3'\n[features]\nfast=[]\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive
            .append_data(&mut header, "sample-1.2.3/Cargo.toml", &bytes[..])
            .unwrap();
        archive.into_inner().unwrap().finish().unwrap()
    }
    #[test]
    fn rejects_links_before_storing_any_source() -> Result<()> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut archive = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        header.set_link_name("/etc/passwd")?;
        header.set_cksum();
        archive.append_data(&mut header, "sample-1.2.3/Cargo.toml", std::io::empty())?;
        let bytes = archive.into_inner()?.finish()?;
        let checksum = format!("{:x}", Sha256::digest(&bytes));
        let registry = CrateRegistry::new(Store::memory()?);
        let error = registry
            .ingest("sample", "1.2.3", &checksum, &bytes)
            .unwrap_err();
        assert!(error.to_string().contains("links and special files"));
        assert_eq!(registry.store.latest_seq()?, 0);
        Ok(())
    }
    #[cfg(unix)]
    #[test]
    fn snapshot_preserves_executable_and_rejects_symlinks() -> Result<()> {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let store = Store::memory()?;
        let directory = std::env::temp_dir().join(format!("loom-snapshot-{}", std::process::id()));
        if directory.exists() {
            std::fs::remove_dir_all(&directory)?;
        }
        std::fs::create_dir(&directory)?;
        std::fs::write(directory.join("tool"), b"#!/bin/sh\nexit 0\n")?;
        std::fs::set_permissions(
            directory.join("tool"),
            std::fs::Permissions::from_mode(0o755),
        )?;
        let hash = snapshot_directory(&store, &directory)?;
        let tree: Tree = store.get_value(&hash)?.unwrap();
        assert_eq!(tree.entries[0].name, "tool");
        assert!(tree.entries[0].executable);
        symlink("tool", directory.join("alias"))?;
        assert!(
            snapshot_directory(&store, &directory)
                .unwrap_err()
                .to_string()
                .contains("symlink")
        );
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }
    #[test]
    fn checksum_identity_and_tree_roundtrip() -> Result<()> {
        let bytes = archive();
        let checksum = format!("{:x}", Sha256::digest(&bytes));
        let first = CrateRegistry::new(Store::memory()?);
        assert!(
            first
                .ingest("sample", "1.2.3", &"0".repeat(64), &bytes)
                .is_err()
        );
        let info = first.ingest("sample", "1.2.3", &checksum, &bytes)?;
        let second = CrateRegistry::new(Store::memory()?);
        assert_eq!(
            info.hash,
            second.ingest("sample", "1.2.3", &checksum, &bytes)?.hash
        );
        assert_eq!(info.features_available, ["fast"]);
        let tree: Tree = first.store.get_value(&info.hash)?.unwrap();
        assert_eq!(tree.entries[0].name, "Cargo.toml");
        let manifest = first
            .store
            .get(tree.entries[0].reference["$ref"].as_str().unwrap())?
            .unwrap();
        assert!(std::str::from_utf8(&manifest)?.contains("sample"));
        assert!(first.ingest("other", "1.2.3", &checksum, &bytes).is_err());
        Ok(())
    }
}
