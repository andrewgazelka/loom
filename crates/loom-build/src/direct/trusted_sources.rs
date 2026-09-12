//! Cross-graph reuse trusts repository SDK sources or independently checked archives.
use crate::BuildError;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::{Component, Path, PathBuf},
};

const SDK: [&str; 2] = ["loom-guest-rs", "loom-proto"];

#[derive(Deserialize)]
struct Lock {
    #[serde(rename = "package")]
    packages: Vec<Package>,
}
#[derive(Deserialize)]
struct Package {
    name: String,
    version: String,
    source: Option<String>,
    checksum: Option<String>,
    #[serde(default)]
    dependencies: Vec<String>,
}

fn rejected(error: impl std::fmt::Display) -> BuildError {
    BuildError::Rejected(error.to_string())
}

pub(super) fn approved(root: &Path, source: &Path) -> Result<bool, BuildError> {
    let source = source.canonicalize()?;
    let Some(directory) = source
        .ancestors()
        .find(|path| path.is_dir() && path.join("Cargo.toml").is_file())
    else {
        return Ok(false);
    };
    for name in SDK {
        let path = root.join("crates").join(name);
        if path.is_dir() && path.canonicalize()? == directory {
            return Ok(true);
        }
    }
    let manifest: toml::Value =
        toml::from_str(&std::fs::read_to_string(directory.join("Cargo.toml"))?)
            .map_err(rejected)?;
    let Some(package) = manifest.get("package") else {
        return Ok(false);
    };
    let Some(name) = package.get("name").and_then(toml::Value::as_str) else {
        return Ok(false);
    };
    let Some(version) = package.get("version").and_then(toml::Value::as_str) else {
        return Ok(false);
    };
    let lock: Lock =
        toml::from_str(&std::fs::read_to_string(root.join("Cargo.lock"))?).map_err(rejected)?;
    let reachable = sdk_packages(&lock);
    for index in reachable {
        let package = &lock.packages[index];
        if package.name != name
            || package.version != version
            || package.source.as_deref()
                != Some("registry+https://github.com/rust-lang/crates.io-index")
        {
            continue;
        }
        let Some(checksum) = &package.checksum else {
            continue;
        };
        if let Some(archive) = cached_archive(name, version, checksum)? {
            return matches_archive(directory, name, version, &archive);
        }
    }
    Ok(false)
}

/// The selected compiler owns this source tree and its checksum-pinned lock.
pub(super) fn compiler_sources(
    sysroot: &Path,
    vendor: Option<&Path>,
) -> Result<Vec<PathBuf>, BuildError> {
    let library = sysroot
        .join("lib/rustlib/src/rust/library")
        .canonicalize()?;
    let lock: Lock =
        toml::from_str(&std::fs::read_to_string(library.join("Cargo.lock"))?).map_err(rejected)?;
    let mut approved = vec![library];
    let mut registry_roots = Vec::new();
    if let Some(home) = cargo_home() {
        let sources = home.join("registry/src");
        if sources.is_dir() {
            for entry in std::fs::read_dir(sources)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    registry_roots.push(entry.path());
                }
            }
        }
    }
    for index in reachable_packages(&lock, &["std", "sysroot"]) {
        let package = &lock.packages[index];
        if package.source.as_deref()
            != Some("registry+https://github.com/rust-lang/crates.io-index")
        {
            continue;
        }
        let Some(checksum) = &package.checksum else {
            continue;
        };
        let mut archive = cached_archive(&package.name, &package.version, checksum)?;
        if archive.is_none()
            && let Some(vendor) = vendor
        {
            let path = vendor.join(format!(
                ".loom-archive-{}-{}.crate",
                package.name, package.version
            ));
            if path.is_file() && path.metadata()?.len() <= 64 * 1024 * 1024 {
                let bytes = std::fs::read(path)?;
                if format!("{:x}", Sha256::digest(&bytes)) == *checksum {
                    archive = Some(bytes);
                }
            }
        }
        let Some(archive) = archive else { continue };
        let mut candidates: Vec<PathBuf> = registry_roots
            .iter()
            .map(|root| root.join(format!("{}-{}", package.name, package.version)))
            .collect();
        if let Some(vendor) = vendor {
            candidates.push(vendor.join(&package.name));
            candidates.push(vendor.join(format!("{}-{}", package.name, package.version)));
        }
        for directory in candidates {
            if directory.is_dir()
                && matches_archive(&directory, &package.name, &package.version, &archive)?
            {
                approved.push(directory.canonicalize()?);
            }
        }
    }
    approved.sort();
    approved.dedup();
    Ok(approved)
}

fn sdk_packages(lock: &Lock) -> BTreeSet<usize> {
    reachable_packages(lock, &SDK)
}

fn reachable_packages(lock: &Lock, roots: &[&str]) -> BTreeSet<usize> {
    let mut pending: Vec<usize> = lock
        .packages
        .iter()
        .enumerate()
        .filter(|entry| entry.1.source.is_none() && roots.contains(&entry.1.name.as_str()))
        .map(|entry| entry.0)
        .collect();
    let mut seen = BTreeSet::new();
    while let Some(index) = pending.pop() {
        if !seen.insert(index) {
            continue;
        }
        for dependency in &lock.packages[index].dependencies {
            let mut parts = dependency.splitn(3, ' ');
            let name = parts.next().unwrap_or_default();
            let version = parts.next();
            let source = parts
                .next()
                .map(|source| source.trim_start_matches('(').trim_end_matches(')'));
            let mut matches = lock.packages.iter().enumerate().filter(|entry| {
                entry.1.name == name
                    && version.is_none_or(|version| entry.1.version == version)
                    && source.is_none_or(|source| entry.1.source.as_deref() == Some(source))
            });
            if let Some(candidate) = matches.next()
                && matches.next().is_none()
            {
                pending.push(candidate.0);
            }
        }
    }
    seen
}

fn cargo_home() -> Option<PathBuf> {
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
}

fn cached_archive(
    name: &str,
    version: &str,
    checksum: &str,
) -> Result<Option<Vec<u8>>, BuildError> {
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-+".contains(&byte))
    {
        return Ok(None);
    }
    let Some(home) = cargo_home() else {
        return Ok(None);
    };
    let cache = home.join("registry/cache");
    if !cache.is_dir() {
        return Ok(None);
    }
    for registry in std::fs::read_dir(cache)? {
        let archive = registry?.path().join(format!("{name}-{version}.crate"));
        if !archive.is_file() || archive.metadata()?.len() > 64 * 1024 * 1024 {
            continue;
        }
        let bytes = std::fs::read(archive)?;
        if format!("{:x}", Sha256::digest(&bytes)) == checksum {
            return Ok(Some(bytes));
        }
    }
    Ok(None)
}

fn matches_archive(
    directory: &Path,
    name: &str,
    version: &str,
    bytes: &[u8],
) -> Result<bool, BuildError> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    let prefix = format!("{name}-{version}");
    struct ExpectedFile {
        bytes: Vec<u8>,
        executable: bool,
    }
    let mut expected = BTreeMap::new();
    let mut total = 0_u64;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if !path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
            || path.components().count() > 128
        {
            return Ok(false);
        }
        let Ok(relative) = path.strip_prefix(&prefix) else {
            return Ok(false);
        };
        if entry.header().entry_type().is_dir() {
            continue;
        }
        if !entry.header().entry_type().is_file() {
            return Ok(false);
        }
        total = total.saturating_add(entry.size());
        if total > 256 * 1024 * 1024 || expected.len() >= 100_000 {
            return Ok(false);
        }
        let mut contents = Vec::new();
        entry.read_to_end(&mut contents)?;
        let expected_file = ExpectedFile {
            bytes: contents,
            executable: entry.header().mode()? & 0o111 != 0,
        };
        if expected
            .insert(relative.to_owned(), expected_file)
            .is_some()
        {
            return Ok(false);
        }
    }
    let mut pending = vec![directory.to_owned()];
    let mut visited = 0_usize;
    while let Some(path) = pending.pop() {
        if path
            .strip_prefix(directory)
            .map_err(rejected)?
            .components()
            .count()
            >= 128
        {
            return Ok(false);
        }
        for entry in std::fs::read_dir(path)? {
            visited += 1;
            if visited > 200_000 {
                return Ok(false);
            }
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                return Ok(false);
            }
            if kind.is_dir() {
                pending.push(path);
                continue;
            }
            if !kind.is_file() {
                return Ok(false);
            }
            let relative = path.strip_prefix(directory).map_err(rejected)?;
            // Cargo creates these metadata files after unpacking the checked archive.
            if relative == Path::new(".cargo-ok") || relative == Path::new(".cargo-checksum.json") {
                continue;
            }
            let Some(expected_file) = expected.remove(relative) else {
                return Ok(false);
            };
            let metadata = entry.metadata()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if (metadata.permissions().mode() & 0o111 != 0) != expected_file.executable {
                    return Ok(false);
                }
            }
            if metadata.len() != expected_file.bytes.len() as u64
                || std::fs::read(&path)? != expected_file.bytes
            {
                return Ok(false);
            }
        }
    }
    // Cargo vendor omits these VCS metadata files even when the published
    // archive contains them (native vendor comparison, 2026-09-09). If present
    // above, they still had to match exactly; only their absence is permitted.
    Ok(expected.keys().all(|path| {
        matches!(
            path.file_name().and_then(|name| name.to_str()),
            Some(".gitignore" | ".gitattributes")
        )
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compiler_source_requires_reachable_lock_and_original_archive() {
        let root = std::env::temp_dir().join(format!("loom-compiler-proof-{}", std::process::id()));
        if root.exists() {
            std::fs::remove_dir_all(&root).unwrap();
        }
        let library = root.join("lib/rustlib/src/rust/library");
        let vendor = root.join("vendor");
        let source = vendor.join("loom-compiler-proof-fixture");
        std::fs::create_dir_all(&library).unwrap();
        std::fs::create_dir_all(source.join("src")).unwrap();
        let original = b"pub fn value() -> u8 { 1 }";
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut archive = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(original.len() as u64);
        header.set_cksum();
        archive
            .append_data(
                &mut header,
                "loom-compiler-proof-fixture-1.0.0/src/lib.rs",
                &original[..],
            )
            .unwrap();
        let bytes = archive.into_inner().unwrap().finish().unwrap();
        let checksum = format!("{:x}", Sha256::digest(&bytes));
        let lock = format!(
            r#"version = 4
[[package]]
name = "std"
version = "0.0.0"
dependencies = ["loom-compiler-proof-fixture"]
[[package]]
name = "loom-compiler-proof-fixture"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "{checksum}"
"#
        );
        std::fs::write(library.join("Cargo.lock"), &lock).unwrap();
        std::fs::write(source.join("src/lib.rs"), original).unwrap();
        let evidence = vendor.join(".loom-archive-loom-compiler-proof-fixture-1.0.0.crate");
        std::fs::write(&evidence, &bytes).unwrap();
        let canonical = source.canonicalize().unwrap();
        assert!(
            compiler_sources(&root, Some(&vendor))
                .unwrap()
                .contains(&canonical)
        );

        std::fs::write(source.join("src/lib.rs"), b"changed").unwrap();
        std::fs::write(source.join(".cargo-checksum.json"), b"{\"files\":{}}").unwrap();
        assert!(
            !compiler_sources(&root, Some(&vendor))
                .unwrap()
                .contains(&canonical)
        );
        std::fs::write(source.join("src/lib.rs"), original).unwrap();
        std::fs::write(&evidence, b"forged archive").unwrap();
        assert!(
            !compiler_sources(&root, Some(&vendor))
                .unwrap()
                .contains(&canonical)
        );
        std::fs::write(&evidence, &bytes).unwrap();
        std::fs::write(
            library.join("Cargo.lock"),
            lock.replace(
                "dependencies = [\"loom-compiler-proof-fixture\"]",
                "dependencies = []",
            ),
        )
        .unwrap();
        assert!(
            !compiler_sources(&root, Some(&vendor))
                .unwrap()
                .contains(&canonical)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn altered_source_cannot_forge_vendor_checksum_metadata() {
        let directory =
            std::env::temp_dir().join(format!("loom-trusted-source-{}", std::process::id()));
        if directory.exists() {
            std::fs::remove_dir_all(&directory).unwrap();
        }
        std::fs::create_dir_all(directory.join("src")).unwrap();
        let original = b"pub fn value() -> u8 { 1 }";
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut archive = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(original.len() as u64);
        header.set_cksum();
        archive
            .append_data(&mut header, "sample-1.0.0/src/lib.rs", &original[..])
            .unwrap();
        let metadata = b"target/\n";
        let mut metadata_header = tar::Header::new_gnu();
        metadata_header.set_mode(0o644);
        metadata_header.set_size(metadata.len() as u64);
        metadata_header.set_cksum();
        archive
            .append_data(
                &mut metadata_header,
                "sample-1.0.0/.gitignore",
                &metadata[..],
            )
            .unwrap();
        let bytes = archive.into_inner().unwrap().finish().unwrap();
        std::fs::write(directory.join("src/lib.rs"), original).unwrap();
        assert!(matches_archive(&directory, "sample", "1.0.0", &bytes).unwrap());
        std::fs::write(directory.join(".gitignore"), b"forged metadata").unwrap();
        assert!(!matches_archive(&directory, "sample", "1.0.0", &bytes).unwrap());
        std::fs::remove_file(directory.join(".gitignore")).unwrap();
        std::fs::write(directory.join("src/lib.rs"), b"pub fn value() -> u8 { 2 }").unwrap();
        std::fs::write(
            directory.join(".cargo-checksum.json"),
            b"{\"package\":\"forged\",\"files\":{}}",
        )
        .unwrap();
        assert!(!matches_archive(&directory, "sample", "1.0.0", &bytes).unwrap());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
