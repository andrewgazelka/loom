use super::*;

async fn service(directory: &std::path::Path) -> anyhow::Result<crate::Service> {
    let service = crate::Service::new(
        loom_store::Store::memory()?,
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        vec![loom_proto::Lang::Rust],
    )?;
    let node = Node::new(
        directory.join("actors"),
        service.actor_registry(),
        std::sync::Arc::new(loom_actor::DefaultEffects),
        loom_actor::Config::default(),
    )
    .await?;
    Ok(service.with_actors(node))
}

#[tokio::test]
async fn dynamic_actor_unknown_name_is_named() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let service = service(directory.path()).await?;
    let error = service
        .actor_command(
            "actor_spawn",
            json!({"behavior_hash":"missing-actor","init":null}),
        )
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("missing-actor"), "{error:#}");
    Ok(())
}

#[tokio::test]
#[ignore = "requires Rust guest toolchain and LOOM_COMPILER_CACHE_OWNER"]
async fn dynamic_actor_add_then_spawn_on_running_node() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let service = service(directory.path()).await?;
    let source = r#"
pub const LOOM_SCHEMA: &str = "CREATE TABLE arrivals(value INTEGER)";
pub fn handle(_message: Vec<u8>) {
    let args: loom::serde_json::Value = loom::serde_json::from_str("{\"sql\":\"INSERT INTO arrivals VALUES (42)\",\"params\":[]}").unwrap();
    let _: loom::serde_json::Value = loom::perform("sql", args).unwrap();
}
"#;
    let added = service
        .command(loom_proto::CommandRequest {
            session: None,
            command: "add".into(),
            args: json!({"name":"new-actor","source":source}),
        })
        .await;
    assert!(added.ok, "{added:?}");
    let spawned = service
        .actor_command(
            "actor_spawn",
            json!({"behavior_hash":"new-actor","init":{}}),
        )
        .await?;
    let id = spawned["id"].as_str().unwrap();
    let node = &service.actors.as_ref().unwrap().node;
    node.run_until_idle().await?;
    assert_eq!(
        node.info(id).await?.behavior_hash,
        added.result["hash"].as_str().unwrap()
    );
    let actor = node.open(id).await?;
    let rows = actor
        .inspect_sql("SELECT value FROM arrivals", Vec::new())
        .await?;
    assert_eq!(rows.rows.len(), 1);
    assert_eq!(rows.rows[0].get::<i64>(0)?, 42);
    Ok(())
}
