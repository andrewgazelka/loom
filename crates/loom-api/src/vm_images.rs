//! Materialize tenant CAS images into a fresh private tree without following
//! image symlinks. Symlinks become meaningful only inside the guest root.
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use loom_proto::{CasReference, DAG_CBOR_CODEC, RAW_CODEC, VmImageEntry, VmImageManifest};
use loom_store::Store;
use std::{
    fs,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

const MAX_ENTRIES: usize = 65_536;
const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 4096;

#[derive(Clone)]
pub struct StoreVmImages {
    store: Store,
}
impl StoreVmImages {
    pub fn new(store: Store) -> Self {
        Self { store }
    }
    pub fn materialize_image(
        &self,
        image: &CasReference,
        destination: &Path,
        max_bytes: u64,
    ) -> Result<()> {
        self.materialize_cancellable(image, destination, max_bytes, &AtomicBool::new(false))
    }
    fn materialize_cancellable(
        &self,
        image: &CasReference,
        destination: &Path,
        max_bytes: u64,
        cancelled: &AtomicBool,
    ) -> Result<()> {
        ensure!(max_bytes > 0, "VM image byte budget must be positive");
        check_cancelled(cancelled)?;
        require_codec(image, DAG_CBOR_CODEC)?;
        let metadata = self
            .store
            .cas_entry(&image.reference)?
            .context("VM image not found in tenant CAS")?;
        ensure!(
            metadata.size <= MAX_MANIFEST_BYTES,
            "VM image manifest too large"
        );
        let manifest: VmImageManifest = self
            .store
            .get_value(&image.reference)?
            .context("VM image missing")?;
        ensure!(
            manifest.entries.len() <= MAX_ENTRIES,
            "VM image has too many entries"
        );
        let mut bytes = 0u64;
        for (path, entry) in &manifest.entries {
            check_cancelled(cancelled)?;
            validate_path(path)?;
            let mut parent = Path::new(path).parent();
            while let Some(path) = parent.filter(|path| !path.as_os_str().is_empty()) {
                ensure!(
                    matches!(
                        manifest
                            .entries
                            .get(path.to_str().context("invalid image path")?),
                        Some(VmImageEntry::Directory { .. })
                    ),
                    "VM image parent must be an explicit directory"
                );
                parent = path.parent();
            }
            match entry {
                VmImageEntry::File { reference, mode } => {
                    validate_mode(*mode, false)?;
                    require_codec(reference, RAW_CODEC)?;
                    let entry = self
                        .store
                        .cas_entry(&reference.reference)?
                        .context("VM file missing from tenant CAS")?;
                    bytes = bytes
                        .checked_add(entry.size)
                        .context("VM image size overflow")?;
                }
                VmImageEntry::Directory { mode } => validate_mode(*mode, true)?,
                VmImageEntry::Symlink { target } => {
                    ensure!(
                        !target.is_empty()
                            && target.len() <= MAX_PATH_BYTES
                            && !target.contains(['\0', '\\']),
                        "invalid guest symlink target"
                    );
                    bytes = bytes
                        .checked_add(target.len() as u64)
                        .context("VM image size overflow")?;
                }
            }
            ensure!(bytes <= max_bytes, "VM image exceeds rootfs byte budget");
        }
        // create_dir refuses preexisting paths, including symlinks. The owner
        // chooses a private host parent; no image-controlled link exists yet.
        check_cancelled(cancelled)?;
        fs::create_dir(destination).context("VM image destination must be new")?;
        let result = self.write_tree(&manifest, destination, cancelled);
        if result.is_err() {
            // Final guest directory modes may deny traversal. Restore access
            // before removing this exclusively owned, partially created tree.
            for (path, entry) in &manifest.entries {
                if matches!(entry, VmImageEntry::Directory { .. }) {
                    let path = destination.join(path);
                    if path.is_dir() {
                        set_mode(&path, 0o700)?;
                    }
                }
            }
            fs::remove_dir_all(destination).context("remove incomplete VM image")?;
        }
        result
    }
    fn write_tree(
        &self,
        manifest: &VmImageManifest,
        destination: &Path,
        cancelled: &AtomicBool,
    ) -> Result<()> {
        set_mode(destination, 0o700)?;
        for (name, entry) in &manifest.entries {
            check_cancelled(cancelled)?;
            if matches!(entry, VmImageEntry::Directory { .. }) {
                let path = destination.join(name);
                fs::create_dir(&path)?;
                set_mode(&path, 0o700)?;
            }
        }
        for (name, entry) in &manifest.entries {
            check_cancelled(cancelled)?;
            if let VmImageEntry::File { reference, mode } = entry {
                let path = destination.join(name);
                self.store
                    .export_file_cancellable(&reference.reference, &path, cancelled)?;
                set_mode(&path, *mode)?;
            }
        }
        for (name, entry) in &manifest.entries {
            check_cancelled(cancelled)?;
            if let VmImageEntry::Symlink { target } = entry {
                create_symlink(target, &destination.join(name))?;
            }
        }
        // Children are complete before directory permissions are tightened.
        for (name, entry) in manifest.entries.iter().rev() {
            check_cancelled(cancelled)?;
            if let VmImageEntry::Directory { mode } = entry {
                set_mode(&destination.join(name), *mode)?;
            }
        }
        Ok(())
    }
}
#[async_trait]
impl loom_actor::drivers::vm::VmImages for StoreVmImages {
    async fn materialize(
        &self,
        image: &CasReference,
        dest: &loom_actor::drivers::vm::VmImageDestination,
        max_bytes: u64,
    ) -> Result<()> {
        let images = self.clone();
        let image = image.clone();
        let dest = dest.clone();
        let cancellation = CancelMaterialization(Arc::new(AtomicBool::new(false)));
        let cancelled = cancellation.0.clone();
        let result = tokio::task::spawn_blocking(move || {
            images.materialize_cancellable(&image, dest.path(), max_bytes, &cancelled)
        })
        .await?;
        drop(cancellation);
        result
    }
}
fn validate_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && path.len() <= MAX_PATH_BYTES
            && !path.contains(['\0', '\\'])
            && path
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."),
        "VM image path must be canonical and relative"
    );
    Ok(())
}
fn validate_mode(mode: u32, directory: bool) -> Result<()> {
    let allowed = if directory { 0o1777 } else { 0o777 };
    ensure!(
        mode & !allowed == 0,
        "VM image mode cannot grant special bits"
    );
    Ok(())
}
fn require_codec(reference: &CasReference, codec: u64) -> Result<()> {
    ensure!(
        loom_proto::parse_reference(&reference.reference)
            .map_err(anyhow::Error::msg)?
            .codec
            == codec,
        "VM image reference has wrong codec"
    );
    Ok(())
}
#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    Ok(fs::set_permissions(path, fs::Permissions::from_mode(mode))?)
}
#[cfg(unix)]
fn create_symlink(target: &str, path: &Path) -> Result<()> {
    Ok(std::os::unix::fs::symlink(target, path)?)
}
#[cfg(not(unix))]
fn set_mode(_: &Path, _: u32) -> Result<()> {
    anyhow::bail!("VM materialization requires Unix")
}
#[cfg(not(unix))]
fn create_symlink(_: &str, _: &Path) -> Result<()> {
    anyhow::bail!("VM materialization requires Unix")
}

struct CancelMaterialization(Arc<AtomicBool>);
impl Drop for CancelMaterialization {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
fn check_cancelled(cancelled: &AtomicBool) -> Result<()> {
    ensure!(
        !cancelled.load(Ordering::Acquire),
        "VM image materialization cancelled"
    );
    Ok(())
}
