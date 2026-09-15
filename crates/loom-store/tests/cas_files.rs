use anyhow::{Context, Result};
use loom_store::{CAS_GUEST_MAX_BYTES, Store};
use serde_json::json;

#[test]
fn streaming_file_roundtrip_reopens_and_refuses_corruption() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("cas.sqlite");
    let source = directory.path().join("source");
    let bytes = vec![0xa5; 2 * 1024 * 1024 + 7];
    std::fs::write(&source, &bytes)?;
    let store = Store::open(&database)?;
    let hash = store.put_file("vm-disk", &source)?;
    assert_eq!(store.put_file("vm-disk", &source)?, hash);
    let reference = store.reference(&hash, loom_proto::RAW_CODEC)?;
    let cid = reference["$ref"].as_str().context("missing CID")?;
    drop(store);
    let store = Store::open(&database)?;
    let destination = directory.path().join("image");
    store.export_file(cid, &destination)?;
    assert_eq!(std::fs::read(&destination)?, bytes);
    assert!(store.export_file(cid, &destination).is_err());
    store.with_connection(|connection| {
        connection.execute("UPDATE cas SET bytes=x'00' WHERE hash=?", [&hash])?;
        Ok(())
    })?;
    let corrupt = directory.path().join("corrupt");
    assert!(store.export_file(cid, &corrupt).is_err());
    assert!(!corrupt.exists());
    assert!(store.get(cid).is_err());
    Ok(())
}

#[test]
fn guest_cas_enforces_tenant_codec_integrity_and_byte_bounds() -> Result<()> {
    let alice = Store::memory()?;
    let bob = Store::memory()?;
    let reference = alice.guest_cas_effect("cas.put_bytes", json!({"bytes":[0,127,255]}))?;
    assert_eq!(
        alice.guest_cas_effect("cas.get_bytes", json!({"reference":reference}))?,
        json!([0, 127, 255])
    );
    assert!(
        bob.guest_cas_effect("cas.get_bytes", json!({"reference":reference}))
            .is_err()
    );
    assert!(
        alice
            .guest_cas_effect("cas.get_bytes", json!({"reference":{"$ref":"bad"}}))
            .is_err()
    );
    let structured = alice.guest_cas_effect("cas.put", json!({"value":42}))?;
    assert!(
        alice
            .guest_cas_effect("cas.get_bytes", json!({"reference":structured}))
            .is_err()
    );
    assert!(
        alice
            .guest_cas_effect("cas.get", json!({"hash":reference["$ref"]}))
            .is_err()
    );
    let maximal = vec![255u8; CAS_GUEST_MAX_BYTES];
    let maximal_ref = alice.guest_cas_effect("cas.put_bytes", json!({"bytes":maximal}))?;
    assert_eq!(
        alice
            .guest_cas_effect("cas.get_bytes", json!({"reference":maximal_ref}))?
            .as_array()
            .unwrap()
            .len(),
        CAS_GUEST_MAX_BYTES
    );
    assert!(
        alice
            .guest_cas_effect(
                "cas.put_bytes",
                json!({"bytes":vec![255u8;CAS_GUEST_MAX_BYTES+1]})
            )
            .is_err()
    );
    let hash = alice.put("blob", &vec![1; CAS_GUEST_MAX_BYTES + 1])?;
    let oversized = alice.reference(&hash, loom_proto::RAW_CODEC)?;
    assert!(
        alice
            .guest_cas_effect("cas.get_bytes", json!({"reference":oversized}))
            .is_err()
    );
    Ok(())
}

#[test]
fn streaming_export_respects_cancellation_and_empty_files() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("empty");
    std::fs::write(&source, [])?;
    let store = Store::memory()?;
    let hash = store.put_file("blob", &source)?;
    let reference = store.reference(&hash, loom_proto::RAW_CODEC)?;
    let cid = reference["$ref"].as_str().unwrap();
    let target = directory.path().join("output");
    let cancelled = std::sync::atomic::AtomicBool::new(true);
    assert!(
        store
            .export_file_cancellable(cid, &target, &cancelled)
            .is_err()
    );
    assert!(!target.exists());
    cancelled.store(false, std::sync::atomic::Ordering::Release);
    store.export_file_cancellable(cid, &target, &cancelled)?;
    assert!(std::fs::read(&target)?.is_empty());
    Ok(())
}
