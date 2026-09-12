use loom_api::Service;
use loom_proto::{CommandRequest, Lang, Value};
use loom_store::Store;
use serde_json::json;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

async fn command(service: &Service, command: &str, args: Value) -> Value {
    let response = service
        .command(CommandRequest {
            session: None,
            command: command.into(),
            args,
        })
        .await;
    assert!(response.ok, "{response:?}");
    response.result
}

#[tokio::test]
#[ignore = "requires Rust guest toolchain and LOOM_COMPILER_CACHE_OWNER"]
async fn rejected_republication_keeps_spawned_actor_executable_pinned() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let service = Service::new(
        Store::memory()?,
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        vec![Lang::Rust],
    )?;
    let node = loom_actor::Node::new(
        directory.path().join("actors"),
        service.actor_registry(),
        Arc::new(loom_actor::DefaultEffects),
        loom_actor::Config::default(),
    )
    .await?;
    let service = service.with_actors(node.clone());
    let source = r#"
pub const LOOM_SCHEMA: &str = "CREATE TABLE arrivals(value INTEGER)";
pub fn handle(_message: Vec<u8>) {
    let args: loom::serde_json::Value = loom::serde_json::from_str("{\"sql\":\"INSERT INTO arrivals VALUES (42)\",\"params\":[]}").unwrap();
    let _: loom::serde_json::Value = loom::perform("sql", args).unwrap();
}
"#;
    let added = command(
        &service,
        "add",
        json!({"name":"pinned-actor","source":source}),
    )
    .await;
    let hash = added["hash"].as_str().unwrap();
    assert_eq!(added["hash"], added["behavior_hash"]);
    let spawned = command(&service, "spawn", json!({"def":hash,"init":{}})).await;
    let id = spawned["id"].as_str().unwrap();
    node.run_until_idle().await?;
    assert_eq!(node.info(id).await?.behavior_hash, hash);
    let original = service.store.definition(hash)?.unwrap();
    let changed_schema_source = source.replace(
        "CREATE TABLE arrivals(value INTEGER)",
        "CREATE TABLE arrivals(value INTEGER, extra TEXT)",
    );
    let updated = command(
        &service,
        "update",
        json!({"name":"pinned-actor","source":changed_schema_source}),
    )
    .await;
    assert_ne!(
        updated["hash"], added["hash"],
        "schema belongs to the entry contract"
    );
    assert_eq!(
        service.store.resolve("pinned-actor")?.unwrap().hash,
        updated["hash"].as_str().unwrap()
    );
    assert_eq!(node.info(id).await?.behavior_hash, hash);
    let identity = service.store.build_identity(hash)?.unwrap();
    let stored_source = service.store.source(hash)?.unwrap();
    let mut changed = original.clone();
    changed.component_hash = Some(service.store.put("component", b"different executable")?);
    let error = service
        .store
        .define_with_identity(
            &changed,
            None,
            &stored_source,
            &BTreeMap::new(),
            Some(&identity),
        )
        .unwrap_err()
        .to_string();
    assert!(
        error.contains(original.component_hash.as_ref().unwrap()),
        "{error}"
    );
    assert!(
        error.contains(changed.component_hash.as_ref().unwrap()),
        "{error}"
    );
    changed = original.clone();
    changed.sig.exports[0].returns = loom_proto::ValueShape::String;
    let old_schema = service
        .store
        .put("schema", &serde_json::to_vec(&original.sig)?)?;
    let new_schema = service
        .store
        .put("schema", &serde_json::to_vec(&changed.sig)?)?;
    let error = service
        .store
        .define_with_identity(
            &changed,
            None,
            &stored_source,
            &BTreeMap::new(),
            Some(&identity),
        )
        .unwrap_err()
        .to_string();
    assert!(
        error.contains(&old_schema) && error.contains(&new_schema),
        "{error}"
    );
    command(&service, "send", json!({"id":id,"msg":{}})).await;
    let rows = node
        .open(id)
        .await?
        .inspect_sql("SELECT * FROM arrivals", Vec::new())
        .await?;
    assert_eq!(rows.rows.len(), 2);
    assert_eq!(
        rows.columns,
        vec!["value".to_owned()],
        "existing actor retains its original schema"
    );
    for row in rows.rows {
        assert_eq!(row.get::<i64>(0)?, 42);
    }
    assert_eq!(
        service.store.definition(hash)?.unwrap().component_hash,
        original.component_hash
    );
    Ok(())
}
