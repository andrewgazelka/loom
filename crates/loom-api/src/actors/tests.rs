use super::*;

#[tokio::test]
async fn actor_references_resolve_store_names_and_preserve_native_behaviors() -> anyhow::Result<()>
{
    let directory = tempfile::tempdir()?;
    let store = loom_store::Store::memory()?;
    let deps = std::collections::BTreeMap::new();
    let source = "pub fn main() -> i32 { 1 }";
    let identity = loom_proto::definition_identity(loom_proto::Lang::Rust, source, &deps, None)?;
    let hash = store.put("def", &identity)?;
    store.define(
        &loom_proto::Def {
            hash: hash.clone(),
            lang: loom_proto::Lang::Rust,
            component_hash: None,
            sig: Default::default(),
            allowed_effects: None,
            observed_effects: Vec::new(),
        },
        Some("candidate"),
        source,
        &deps,
    )?;
    let node = Node::new(
        directory.path().join("actors"),
        loom_actor::Registry::new(),
        std::sync::Arc::new(loom_actor::DefaultEffects),
        loom_actor::Config::default(),
    )
    .await?;
    let service = crate::Service::new(
        store,
        directory.path().to_owned(),
        vec![loom_proto::Lang::Rust],
    )?
    .with_actors(node);
    let spawned = service
        .actor_command(
            "actor_spawn",
            json!({"behavior_hash":"counter-v1","init":null}),
        )
        .await?;
    let id = spawned["id"].as_str().unwrap();
    let spawn_error = service
        .actor_command(
            "actor_spawn",
            json!({"behavior_hash":"candidate","init":null}),
        )
        .await
        .unwrap_err();
    assert!(spawn_error.to_string().contains(&hash), "{spawn_error:#}");
    let validate_error = service
        .actor_command(
            "actor_validate",
            json!({"id":id,"candidate_hash":"candidate","k":0}),
        )
        .await
        .unwrap_err();
    assert!(
        validate_error.to_string().contains(&hash),
        "{validate_error:#}"
    );
    let validation = service
        .actor_command(
            "actor_validate",
            json!({"id":id,"candidate_hash":"counter-v1","k":0}),
        )
        .await?;
    assert!(validation.is_object(), "{validation}");
    Ok(())
}
