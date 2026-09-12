use super::{
    message,
    support::{fixtures, integer},
};
use async_trait::async_trait;
use loom_actor::{EffectError, EffectHandler, EffectKey, Registry};
use serde_json::json;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct CountCalls {
    calls: Mutex<Vec<EffectKey>>,
}

#[async_trait]
impl EffectHandler for CountCalls {
    async fn call(
        &self,
        key: &EffectKey,
        kind: &str,
        request: &[u8],
    ) -> Result<Vec<u8>, EffectError> {
        assert_eq!(kind, "test.effect");
        self.calls.lock().unwrap().push(key.clone());
        Ok(request.to_vec())
    }
}

#[tokio::test]
async fn scoped_and_handler_guest_traps_are_not_retried() {
    let fixtures = fixtures().await;
    for action in ["scoped_trap", "handler_trap"] {
        let directory = tempfile::tempdir().unwrap();
        let effects = Arc::new(CountCalls::default());
        let node = fixtures
            .node_with_effects(directory.path(), effects.clone())
            .await;
        let id = node
            .spawn_root(&fixtures.handler, &message(json!({"action":action})))
            .await
            .unwrap();
        node.run_until_idle().await.unwrap();
        let actor = node.open(&id).await.unwrap();
        assert_eq!(
            effects.calls.lock().unwrap().len(),
            1,
            "{action} was retried"
        );
        assert_eq!(actor.cursor().await.unwrap(), 0);
        assert_eq!(
            integer(&actor, "SELECT count(*) FROM dead_letters").await,
            1
        );
        assert_eq!(integer(&actor, "SELECT count(*) FROM effects").await, 0);
    }
    fixtures.assert_no_legacy_execution();
}

#[derive(Default)]
struct FailOnce {
    calls: Mutex<Vec<EffectKey>>,
}

#[async_trait]
impl EffectHandler for FailOnce {
    async fn call(
        &self,
        key: &EffectKey,
        kind: &str,
        request: &[u8],
    ) -> Result<Vec<u8>, EffectError> {
        assert_eq!(kind, "test.effect");
        assert_eq!(request, &[1, 2, 3]);
        let mut calls = self.calls.lock().unwrap();
        calls.push(key.clone());
        if calls.len() == 1 {
            Err(EffectError::Environmental(anyhow::anyhow!(
                "retry this fixture effect"
            )))
        } else {
            Ok(request.to_vec())
        }
    }
}

#[tokio::test]
async fn environmental_effect_error_retries_even_when_guest_ignores_it() {
    let fixtures = fixtures().await;
    let directory = tempfile::tempdir().unwrap();
    let effects = Arc::new(FailOnce::default());
    let node = fixtures
        .node_with_effects(directory.path(), effects.clone())
        .await;
    let body = message(json!({"action":"effect"}));
    let id = node.spawn_root(&fixtures.handler, &body).await.unwrap();
    node.run_until_idle().await.unwrap();
    let actor = node.open(&id).await.unwrap();

    assert_eq!(actor.cursor().await.unwrap(), 1);
    assert_eq!(integer(&actor, "SELECT count(*) FROM entries").await, 1);
    assert_eq!(
        integer(&actor, "SELECT count(*) FROM dead_letters").await,
        0
    );
    let recorded = actor
        .sql("SELECT kind,request,result FROM effects", ())
        .await
        .unwrap();
    assert_eq!(recorded.rows.len(), 1);
    assert_eq!(recorded.rows[0].get::<String>(0).unwrap(), "test.effect");
    assert_eq!(recorded.rows[0].get::<Vec<u8>>(1).unwrap(), vec![1, 2, 3]);
    assert_eq!(recorded.rows[0].get::<Vec<u8>>(2).unwrap(), vec![1, 2, 3]);
    let calls = effects.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[0], calls[1],
        "retry must preserve its idempotency key"
    );
    drop(calls);
    fixtures.assert_no_legacy_execution();
}

#[tokio::test]
async fn resolution_reads_root_schema_constant() {
    let fixtures = fixtures().await;
    let registry = loom_behavior::StoreRegistry::new(fixtures.store.clone());
    let behavior = registry.resolve(&fixtures.handler).await.unwrap();
    assert_eq!(
        behavior.schema(),
        "CREATE TABLE entries(body BLOB NOT NULL)"
    );
    let promoted = registry.resolve(&fixtures.promoted).await.unwrap();
    assert_eq!(
        promoted.schema(),
        "ALTER TABLE entries ADD COLUMN revision TEXT"
    );
    fixtures.assert_no_legacy_execution();
}

#[tokio::test]
async fn definition_without_schema_exports_empty_schema() {
    let fixtures = fixtures().await;
    let registry = loom_behavior::StoreRegistry::new(fixtures.store.clone());
    let behavior = registry.resolve(&fixtures.no_schema).await.unwrap();
    assert_eq!(behavior.hash(), fixtures.no_schema);
    assert_eq!(behavior.schema(), "");
    fixtures.assert_no_legacy_execution();
}

#[tokio::test]
async fn missing_stored_definition_error_names_the_hash() {
    let directory = tempfile::tempdir().unwrap();
    let store = loom_store::Store::open(directory.path().join("empty.sqlite")).unwrap();
    let registry = loom_behavior::StoreRegistry::new(store);
    let missing = "0".repeat(64);
    let error = match registry.resolve(&missing).await {
        Ok(_) => panic!("resolved a missing definition"),
        Err(error) => error,
    };
    assert!(
        format!("{error:#}").contains(&missing),
        "missing definition identity: {error:#}"
    );
    assert!(registry.behaviors().await.unwrap().is_empty());
}
