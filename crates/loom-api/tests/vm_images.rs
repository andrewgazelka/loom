use anyhow::Result;
use loom_api::StoreVmImages;
use loom_proto::{CasReference, VmArchitecture, VmImageEntry, VmImageFormat, VmImageManifest};
use loom_store::Store;
use std::collections::BTreeMap;

fn reference(store: &Store, hash: &str, codec: u64) -> Result<CasReference> {
    Ok(serde_json::from_value(store.reference(hash, codec)?)?)
}
fn image(store: &Store, entries: BTreeMap<String, VmImageEntry>) -> Result<CasReference> {
    let hash = store.put_value(
        "vm_image",
        &VmImageManifest {
            format: VmImageFormat::RootfsV1,
            arch: VmArchitecture::X86_64,
            entries,
        },
    )?;
    reference(store, &hash, loom_proto::DAG_CBOR_CODEC)
}
fn file(store: &Store, bytes: &[u8], mode: u32) -> Result<VmImageEntry> {
    let hash = store.put("vm_file", bytes)?;
    Ok(VmImageEntry::File {
        reference: reference(store, &hash, loom_proto::RAW_CODEC)?,
        mode,
    })
}
#[test]
fn files_modes_and_guest_symlinks_materialize_into_independent_copies() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let store = Store::memory()?;
    let manifest = image(
        &store,
        BTreeMap::from([
            ("bin".into(), VmImageEntry::Directory { mode: 0o755 }),
            ("bin/main".into(), file(&store, b"executable", 0o755)?),
            (
                "absolute".into(),
                VmImageEntry::Symlink {
                    target: "/etc/guest-only".into(),
                },
            ),
            (
                "main".into(),
                VmImageEntry::Symlink {
                    target: "bin/main".into(),
                },
            ),
        ]),
    )?;
    let directory = tempfile::tempdir()?;
    let first = directory.path().join("first");
    let second = directory.path().join("second");
    let images = StoreVmImages::new(store);
    images.materialize_image(&manifest, &first, 1024)?;
    images.materialize_image(&manifest, &second, 1024)?;
    assert_eq!(std::fs::read(first.join("bin/main"))?, b"executable");
    assert_eq!(
        std::fs::metadata(first.join("bin/main"))?
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert_eq!(
        std::fs::read_link(first.join("absolute"))?,
        std::path::Path::new("/etc/guest-only")
    );
    std::fs::write(first.join("bin/main"), b"changed")?;
    assert_eq!(std::fs::read(second.join("bin/main"))?, b"executable");
    Ok(())
}
#[test]
fn traversal_parent_aliases_and_special_modes_fail_before_creation() -> Result<()> {
    let store = Store::memory()?;
    let directory = tempfile::tempdir()?;
    let images = StoreVmImages::new(store.clone());
    for path in ["../outside", "/absolute", "a//b", "a/./b", "a/../b", "a\\b"] {
        let manifest = image(
            &store,
            BTreeMap::from([(path.into(), file(&store, b"bad", 0o644)?)]),
        )?;
        let dest = directory.path().join("new");
        assert!(images.materialize_image(&manifest, &dest, 1024).is_err());
        assert!(!dest.exists());
    }
    for parent in [
        VmImageEntry::Symlink {
            target: "/tmp".into(),
        },
        file(&store, b"not-directory", 0o644)?,
    ] {
        let manifest = image(
            &store,
            BTreeMap::from([
                ("a".into(), parent),
                ("a/file".into(), file(&store, b"bad", 0o644)?),
            ]),
        )?;
        assert!(
            images
                .materialize_image(&manifest, &directory.path().join("new"), 1024)
                .is_err()
        );
    }
    let manifest = image(
        &store,
        BTreeMap::from([("setuid".into(), file(&store, b"bad", 0o4755)?)]),
    )?;
    assert!(
        images
            .materialize_image(&manifest, &directory.path().join("new"), 1024)
            .is_err()
    );
    Ok(())
}
#[test]
fn tenant_scope_and_total_copy_budget_are_enforced() -> Result<()> {
    let alice = Store::memory()?;
    let bob = Store::memory()?;
    let raw = file(&alice, b"123456", 0o644)?;
    let entries = BTreeMap::from([("one".into(), raw.clone()), ("two".into(), raw)]);
    let manifest = image(&alice, entries.clone())?;
    let directory = tempfile::tempdir()?;
    assert!(
        StoreVmImages::new(bob.clone())
            .materialize_image(&manifest, &directory.path().join("wrong-tenant"), 1024)
            .is_err()
    );
    let bob_manifest = image(&bob, entries)?;
    assert!(
        StoreVmImages::new(bob)
            .materialize_image(&bob_manifest, &directory.path().join("foreign-file"), 1024)
            .is_err()
    );
    let images = StoreVmImages::new(alice);
    assert!(
        images
            .materialize_image(&manifest, &directory.path().join("too-large"), 11)
            .is_err()
    );
    images.materialize_image(&manifest, &directory.path().join("exact"), 12)?;
    assert!(
        images
            .materialize_image(&manifest, &directory.path().join("exact"), 12)
            .is_err()
    );
    Ok(())
}

#[test]
fn cancelled_blocking_materialization_retains_workspace_until_it_exits() -> Result<()> {
    use loom_actor::drivers::vm::{VmImageDestination, VmImages};
    use std::{sync::Arc, task::Poll, time::Duration};
    // Hold the only blocking worker. Polling the actual materializer once then
    // deterministically queues its owning closure without relying on Arc layout
    // or a scheduler timing window inside filesystem I/O.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()?;
    runtime.block_on(async {
        let store = Store::memory()?;
        let manifest = image(&store, BTreeMap::new())?;
        let workspace = Arc::new(tempfile::tempdir()?);
        let weak = Arc::downgrade(&workspace);
        let destination = VmImageDestination::new(workspace.clone());
        let images = StoreVmImages::new(store);
        let started = tokio::sync::oneshot::channel();
        let release = std::sync::mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || -> Result<()> {
            started
                .0
                .send(())
                .map_err(|_| anyhow::anyhow!("test receiver dropped"))?;
            release.1.recv()?;
            Ok(())
        });
        started.1.await?;
        let mut materialization = Box::pin(images.materialize(&manifest, &destination, 1024));
        assert!(matches!(
            futures_util::poll!(materialization.as_mut()),
            Poll::Pending
        ));
        drop(materialization);
        drop(destination);
        drop(workspace);
        assert!(
            weak.upgrade().is_some(),
            "queued exporter lost workspace owner"
        );
        release.0.send(())?;
        blocker.await??;
        tokio::time::timeout(Duration::from_secs(5), async {
            while weak.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        Ok(())
    })
}
