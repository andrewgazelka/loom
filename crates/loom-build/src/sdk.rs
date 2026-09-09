//! Generated SDK lock/vendor overlays never change the immutable definition source.
use crate::{BuildError, VENDOR_CONFIG};
use loom_check::{CheckedDef, SourceBundle};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{fs, process::Command};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct PackageKey {
    name: String,
    version: String,
    source: Option<String>,
}
#[derive(Clone)]
struct Package {
    key: PackageKey,
    dependencies: Vec<String>,
    raw: toml::Value,
}
struct Lock {
    raw: toml::Value,
    packages: Vec<Package>,
}
impl Lock {
    fn parse(bytes: &[u8]) -> Result<Self, BuildError> {
        let text =
            std::str::from_utf8(bytes).map_err(|error| BuildError::Rejected(error.to_string()))?;
        let raw: toml::Value =
            toml::from_str(text).map_err(|error| BuildError::Rejected(error.to_string()))?;
        let packages = raw
            .get("package")
            .and_then(toml::Value::as_array)
            .ok_or_else(|| BuildError::Rejected("Cargo.lock has no package array".into()))?
            .iter()
            .map(|value| {
                let field = |name: &str| {
                    value
                        .get(name)
                        .and_then(toml::Value::as_str)
                        .map(str::to_owned)
                        .ok_or_else(|| {
                            BuildError::Rejected(format!("Cargo.lock package missing {name}"))
                        })
                };
                Ok(Package {
                    key: PackageKey {
                        name: field("name")?,
                        version: field("version")?,
                        source: value
                            .get("source")
                            .and_then(toml::Value::as_str)
                            .map(str::to_owned),
                    },
                    dependencies: value
                        .get("dependencies")
                        .and_then(toml::Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(toml::Value::as_str)
                                .map(str::to_owned)
                                .collect()
                        })
                        .unwrap_or_default(),
                    raw: value.clone(),
                })
            })
            .collect::<Result<Vec<_>, BuildError>>()?;
        Ok(Self { raw, packages })
    }
    fn reachable(&self, roots: &BTreeSet<PackageKey>, exclude_sdk: bool) -> BTreeSet<PackageKey> {
        let mut pending: Vec<&Package> = self
            .packages
            .iter()
            .filter(|package| roots.contains(&package.key))
            .collect();
        let mut visited = BTreeSet::new();
        while let Some(package) = pending.pop() {
            if exclude_sdk && sdk_package(&package.key) || !visited.insert(package.key.clone()) {
                continue;
            }
            for reference in &package.dependencies {
                let mut parts = reference.splitn(3, ' ');
                let name = parts.next().unwrap_or_default();
                let version = parts.next();
                let source = parts
                    .next()
                    .map(|source| source.trim_start_matches('(').trim_end_matches(')'));
                let candidates: Vec<_> = self
                    .packages
                    .iter()
                    .filter(|candidate| {
                        candidate.key.name == name
                            && version.is_none_or(|version| candidate.key.version == version)
                            && source.is_none_or(|source| {
                                candidate.key.source.as_deref() == Some(source)
                            })
                    })
                    .collect();
                // Ambiguous external lock references cannot grant SDK ownership.
                // User reachability remains conservative and protects every match.
                if exclude_sdk || candidates.len() == 1 {
                    pending.extend(candidates);
                }
            }
        }
        visited
    }
}
fn sdk_package(key: &PackageKey) -> bool {
    key.source.is_none()
        && ["loom-guest-rs", "loom-guest-macros", "loom-proto"].contains(&key.name.as_str())
}
fn sdk_roots(lock: &Lock) -> BTreeSet<PackageKey> {
    lock.packages
        .iter()
        .filter(|package| sdk_package(&package.key))
        .map(|package| package.key.clone())
        .collect()
}
fn user_roots(manifest: &toml::Value) -> BTreeSet<String> {
    fn walk(value: &toml::Value, roots: &mut BTreeSet<String>) {
        if let Some(table) = value.as_table() {
            for (key, value) in table {
                if ["dependencies", "build-dependencies", "dev-dependencies"]
                    .contains(&key.as_str())
                {
                    if let Some(dependencies) = value.as_table() {
                        for (alias, dependency) in dependencies {
                            // Only this reserved path dependency is injected by the host.
                            if alias == "loom"
                                && dependency.get("package").and_then(toml::Value::as_str)
                                    == Some("loom-guest-rs")
                                && dependency.get("path").is_some()
                            {
                                continue;
                            }
                            roots.insert(
                                dependency
                                    .get("package")
                                    .and_then(toml::Value::as_str)
                                    .unwrap_or(alias)
                                    .to_owned(),
                            );
                        }
                    }
                } else {
                    walk(value, roots);
                }
            }
        }
    }
    let mut roots = BTreeSet::new();
    walk(manifest, &mut roots);
    roots
}
fn validate_pins(
    original: &Lock,
    updated: &Lock,
    manifest: &toml::Value,
) -> Result<(), BuildError> {
    let names = user_roots(manifest);
    let roots = original
        .packages
        .iter()
        .filter(|package| names.contains(&package.key.name))
        .map(|package| package.key.clone())
        .collect();
    let user = original.reachable(&roots, true);
    let sdk = original.reachable(&sdk_roots(original), false);
    for old in &original.packages {
        // Cargo may fill a checksum omitted by an older lockfile. That adds a
        // content pin; removing or changing an existing pin remains forbidden.
        if let Some(checksum) = old.raw.get("checksum")
            && let Some(new) = updated.packages.iter().find(|new| new.key == old.key)
            && Some(checksum) != new.raw.get("checksum")
        {
            return Err(BuildError::Rejected(format!(
                "SDK rebuild changed checksum for {} {}",
                old.key.name, old.key.version
            )));
        }
    }
    let updated: BTreeSet<&PackageKey> = updated
        .packages
        .iter()
        .map(|package| &package.key)
        .collect();
    for package in &original.packages {
        if package.key.source.is_some()
            && !updated.contains(&package.key)
            && (user.contains(&package.key) || !sdk.contains(&package.key))
        {
            return Err(BuildError::Rejected(format!(
                "SDK rebuild would change user registry pin {} {}. Define an explicit source/lock migration instead.",
                package.key.name, package.key.version
            )));
        }
    }
    Ok(())
}
fn sdk_graph_changed(original: &Lock, current: &Lock) -> bool {
    let sdk = current.reachable(&sdk_roots(current), false);
    current
        .packages
        .iter()
        .filter(|package| sdk.contains(&package.key))
        .any(|package| {
            original
                .packages
                .iter()
                .find(|old| old.key == package.key)
                .is_none_or(|old| sdk_package(&package.key) && old.raw != package.raw)
        })
}

pub(crate) struct Rebuild<'a> {
    pub store: &'a loom_store::Store,
    pub root: &'a Path,
    pub cache: &'a Path,
    pub directory: &'a Path,
    pub definition: &'a CheckedDef,
    pub isolated: bool,
}
pub(crate) async fn reconcile(job: Rebuild<'_>) -> Result<(), BuildError> {
    let lock_path = job.directory.join("Cargo.lock");
    if !lock_path.is_file() {
        return Ok(());
    }
    let original_bytes = fs::read(&lock_path).await?;
    let original = Lock::parse(&original_bytes)?;
    let current = Lock::parse(&fs::read(job.root.join("Cargo.lock")).await?)?;
    let manifest: toml::Value =
        toml::from_str(&fs::read_to_string(job.directory.join("Cargo.toml")).await?)
            .map_err(|error| BuildError::Rejected(error.to_string()))?;
    if !sdk_graph_changed(&original, &current) {
        return Ok(());
    }
    let overlay = job.cache.join("sdk-overlays").join(&job.definition.hash);
    if overlay.exists() {
        fs::remove_dir_all(&overlay).await?;
    }
    fs::create_dir_all(&overlay).await?;
    // Resolve the current SDK manifests against the definition's own lock.
    // Importing the workspace SDK lock would override explicit CAS version pins
    // (for example serde 1.0.210 with a workspace locked to serde 1.0.229).
    // Metadata resolves dependencies but never executes a build script. User
    // path/git/registry configuration has already been rejected by materialize.
    let config = job.directory.join(".cargo");
    let saved = overlay.join("original-cargo-config");
    let has_config = config.exists();
    if has_config {
        fs::rename(&config, &saved).await?;
    }
    let outcome = tokio::time::timeout(
        Duration::from_secs(300),
        Command::new("cargo")
            .args(["metadata", "--format-version=1"])
            .current_dir(job.directory)
            .kill_on_drop(true)
            .output(),
    )
    .await;
    if has_config {
        fs::rename(&saved, &config).await?;
    }
    let output = outcome
        .map_err(|_| BuildError::Rejected("SDK lock resolution exceeded 300 seconds".into()))??;
    if !output.status.success() {
        fs::write(&lock_path, &original_bytes).await?;
        return Err(BuildError::Rejected(format!(
            "SDK lock resolution: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let updated_bytes = fs::read(&lock_path).await?;
    let updated = Lock::parse(&updated_bytes)?;
    if let Err(error) = validate_pins(&original, &updated, &manifest) {
        fs::write(&lock_path, &original_bytes).await?;
        return Err(error);
    }
    if job.isolated && updated.raw != original.raw {
        // Re-vendor into an isolated overlay, then add only package identities
        // absent from the immutable original vendor tree. Existing vendor bytes
        // (including intentional patches) are never replaced.
        let crate_dir = overlay.join("crate");
        fs::create_dir_all(crate_dir.join("src")).await?;
        if let Ok(bundle) = serde_json::from_str::<SourceBundle>(&job.definition.source) {
            for name in bundle.files.keys() {
                if name.starts_with("vendor/")
                    || name.starts_with(".cargo/")
                    || name == "Cargo.toml"
                    || name == "Cargo.lock"
                {
                    continue;
                }
                let path = crate_dir.join(name);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).await?;
                }
                fs::copy(job.directory.join(name), path).await?;
            }
        } else {
            fs::copy(
                job.directory.join("src/lib.rs"),
                crate_dir.join("src/lib.rs"),
            )
            .await?;
        }
        materialize_pinned_crates(job.store, &manifest, &crate_dir)?;
        fs::copy(
            job.directory.join("Cargo.toml"),
            crate_dir.join("Cargo.toml"),
        )
        .await?;
        fs::write(crate_dir.join("Cargo.lock"), &updated_bytes).await?;
        let output = tokio::time::timeout(
            Duration::from_secs(300),
            Command::new(job.root.join("loom-rustc/sandbox.sh"))
                .arg("vendor")
                .arg(job.cache)
                .arg(&crate_dir)
                .arg(overlay.join("target"))
                .arg(job.root)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| BuildError::Rejected("SDK vendor overlay exceeded 300 seconds".into()))??;
        if !output.status.success() {
            return Err(BuildError::Rejected(format!(
                "SDK vendor overlay: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        merge_vendor(&crate_dir.join("vendor"), &job.directory.join("vendor"))?;
        fs::write(config.join("config.toml"), VENDOR_CONFIG).await?;
    }
    fs::remove_dir_all(overlay).await?;
    Ok(())
}
fn materialize_pinned_crates(
    store: &loom_store::Store,
    manifest: &toml::Value,
    directory: &Path,
) -> Result<(), BuildError> {
    let Some(patches) = manifest
        .get("patch")
        .and_then(|patch| patch.get("crates-io"))
        .and_then(toml::Value::as_table)
    else {
        return Ok(());
    };
    let mut seen = BTreeSet::new();
    for patch in patches.values() {
        let path = patch
            .get("path")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| BuildError::Rejected("host crate patch path missing".into()))?;
        let hash = path
            .strip_prefix("loom-crates/")
            .filter(|hash| {
                hash.len() == 64
                    && hash
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
            .ok_or_else(|| {
                BuildError::Rejected("host crate patch requires a source hash".into())
            })?;
        if seen.insert(hash) {
            crate::registry::CrateRegistry::new(store.clone())
                .materialize(hash, &directory.join("loom-crates").join(hash))
                .map_err(|error| BuildError::Rejected(error.to_string()))?;
        }
    }
    Ok(())
}
fn vendor_identity(directory: &Path) -> Result<PackageKey, BuildError> {
    let manifest: toml::Value =
        toml::from_str(&std::fs::read_to_string(directory.join("Cargo.toml"))?)
            .map_err(|error| BuildError::Rejected(error.to_string()))?;
    let field = |name: &str| {
        manifest
            .get("package")
            .and_then(|package| package.get(name))
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| BuildError::Rejected(format!("vendor package missing {name}")))
    };
    Ok(PackageKey {
        name: field("name")?,
        version: field("version")?,
        source: None,
    })
}
fn merge_vendor(source: &Path, destination: &Path) -> Result<(), BuildError> {
    fn copy_tree(source: &Path, destination: &Path) -> Result<(), BuildError> {
        std::fs::create_dir_all(destination)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                return Err(BuildError::Rejected("SDK vendor symlink rejected".into()));
            }
            if kind.is_dir() {
                copy_tree(&entry.path(), &destination.join(entry.file_name()))?;
            } else {
                std::fs::copy(entry.path(), destination.join(entry.file_name()))?;
            }
        }
        Ok(())
    }
    // CAS materializations are shared by definitions. An SDK upgrade gets a
    // private overlay before adding files; it cannot mutate the addressed tree.
    if std::fs::symlink_metadata(destination)?
        .file_type()
        .is_symlink()
    {
        let original = std::fs::canonicalize(destination)?;
        std::fs::remove_file(destination)?;
        copy_tree(&original, destination)?;
    }
    let mut existing = BTreeMap::<PackageKey, PathBuf>::new();
    for entry in std::fs::read_dir(destination)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            existing.insert(vendor_identity(&entry.path())?, entry.path());
        }
    }
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let identity = vendor_identity(&entry.path())?;
        if existing.contains_key(&identity) {
            continue;
        }
        let path = destination.join(format!("{}-{}", identity.name, identity.version));
        if path.exists() {
            return Err(BuildError::Rejected(
                "SDK vendor package path collision".into(),
            ));
        }
        copy_tree(&entry.path(), &path)?;
        existing.insert(identity, path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn lock(source: &str) -> Lock {
        Lock::parse(source.as_bytes()).unwrap()
    }
    #[test]
    fn overlay_materializes_pinned_crates_from_cas() {
        let store = loom_store::Store::memory().unwrap();
        let bytes = b"[package]\nname='pinned'\nversion='1.0.0'\n";
        let blob = store.put("blob", bytes).unwrap();
        let tree = loom_proto::Tree {
            entries: vec![loom_proto::TreeEntry {
                name: "Cargo.toml".into(),
                reference: store.reference(&blob, loom_proto::RAW_CODEC).unwrap(),
                directory: false,
                executable: false,
            }],
        };
        let hash = store.put_value("tree", &tree).unwrap();
        let manifest: toml::Value = format!("[patch.crates-io]\npinned={{path='loom-crates/{hash}'}}\nalias={{path='loom-crates/{hash}'}}\n").parse().unwrap();
        let directory =
            std::env::temp_dir().join(format!("loom-sdk-overlay-{}", std::process::id()));
        if directory.exists() {
            std::fs::remove_dir_all(&directory).unwrap();
        }
        materialize_pinned_crates(&store, &manifest, &directory).unwrap();
        assert_eq!(
            std::fs::read(directory.join("loom-crates").join(&hash).join("Cargo.toml")).unwrap(),
            bytes
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn filling_missing_checksum_preserves_existing_pins() {
        let source = "version=4\n[[package]]\nname='anyhow'\nversion='1.0.100'\nsource='registry+https://github.com/rust-lang/crates.io-index'\n";
        let original = lock(source);
        let pinned = lock(&format!(
            "{source}checksum='a23eb6b1614318a8071c9b2521f36b424b2c83db5eb3a0fead4a6c0809af6e61'\n"
        ));
        let changed = lock(&format!("{source}checksum='{}'\n", "b".repeat(64)));
        let manifest = toml::from_str("[dependencies]\nanyhow='=1.0.100'\n").unwrap();
        assert!(validate_pins(&original, &pinned, &manifest).is_ok());
        assert!(validate_pins(&pinned, &original, &manifest).is_err());
        assert!(validate_pins(&pinned, &changed, &manifest).is_err());
    }
    #[test]
    fn registry_package_sharing_sdk_name_is_not_sdk_owned() {
        let original = lock(
            r#"version=4
[[package]]
name="loom-guest-rs"
version="0.1.0"
[[package]]
name="loom-guest-rs"
version="9.0.0"
source="registry+https://github.com/rust-lang/crates.io-index"
dependencies=["user-only"]
[[package]]
name="user-only"
version="1.0.0"
source="registry+https://github.com/rust-lang/crates.io-index"
"#,
        );
        let updated = lock(
            r#"version=4
[[package]]
name="loom-guest-rs"
version="0.1.0"
"#,
        );
        let manifest =
            toml::from_str("[dependencies]\nloom={package=\"loom-guest-rs\",path=\"../guest\"}")
                .unwrap();
        assert!(validate_pins(&original, &updated, &manifest).is_err());
    }
    #[test]
    fn sdk_overlap_does_not_authorize_changing_user_pins() {
        let old = lock(
            r#"version=4
[[package]]
name="loom-guest-rs"
version="0.1.0"
dependencies=["serde", "old-codec"]
[[package]]
name="serde"
version="1.0.0"
source="registry+https://github.com/rust-lang/crates.io-index"
[[package]]
name="old-codec"
version="0.2.0"
source="registry+https://github.com/rust-lang/crates.io-index"
"#,
        );
        let updated = lock(
            r#"version=4
[[package]]
name="loom-guest-rs"
version="0.1.0"
dependencies=["serde", "new-codec"]
[[package]]
name="serde"
version="1.1.0"
source="registry+https://github.com/rust-lang/crates.io-index"
[[package]]
name="new-codec"
version="0.7.0"
source="registry+https://github.com/rust-lang/crates.io-index"
"#,
        );
        let user: toml::Value =
            toml::from_str("[dependencies]\nmy_serde={package=\"serde\",version=\"1\"}").unwrap();
        assert!(validate_pins(&old, &updated, &user).is_err());
        let sdk_only: toml::Value =
            toml::from_str("[dependencies]\nloom={package=\"loom-guest-rs\",path=\"../guest\"}")
                .unwrap();
        assert!(validate_pins(&old, &updated, &sdk_only).is_ok());
    }
}
