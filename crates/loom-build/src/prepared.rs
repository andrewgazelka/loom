//! One compiler resolution per builder, revalidated from disk on every build.
//!
//! Resolving the guest toolchain spawns rustup and rustc four times, and
//! preparing the driver runs Cargo's freshness check over `tools/hash-rustc`,
//! `hash-rustc -vV`, and a hash of the driver binary. Before this memo that ran
//! twice per add (preflight, build) and the toolchain resolution a third time
//! inside the replay. The memo keeps the last resolution beside a fingerprint
//! of every input those processes read. A build reuses it only while the
//! fingerprint recomputed from disk is byte-identical and the resolved
//! binaries still have the same length and mtime; anything else discards the
//! memo and the full resolution runs again, so a stale memo can only cost one
//! resolution, never a wrong compiler.
//!
//! Invalidators (each named where it is read in [`fingerprint`] and
//! [`Prepared::binaries_unchanged`]): the selected driver path, the `RUSTC`
//! override, the cache directory (the pinned driver is built under it), the
//! toolchain pin `tools/hash-rustc/rust-toolchain.toml`, the driver sources
//! Cargo's freshness check would read (`Cargo.toml`, `Cargo.lock`, `build.rs`,
//! `src/**`), and the length and mtime of the resolved rustc, cargo and driver
//! binaries. A rustup channel update under an unchanged pin replaces the
//! rustc binary, which the mtime check catches without spawning it.
use crate::{BuildError, GuestToolchain, identity::Driver};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::SystemTime,
};

fn rejected(error: impl std::fmt::Display) -> BuildError {
    BuildError::Rejected(error.to_string())
}

#[derive(PartialEq, Eq, Clone, Debug)]
struct BinarySnapshot {
    path: PathBuf,
    len: u64,
    modified: SystemTime,
}

fn snapshot(path: &Path) -> Result<BinarySnapshot, BuildError> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| rejected(format!("compiler binary {}: {error}", path.display())))?;
    Ok(BinarySnapshot {
        path: path.to_owned(),
        len: metadata.len(),
        modified: metadata
            .modified()
            .map_err(|error| rejected(format!("compiler binary {}: {error}", path.display())))?,
    })
}

pub(crate) struct Prepared {
    pub toolchain: Arc<GuestToolchain>,
    pub driver: Arc<Driver>,
    fingerprint: String,
    binaries: Vec<BinarySnapshot>,
}

impl Prepared {
    /// False when any resolved binary was replaced, resized or removed since
    /// the resolution; a metadata read error counts as changed.
    fn binaries_unchanged(&self) -> bool {
        self.binaries
            .iter()
            .all(|recorded| snapshot(&recorded.path).is_ok_and(|current| current == *recorded))
    }
}

/// Fingerprint of every input the resolution processes read, computed without
/// spawning anything. Two builds with equal fingerprints and unchanged binaries
/// would resolve the same toolchain and driver.
pub(crate) fn fingerprint(
    root: &Path,
    cache: &Path,
    selected: Option<&Path>,
) -> Result<String, BuildError> {
    fn field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"loom-prepared-toolchain-v1");
    field(&mut hasher, cache.as_os_str().as_encoded_bytes());
    match selected {
        Some(driver) => field(&mut hasher, driver.as_os_str().as_encoded_bytes()),
        None => field(&mut hasher, b"pinned"),
    }
    match std::env::var_os("RUSTC") {
        Some(rustc) => field(&mut hasher, rustc.as_encoded_bytes()),
        None => field(&mut hasher, b""),
    }
    if selected.is_none() {
        let source = root.join("tools/hash-rustc");
        // The pin selects the channel; the sources are Cargo's freshness inputs
        // for the driver build that `Driver::prepare_with_toolchain` runs.
        for name in [
            "rust-toolchain.toml",
            "Cargo.toml",
            "Cargo.lock",
            "build.rs",
        ] {
            field(&mut hasher, name.as_bytes());
            field(
                &mut hasher,
                &std::fs::read(source.join(name)).unwrap_or_default(),
            );
        }
        let mut files = Vec::new();
        collect_files(&source.join("src"), &mut files)?;
        files.sort();
        for path in files {
            let relative = path
                .strip_prefix(&source)
                .map_err(rejected)?
                .to_string_lossy()
                .into_owned();
            field(&mut hasher, relative.as_bytes());
            field(&mut hasher, &std::fs::read(&path)?);
        }
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn collect_files(directory: &Path, files: &mut Vec<PathBuf>) -> Result<(), BuildError> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect_files(&entry.path(), files)?;
        } else if kind.is_file() {
            files.push(entry.path());
        }
    }
    Ok(())
}

/// Shared by every builder clone that owns the same cache directory
/// (`Builder::for_store`); a builder for another cache gets its own memo.
#[derive(Default)]
pub(crate) struct Memo(Mutex<Option<Prepared>>);

impl Memo {
    /// The resolved toolchain and driver for `root`, reusing the memo while
    /// [`fingerprint`] and the binary snapshots are unchanged.
    pub(crate) async fn prepare(
        &self,
        root: &Path,
        cache: &Path,
        selected: Option<&Path>,
    ) -> Result<(Arc<GuestToolchain>, Arc<Driver>), BuildError> {
        Driver::check_manifest(root, selected)?;
        let fingerprint = fingerprint(root, cache, selected)?;
        {
            let memo = self
                .0
                .lock()
                .map_err(|_| rejected("prepared toolchain memo poisoned"))?;
            if let Some(prepared) = memo.as_ref()
                && prepared.fingerprint == fingerprint
                && prepared.binaries_unchanged()
            {
                return Ok((prepared.toolchain.clone(), prepared.driver.clone()));
            }
        }
        let toolchain = Arc::new(crate::resolve_guest_toolchain_with_driver(root, selected).await?);
        let driver =
            Arc::new(Driver::prepare_with_toolchain(root, cache, selected, &toolchain).await?);
        let binaries = [&toolchain.rustc, &toolchain.cargo, &driver.path]
            .into_iter()
            .map(|path| snapshot(path))
            .collect::<Result<Vec<_>, _>>()?;
        *self
            .0
            .lock()
            .map_err(|_| rejected("prepared toolchain memo poisoned"))? = Some(Prepared {
            toolchain: toolchain.clone(),
            driver: driver.clone(),
            fingerprint,
            binaries,
        });
        Ok((toolchain, driver))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("loom-prepared-{name}-{}", std::process::id()));
        if root.exists() {
            std::fs::remove_dir_all(&root).unwrap();
        }
        let source = root.join("tools/hash-rustc");
        std::fs::create_dir_all(source.join("src/graph")).unwrap();
        std::fs::write(
            source.join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"nightly-2026-08-24\"\n",
        )
        .unwrap();
        std::fs::write(source.join("Cargo.toml"), "[package]\nname='hash-rustc'\n").unwrap();
        std::fs::write(source.join("Cargo.lock"), "version = 4\n").unwrap();
        std::fs::write(source.join("build.rs"), "fn main() {}\n").unwrap();
        std::fs::write(source.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(source.join("src/graph/mod.rs"), "pub fn graph() {}\n").unwrap();
        root
    }

    #[test]
    fn fingerprint_tracks_pin_driver_sources_and_selection() {
        let root = fixture("fingerprint");
        let cache = root.join("cache");
        let baseline = fingerprint(&root, &cache, None).unwrap();
        assert_eq!(baseline, fingerprint(&root, &cache, None).unwrap());
        std::fs::write(
            root.join("tools/hash-rustc/src/graph/mod.rs"),
            "pub fn graph() { changed() }\n",
        )
        .unwrap();
        let edited_source = fingerprint(&root, &cache, None).unwrap();
        assert_ne!(
            baseline, edited_source,
            "nested driver source edit must invalidate"
        );
        std::fs::write(
            root.join("tools/hash-rustc/rust-toolchain.toml"),
            "[toolchain]\nchannel = \"nightly-2026-09-01\"\n",
        )
        .unwrap();
        let repinned = fingerprint(&root, &cache, None).unwrap();
        assert_ne!(edited_source, repinned, "pin change must invalidate");
        assert_ne!(
            repinned,
            fingerprint(&root, &root.join("other-cache"), None).unwrap(),
            "the driver is built under the cache directory"
        );
        let prebuilt = fingerprint(&root, &cache, Some(Path::new("/opt/loom/hash-rustc"))).unwrap();
        assert_ne!(repinned, prebuilt);
        // A prebuilt driver reads no pin and no sources: edits there are irrelevant.
        std::fs::write(
            root.join("tools/hash-rustc/src/main.rs"),
            "fn main() { 1 }\n",
        )
        .unwrap();
        assert_eq!(
            prebuilt,
            fingerprint(&root, &cache, Some(Path::new("/opt/loom/hash-rustc"))).unwrap()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replaced_binary_invalidates_the_memo() {
        let root = fixture("binaries");
        let binary = root.join("rustc");
        std::fs::write(&binary, b"compiler v1").unwrap();
        let prepared = Prepared {
            toolchain: Arc::new(GuestToolchain {
                channel: None,
                rustc: binary.clone(),
                cargo: binary.clone(),
                sysroot: root.clone(),
                version: "test".into(),
            }),
            driver: Arc::new(Driver {
                path: binary.clone(),
                toolchain_hash: "test".into(),
            }),
            fingerprint: "unchanged".into(),
            binaries: vec![snapshot(&binary).unwrap()],
        };
        assert!(prepared.binaries_unchanged());
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&binary, b"compiler v2 with a longer body").unwrap();
        assert!(
            !prepared.binaries_unchanged(),
            "resized binary must invalidate"
        );
        std::fs::remove_file(&binary).unwrap();
        assert!(
            !prepared.binaries_unchanged(),
            "missing binary must invalidate"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
