use anyhow::{Context, Result};
use loom_proto::{Def, Lang};
use loom_store::Store;
use std::collections::BTreeMap;

const ABI: &str = "test-v8/1";
const SOURCE: &str = "async function main() { return 42; }";

fn publish(store: &Store) -> Result<String> {
    let identity = loom_proto::javascript_definition_identity(
        SOURCE,
        &BTreeMap::new(),
        Some(&["sql".into()]),
        ABI,
    )?;
    let hash = store.put("javascript_definition", &identity)?;
    let artifact = store.put("javascript_source", SOURCE.as_bytes())?;
    store.define(
        &Def {
            hash: hash.clone(),
            lang: Lang::JavaScript,
            component_hash: Some(artifact),
            sig: Default::default(),
            allowed_effects: Some(vec!["sql".into()]),
            observed_effects: Vec::new(),
        },
        Some("main"),
        SOURCE,
        &BTreeMap::new(),
    )?;
    Ok(hash)
}

#[test]
fn javascript_reopen_checks_engine_abi_and_policy() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("store.sqlite");
    let store = Store::open(&path)?;
    let hash = publish(&store)?;
    drop(store);
    let store = Store::open(&path)?;
    assert_eq!(store.javascript_source(&hash, ABI)?, SOURCE);
    assert!(
        store
            .javascript_source(&hash, "test-v8/2")
            .unwrap_err()
            .to_string()
            .contains("identity mismatch")
    );
    store.with_connection(|connection| {
        connection.execute("UPDATE defs SET allowed_effects=NULL WHERE hash=?", [&hash])?;
        Ok(())
    })?;
    assert!(
        store
            .javascript_source(&hash, ABI)
            .unwrap_err()
            .to_string()
            .contains("identity mismatch")
    );
    Ok(())
}

#[test]
fn javascript_display_source_revisions_do_not_replace_executable() -> Result<()> {
    let store = Store::memory()?;
    let hash = publish(&store)?;
    let definition = store.definition(&hash)?.context("definition missing")?;
    store.define(
        &definition,
        Some("main"),
        "display source changed",
        &BTreeMap::new(),
    )?;
    assert_eq!(
        store.source(&hash)?.as_deref(),
        Some("display source changed")
    );
    assert_eq!(store.javascript_source(&hash, ABI)?, SOURCE);
    Ok(())
}

#[test]
fn javascript_rejects_corrupt_or_replaced_executable() -> Result<()> {
    for corrupt_bytes in [false, true] {
        let store = Store::memory()?;
        let hash = publish(&store)?;
        let replacement = store.put("javascript_source", b"async function main() { return 0; }")?;
        store.with_connection(|connection| {
            if corrupt_bytes {
                connection.execute("UPDATE cas SET bytes=? WHERE hash=(SELECT component_hash FROM defs WHERE hash=?)", rusqlite::params![b"corrupt".as_slice(), hash])?;
            } else {
                connection.execute("UPDATE defs SET component_hash=? WHERE hash=?", rusqlite::params![replacement, hash])?;
            }
            Ok(())
        })?;
        assert!(store.javascript_source(&hash, ABI).is_err());
    }
    Ok(())
}
