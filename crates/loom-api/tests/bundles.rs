//! export/import round trips between two in-process stores. JavaScript
//! definitions need no Rust toolchain, so most cases run everywhere; the Rust
//! dependency case is gated like the other guest-toolchain tests.
use anyhow::{Context, Result};
use loom_api::Service;
use loom_proto::{CommandRequest, Lang, Response, Value};
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
