//! export/import round trips between two in-process stores. JavaScript
//! definitions need no Rust toolchain, so most cases run everywhere; the Rust
//! dependency case is gated like the other guest-toolchain tests.
use anyhow::{Context, Result};
use loom_api::Service;
use loom_proto::bundle::{Frame, decode_car, encode_car};
use loom_proto::{CommandRequest, DAG_CBOR_CODEC, Lang, RAW_CODEC, Response, Value, cid_for_hash};
use loom_store::Store;
use serde_json::json;
use std::{collections::BTreeMap, path::PathBuf};

const SUM: &str = "async function main(a, b) { return a + b; }";
const PRODUCT: &str = "async function main(a, b) { return a * b; }";

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}
/// JavaScript plus a Rust driver that cannot exist: any rustc use is a failure.
fn service(store: Store, languages: Vec<Lang>) -> Result<Service> {
    Ok(Service::new(store, root(), languages)?
        .with_driver_path(PathBuf::from("/nonexistent/loom-bundle-rustc")))
}
fn scripted() -> Result<(Store, Service)> {
    let store = Store::memory()?;
    let service = service(store.clone(), vec![Lang::Rust, Lang::JavaScript])?;
    Ok((store, service))
}
async fn command(service: &Service, name: &str, args: Value) -> Response {
    service
        .command(CommandRequest {
            session: None,
            command: name.into(),
            args,
        })
        .await
}
async fn ok(service: &Service, name: &str, args: Value) -> Value {
    let response = command(service, name, args).await;
    assert!(response.ok, "{name}: {response:?}");
    response.result
}
async fn failure(service: &Service, name: &str, args: Value) -> String {
    let response = command(service, name, args).await;
    assert!(!response.ok, "{name} succeeded: {response:?}");
    response.result["error"]
        .as_str()
        .unwrap_or_else(|| panic!("{name}: no error text: {response:?}"))
        .to_owned()
}
fn counts(store: &Store) -> Result<BTreeMap<String, i64>> {
    store.with_connection(|connection| {
        let names = connection
            .prepare("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        names
            .into_iter()
            .map(|name| {
                let sql = format!("SELECT count(*) FROM \"{}\"", name.replace('"', "\"\""));
                Ok((name, connection.query_row(&sql, [], |row| row.get(0))?))
            })
            .collect()
    })
}
/// Export on `source`, then read the bundle bytes back out of its CAS the way
/// an HTTP client would through `GET /v1/cas/{cid}`.
async fn export(source: &Service, targets: &[&str]) -> Result<(Value, Vec<u8>)> {
    let result = ok(source, "export", json!({"targets": targets})).await;
    let reference = result["bundle"]["$ref"]
        .as_str()
        .context("export result has no bundle reference")?;
    let bytes = source
        .store
        .get(reference)?
        .context("exported bundle missing from CAS")?;
    assert_eq!(result["bytes"], bytes.len());
    Ok((result, bytes))
}
/// Upload as `POST /v1/cas` would, then import.
async fn import(target: &Service, bytes: &[u8], into: Option<&str>) -> Result<Response> {
    let hash = target.store.put("bundle", bytes)?;
    let reference = target.store.reference(&hash, loom_proto::RAW_CODEC)?;
    Ok(command(
        target,
        "import",
        json!({"bundle": reference["$ref"], "into": into}),
    )
    .await)
}
/// A DAG-CBOR block with its BLAKE3 CID, as the exporter would frame it.
fn dag(value: &Value) -> Frame {
    let bytes = loom_proto::encode(value).unwrap();
    Frame {
        cid: cid_for_hash(&loom_store::content_hash(&bytes), DAG_CBOR_CODEC).unwrap(),
        bytes,
    }
}
/// A raw block with its BLAKE3 CID.
fn raw(bytes: &[u8]) -> Frame {
    Frame {
        cid: cid_for_hash(&loom_store::content_hash(bytes), RAW_CODEC).unwrap(),
        bytes: bytes.to_vec(),
    }
}
/// Split a bundle into its decoded root and the frames after it, in file
/// order (records ascending by hash, then objects ascending by CID).
fn open(bytes: &[u8]) -> (Value, Vec<Frame>) {
    let car = decode_car(bytes).unwrap();
    let mut frames = car.frames;
    let root = frames.remove(0);
    assert_eq!(car.roots, vec![root.cid.clone()]);
    (loom_proto::decode(&root.bytes).unwrap(), frames)
}
/// Frame `root` first and name it in the header: a bundle whose bytes all
/// verify, so a refusal comes from the content checks, never the container.
fn close(root: &Value, frames: &[Frame]) -> Vec<u8> {
    let root = dag(root);
    let mut all = vec![root.clone()];
    all.extend(frames.iter().cloned());
    encode_car(&root.cid, &all).unwrap()
}
/// Everything the live store holds, minus the bundle object `import` uploaded.
fn counts_without_upload(store: &Store) -> Result<BTreeMap<String, i64>> {
    let mut after = counts(store)?;
    *after.get_mut("cas").context("cas table")? -= 1;
    *after.get_mut("cas_codecs").context("cas_codecs table")? -= 1;
    Ok(after)
}
/// Add `sum` on a fresh node and export it: the smallest valid bundle.
async fn exported_sum() -> Result<(String, Vec<u8>)> {
    let (_, source) = scripted()?;
    let added = ok(
        &source,
        "add",
        json!({"name":"sum","lang":"javascript","source":SUM}),
    )
    .await;
    let hash = added["hash"].as_str().context("hash")?.to_owned();
    let (_, bytes) = export(&source, &["sum"]).await?;
    Ok((hash, bytes))
}

#[tokio::test]
async fn export_import_preserves_hash_source_and_binds_names_with_or_without_prefix() -> Result<()>
{
    let (_, source) = scripted()?;
    let added = ok(
        &source,
        "add",
        json!({"name":"sum","lang":"javascript","source":SUM}),
    )
    .await;
    let hash = added["hash"].as_str().context("hash")?.to_owned();
    let (result, bytes) = export(&source, &["sum"]).await?;
    assert_eq!(result["roots"], json!({"sum": hash}));
    assert_eq!(result["definitions"], 1);
    // A script's record links only its source: no identity, preimages or trees.
    assert_eq!(result["objects"], 1);
    let verified = loom_store::verify_bundle(&bytes)?;
    assert_eq!(verified.root, result["root"]);
    assert_eq!(verified.blocks.len(), 3, "root, one record, one source");
    assert_eq!(verified.blocks[2].bytes, SUM.as_bytes());

    let (target_store, target) = scripted()?;
    let imported = import(&target, &bytes, Some("friend")).await?;
    assert!(imported.ok, "{imported:?}");
    assert_eq!(
        imported.result["imported"],
        json!([{"name":"friend/sum","hash":hash}])
    );
    assert_eq!(imported.result["prefix"], "friend/");
    assert_eq!(target_store.resolve("friend/sum")?.unwrap().hash, hash);
    assert_eq!(target_store.source(&hash)?.as_deref(), Some(SUM));
    assert!(target_store.resolve("sum")?.is_none());
    let viewed = ok(&target, "view", json!({"target":"friend/sum"})).await;
    assert_eq!(viewed["hash"], hash);
    assert_eq!(
        ok(
            &target,
            "run",
            json!({"target":"friend/sum","args":[20,22]})
        )
        .await["output"],
        42
    );
    // Without a prefix the bundle's own name binds; repeating is a no-op.
    let bare = import(&target, &bytes, None).await?;
    assert!(bare.ok, "{bare:?}");
    assert_eq!(target_store.resolve("sum")?.unwrap().hash, hash);
    let before = target_store.name_history("sum")?.len();
    let again = import(&target, &bytes, None).await?;
    assert!(again.ok, "{again:?}");
    assert_eq!(target_store.name_history("sum")?.len(), before);
    Ok(())
}

#[tokio::test]
async fn corrupted_block_is_rejected_before_anything_is_written() -> Result<()> {
    let (_, source) = scripted()?;
    ok(
        &source,
        "add",
        json!({"name":"sum","lang":"javascript","source":SUM}),
    )
    .await;
    let (_, mut bytes) = export(&source, &["sum"]).await?;
    // The final byte belongs to the last block: the raw source object.
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    let (target_store, target) = scripted()?;
    let hash = target_store.put("bundle", &bytes)?;
    let before = counts(&target_store)?;
    let refused = command(&target, "import", json!({"bundle": hash})).await;
    assert!(!refused.ok, "{refused:?}");
    let error = refused.result["error"].as_str().unwrap();
    assert!(error.contains("corrupt"), "{error}");
    assert_eq!(
        counts(&target_store)?,
        before,
        "a refused import writes nothing"
    );
    let stray = command(&target, "import", json!({"bundle": "0".repeat(64)})).await;
    assert!(
        stray.result["error"]
            .as_str()
            .unwrap()
            .contains("POST /v1/cas"),
        "{stray:?}"
    );
    Ok(())
}

#[tokio::test]
async fn name_bound_to_a_different_hash_refuses_the_import_unless_prefixed() -> Result<()> {
    let (_, source) = scripted()?;
    let added = ok(
        &source,
        "add",
        json!({"name":"sum","lang":"javascript","source":SUM}),
    )
    .await;
    let (_, bytes) = export(&source, &["sum"]).await?;
    let (target_store, target) = scripted()?;
    let local = ok(
        &target,
        "add",
        json!({"name":"sum","lang":"javascript","source":PRODUCT}),
    )
    .await;
    assert_ne!(local["hash"], added["hash"]);
    let before = counts(&target_store)?;
    let refused = import(&target, &bytes, None).await?;
    assert!(!refused.ok, "{refused:?}");
    let error = refused.result["error"].as_str().unwrap();
    assert!(
        error.contains("import conflict")
            && error.contains("\"sum\"")
            && error.contains(local["hash"].as_str().unwrap())
            && error.contains(added["hash"].as_str().unwrap()),
        "{error}"
    );
    let mut after = counts(&target_store)?;
    // Only the bundle object the test uploaded may differ.
    *after.get_mut("cas").unwrap() -= 1;
    *after.get_mut("cas_codecs").unwrap() -= 1;
    assert_eq!(after, before);
    assert_eq!(target_store.resolve("sum")?.unwrap().hash, local["hash"]);
    let prefixed = import(&target, &bytes, Some("friend")).await?;
    assert!(prefixed.ok, "{prefixed:?}");
    assert_eq!(
        target_store.resolve("friend/sum")?.unwrap().hash,
        added["hash"]
    );
    assert_eq!(target_store.resolve("sum")?.unwrap().hash, local["hash"]);
    Ok(())
}

#[tokio::test]
async fn import_admits_through_the_add_path_so_disabled_languages_refuse() -> Result<()> {
    let (_, source) = scripted()?;
    ok(
        &source,
        "add",
        json!({"name":"sum","lang":"javascript","source":SUM}),
    )
    .await;
    let (target_store, target) = {
        let store = Store::memory()?;
        (store.clone(), service(store, vec![Lang::Rust])?)
    };
    let (_, bytes) = export(&source, &["sum"]).await?;
    let refused = import(&target, &bytes, None).await?;
    assert!(!refused.ok, "{refused:?}");
    assert!(
        refused.result["error"]
            .as_str()
            .unwrap()
            .contains("language javascript is disabled"),
        "{refused:?}"
    );
    assert!(target_store.resolve("sum")?.is_none());
    Ok(())
}

#[tokio::test]
async fn export_refuses_unknown_targets_and_hashes_without_a_current_name() -> Result<()> {
    let (store, service) = scripted()?;
    let error = failure(&service, "export", json!({"targets":["nope"]})).await;
    assert!(
        error.contains("\"nope\"") && error.contains("not found"),
        "{error}"
    );
    let unnamed = loom_proto::Def {
        hash: store.put("item-preimage", b"unnamed entry")?,
        lang: Lang::Rust,
        component_hash: None,
        sig: Default::default(),
        allowed_effects: None,
        observed_effects: Vec::new(),
    };
    store.define(&unnamed, None, "pub fn main() {}", &BTreeMap::new())?;
    let error = failure(&service, "export", json!({"targets":[unnamed.hash]})).await;
    assert!(error.contains("without a current name"), "{error}");
    // A hash that is the current value of a name exports under that name.
    let added = ok(
        &service,
        "add",
        json!({"name":"sum","lang":"javascript","source":SUM}),
    )
    .await;
    let (result, _) = export(&service, &[added["hash"].as_str().unwrap()]).await?;
    assert_eq!(result["roots"], json!({"sum": added["hash"]}));
    Ok(())
}

#[tokio::test]
async fn add_resolves_dependency_names_on_the_server_before_any_build() -> Result<()> {
    let (store, service) = scripted()?;
    let before = counts(&store)?;
    let error = failure(
        &service,
        "add",
        json!({"name":"caller","lang":"rust","source":"pub fn main() -> i64 { util::twice(21) }","deps":{"util":"missing"}}),
    )
    .await;
    assert!(
        error.contains("dependency util") && error.contains("\"missing\""),
        "{error}"
    );
    let absent = "f".repeat(64);
    let error = failure(
        &service,
        "add",
        json!({"name":"caller","lang":"rust","source":"pub fn main() -> i64 { util::twice(21) }","deps":{"util":absent}}),
    )
    .await;
    assert!(error.contains(&absent), "{error}");
    // Neither attempt reached the (nonexistent) compiler driver or wrote rows.
    assert!(!error.contains("loom-bundle-rustc"), "{error}");
    assert_eq!(counts(&store)?, before);
    Ok(())
}

/// A Rust record whose `preparation.key` this node never derives for its
/// source is refused before any compiler runs (the driver path here does not
/// exist), and the live resolver cache is left exactly as it was.
#[tokio::test]
async fn preparation_key_this_node_does_not_derive_is_refused_before_any_build() -> Result<()> {
    let source = raw(b"pub fn main() -> i64 { 42 }");
    let items = raw(br#"{"items":{}}"#);
    let overlay = dag(&json!({"Cargo.lock": "version = 4\n"}));
    let hash = "1a".repeat(32);
    let key = "2b".repeat(32);
    let record = dag(&json!({
        "loom_definition": 1,
        "hash": hash,
        "lang": "rust",
        "names": ["answer"],
        "source": {"$ref": source.cid},
        "deps": {},
        "allowed_effects": null,
        "sig": loom_proto::TypeSig::default(),
        "identity": {
            "behavior_hash": hash,
            "wasm_hash": "3c".repeat(32),
            "toolchain_hash": "4d".repeat(32),
            "item_hashes": {"$ref": items.cid},
        },
        "preimages": [],
        "trees": [],
        "preparation": {"key": key, "overlay": {"$ref": overlay.cid}},
    }));
    let mut root = json!({
        "loom_bundle": 1,
        "roots": {"answer": hash},
        "definitions": {},
        "objects": [
            {"cid": {"$ref": source.cid}, "kind": "source_bundle"},
            {"cid": {"$ref": items.cid}, "kind": "item-hashes"},
            {"cid": {"$ref": overlay.cid}, "kind": "rust-prepared-dependencies"},
        ],
    });
    root["definitions"][hash.as_str()] = json!({"$ref": record.cid});
    let bytes = close(&root, &[record, source, items, overlay]);
    let (target_store, target) = scripted()?;
    let before = counts(&target_store)?;
    let refused = import(&target, &bytes, None).await?;
    assert!(!refused.ok, "{refused:?}");
    let error = refused.result["error"].as_str().unwrap();
    assert!(
        error.contains("preparation key") && error.contains(&key) && error.contains(&hash),
        "{error}"
    );
    assert!(
        !error.contains("loom-bundle-rustc") && !error.contains("rebuild"),
        "refused before the compiler, not by it: {error}"
    );
    assert_eq!(
        counts_without_upload(&target_store)?,
        before,
        "no resolver row, object or name reached the live store"
    );
    let seeded: bool = target_store.with_connection(|connection| {
        Ok(connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='rust_preparations')",
            [],
            |row| row.get(0),
        )?)
    })?;
    assert!(
        !seeded,
        "the live rust_preparations table was never created"
    );
    assert!(target_store.resolve("answer")?.is_none());
    Ok(())
}

/// A record carrying an overlay whose vendor tree is not among its `trees`
/// is refused by the content checks, before any store is touched.
#[tokio::test]
async fn preparation_overlay_vendor_tree_must_travel_in_the_record_trees() -> Result<()> {
    let source = raw(b"pub fn main() -> i64 { 42 }");
    let items = raw(br#"{"items":{}}"#);
    let vendor_tree = "5e".repeat(32);
    let overlay = dag(&json!({"Cargo.lock": "version = 4\n", "loom.vendor-tree": vendor_tree}));
    let hash = "1a".repeat(32);
    let record = dag(&json!({
        "loom_definition": 1,
        "hash": hash,
        "lang": "rust",
        "names": ["answer"],
        "source": {"$ref": source.cid},
        "deps": {},
        "allowed_effects": null,
        "sig": loom_proto::TypeSig::default(),
        "identity": {
            "behavior_hash": hash,
            "wasm_hash": "3c".repeat(32),
            "toolchain_hash": "4d".repeat(32),
            "item_hashes": {"$ref": items.cid},
        },
        "preimages": [],
        "trees": [],
        "preparation": {"key": "2b".repeat(32), "overlay": {"$ref": overlay.cid}},
    }));
    let mut root = json!({
        "loom_bundle": 1,
        "roots": {"answer": hash},
        "definitions": {},
        "objects": [
            {"cid": {"$ref": source.cid}, "kind": "source_bundle"},
            {"cid": {"$ref": items.cid}, "kind": "item-hashes"},
            {"cid": {"$ref": overlay.cid}, "kind": "rust-prepared-dependencies"},
        ],
    });
    root["definitions"][hash.as_str()] = json!({"$ref": record.cid});
    let bytes = close(&root, &[record, source, items, overlay]);
    let (target_store, target) = scripted()?;
    let before = counts(&target_store)?;
    let refused = import(&target, &bytes, None).await?;
    assert!(!refused.ok, "{refused:?}");
    let error = refused.result["error"].as_str().unwrap();
    let tree_cid = cid_for_hash(&vendor_tree, DAG_CBOR_CODEC).unwrap();
    assert!(
        error.contains("vendor tree") && error.contains(&tree_cid) && error.contains(&hash),
        "{error}"
    );
    assert_eq!(counts_without_upload(&target_store)?, before);
    Ok(())
}

/// An object the root lists but no record reaches is refused, naming it; the
/// exporter never writes such a bundle, so it is always a foreign object
/// trying to ride into the live CAS.
#[tokio::test]
async fn listed_object_no_record_references_is_refused() -> Result<()> {
    let (hash, bytes) = exported_sum().await?;
    let (mut root, mut frames) = open(&bytes);
    let stray = raw(b"stray bytes no record links");
    root["objects"]
        .as_array_mut()
        .unwrap()
        .push(json!({"cid": {"$ref": stray.cid}, "kind": "blob"}));
    frames.push(stray.clone());
    let bytes = close(&root, &frames);
    // The container itself is sound: the refusal is the reference check.
    loom_store::verify_bundle(&bytes)?;
    let (target_store, target) = scripted()?;
    let before = counts(&target_store)?;
    let refused = import(&target, &bytes, None).await?;
    assert!(!refused.ok, "{refused:?}");
    let error = refused.result["error"].as_str().unwrap();
    assert!(
        error.contains(&stray.cid) && error.contains("references"),
        "{error}"
    );
    assert_eq!(counts_without_upload(&target_store)?, before);
    assert!(target_store.resolve("sum")?.is_none());
    assert!(target_store.definition(&hash)?.is_none());
    assert!(
        target_store
            .cas_entry(&loom_store::content_hash(&stray.bytes))?
            .is_none(),
        "the stray object never reached the live CAS"
    );
    Ok(())
}

/// Twenty thousand chained records: the definition cap refuses the root
/// before any record is decoded or ordered, so the chain's depth never
/// reaches the stack.
#[tokio::test]
async fn chain_of_twenty_thousand_records_is_refused_by_the_definition_cap() -> Result<()> {
    let length = 20_000;
    let source = raw(SUM.as_bytes());
    let mut frames = Vec::with_capacity(length + 1);
    let mut definitions = serde_json::Map::new();
    let mut previous: Option<String> = None;
    let mut last = String::new();
    for index in 0..length {
        let hash = format!("{index:064x}");
        let deps = match &previous {
            Some(previous) => json!({"prev": previous}),
            None => json!({}),
        };
        let record = dag(&json!({
            "loom_definition": 1,
            "hash": hash,
            "lang": "javascript",
            "names": [],
            "source": {"$ref": source.cid},
            "deps": deps,
            "allowed_effects": null,
            "sig": loom_proto::TypeSig::default(),
            "identity": null,
            "preimages": [],
            "trees": [],
            "preparation": null,
        }));
        definitions.insert(hash.clone(), json!({"$ref": record.cid}));
        frames.push(record);
        previous = Some(hash.clone());
        last = hash;
    }
    frames.push(source.clone());
    let root = json!({
        "loom_bundle": 1,
        "roots": {"tip": last},
        "definitions": definitions,
        "objects": [{"cid": {"$ref": source.cid}, "kind": "source_bundle"}],
    });
    let bytes = close(&root, &frames);
    let (target_store, target) = scripted()?;
    let before = counts(&target_store)?;
    let started = std::time::Instant::now();
    let refused = import(&target, &bytes, None).await?;
    let elapsed = started.elapsed();
    assert!(!refused.ok, "{refused:?}");
    let error = refused.result["error"].as_str().unwrap();
    assert!(
        error.contains("20000 definitions") && error.contains("1024"),
        "{error}"
    );
    // Hashing 20k blocks and one comparison: nothing here scales with depth.
    assert!(elapsed.as_secs() < 30, "cap refusal took {elapsed:?}");
    assert_eq!(counts_without_upload(&target_store)?, before);
    Ok(())
}

/// A record whose recorded hash is not what its source rebuilds to is refused
/// by the rebuilt-hash check, the error naming both hashes, and nothing the
/// staged rebuild produced reaches the live store.
#[tokio::test]
async fn record_hash_altered_after_export_is_refused_by_the_rebuilt_hash_check() -> Result<()> {
    let (real, bytes) = exported_sum().await?;
    let (mut root, mut frames) = open(&bytes);
    let forged = "f".repeat(63) + "0";
    assert_ne!(forged, real);
    // Block order: the one record, then the source object.
    let mut record: Value = loom_proto::decode(&frames[0].bytes).unwrap();
    assert_eq!(record["hash"], real);
    record["hash"] = json!(forged);
    let record = dag(&record);
    frames[0] = record.clone();
    root["definitions"] = json!({});
    root["definitions"][forged.as_str()] = json!({"$ref": record.cid});
    root["roots"]["sum"] = json!(forged);
    let bytes = close(&root, &frames);
    let (target_store, target) = scripted()?;
    let before = counts(&target_store)?;
    let refused = import(&target, &bytes, None).await?;
    assert!(!refused.ok, "{refused:?}");
    let error = refused.result["error"].as_str().unwrap();
    assert!(
        error.contains("import refused")
            && error.contains(&format!("produced definition {real}"))
            && error.contains(&format!("records {forged}"))
            && error.contains("none (script identity)"),
        "{error}"
    );
    assert_eq!(
        counts_without_upload(&target_store)?,
        before,
        "the staged rebuild of {real} was discarded with the staged store"
    );
    assert!(target_store.definition(&real)?.is_none());
    assert!(target_store.resolve("sum")?.is_none());
    Ok(())
}

/// Names a bundle binds pass the same rule as `--into`: a leading `#` would
/// bind a name `Store::resolve` strips before looking up, and a hash-shaped
/// name would be shadowed by the definition it spells.
#[tokio::test]
async fn root_names_and_into_starting_with_hash_sign_are_refused() -> Result<()> {
    let (hash, bytes) = exported_sum().await?;
    let (target_store, target) = scripted()?;
    let before = counts(&target_store)?;
    let (mut root, frames) = open(&bytes);
    root["roots"] = json!({"#sum": hash});
    let refused = import(&target, &close(&root, &frames), None).await?;
    assert!(!refused.ok, "{refused:?}");
    let error = refused.result["error"].as_str().unwrap();
    assert!(
        error.contains("\"#sum\"") && error.contains("'#'"),
        "{error}"
    );
    root["roots"] = json!({});
    root["roots"][hash.as_str()] = json!(hash);
    let refused = import(&target, &close(&root, &frames), None).await?;
    assert!(!refused.ok, "{refused:?}");
    let error = refused.result["error"].as_str().unwrap();
    assert!(error.contains("shaped like a definition hash"), "{error}");
    // The unmodified bundle under a `#` prefix is refused by the same rule.
    let refused = import(&target, &bytes, Some("#friend")).await?;
    assert!(!refused.ok, "{refused:?}");
    let error = refused.result["error"].as_str().unwrap();
    assert!(
        error.contains("import prefix") && error.contains("'#'"),
        "{error}"
    );
    // Three uploads, nothing else.
    let mut after = counts(&target_store)?;
    *after.get_mut("cas").unwrap() -= 3;
    *after.get_mut("cas_codecs").unwrap() -= 3;
    assert_eq!(after, before);
    assert!(target_store.current_names()?.is_empty());
    Ok(())
}

/// Real toolchain: a caller pinned to a dependency by NAME round-trips with
/// both hashes, its pins, and its output intact on a fresh node.
#[tokio::test]
#[ignore = "requires Rust guest toolchain and LOOM_COMPILER_CACHE_OWNER"]
async fn rust_caller_and_dependency_round_trip_with_identical_hashes() -> Result<()> {
    let source_store = Store::memory()?;
    let source = Service::new(source_store.clone(), root(), vec![Lang::Rust])?;
    let util = ok(
        &source,
        "add",
        json!({"name":"util","lang":"rust","source":"pub fn twice(value: i64) -> i64 { value * 2 }"}),
    )
    .await;
    let caller = ok(
        &source,
        "add",
        json!({"name":"caller","lang":"rust","source":"pub fn main() -> i64 { util::twice(21) }","deps":{"util":"util"}}),
    )
    .await;
    let util_hash = util["hash"].as_str().unwrap().to_owned();
    let caller_hash = caller["hash"].as_str().unwrap().to_owned();
    assert_eq!(
        source_store.definition_deps(&caller_hash)?,
        BTreeMap::from([("util".to_owned(), util_hash.clone())]),
        "the name resolved to the current hash at admission"
    );
    assert_eq!(
        ok(&source, "run", json!({"target":"caller"})).await["output"],
        42
    );
    let (result, bytes) = export(&source, &["caller"]).await?;
    assert_eq!(result["definitions"], 2, "the dependency closure travels");

    let target_store = Store::memory()?;
    let target = Service::new(target_store.clone(), root(), vec![Lang::Rust])?;
    let imported = import(&target, &bytes, Some("friend")).await?;
    assert!(imported.ok, "{imported:?}");
    assert_eq!(
        target_store.resolve("friend/caller")?.unwrap().hash,
        caller_hash
    );
    assert_eq!(
        target_store.definition_deps(&caller_hash)?,
        BTreeMap::from([("util".to_owned(), util_hash.clone())])
    );
    assert!(target_store.definition(&util_hash)?.is_some());
    assert!(
        !target_store
            .current_names()?
            .values()
            .any(|hash| hash == &util_hash),
        "only roots bind names"
    );
    assert_eq!(
        target_store
            .build_identity(&caller_hash)?
            .unwrap()
            .toolchain_hash,
        source_store
            .build_identity(&caller_hash)?
            .unwrap()
            .toolchain_hash
    );
    assert_eq!(
        ok(&target, "run", json!({"target":"friend/caller"})).await["output"],
        42
    );
    Ok(())
}
