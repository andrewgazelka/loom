mod admission;
mod errors;
mod support;

use loom_actor::{
    Behavior, Config, Ctx, EffectError, EffectHandler, EffectKey, Node, Registry, Rights, Trap,
};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use support::{fixtures, integer};

struct ActorOpProbe {
    definition: Arc<loom_behavior::LoomBehavior>,
    attempts: AtomicUsize,
    effect_calls: AtomicUsize,
}

#[async_trait::async_trait]
impl Behavior for ActorOpProbe {
    fn hash(&self) -> &str {
        self.definition.hash()
    }
    fn schema(&self) -> &str {
        self.definition.schema()
    }
    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        self.definition.handle(cx, msg).await
    }
}

#[async_trait::async_trait]
impl EffectHandler for ActorOpProbe {
    async fn call(
        &self,
        _key: &EffectKey,
        _kind: &str,
        request: &[u8],
    ) -> Result<Vec<u8>, EffectError> {
        self.effect_calls.fetch_add(1, Ordering::SeqCst);
        Ok(request.to_vec())
    }
}

struct ProbeRegistry {
    probe: Arc<ActorOpProbe>,
}

#[async_trait::async_trait]
impl Registry for ProbeRegistry {
    async fn resolve(&self, reference: &str) -> anyhow::Result<Arc<dyn Behavior>> {
        anyhow::ensure!(
            reference == self.probe.hash(),
            "unknown behavior {reference}"
        );
        Ok(self.probe.clone())
    }

    async fn behaviors(&self) -> anyhow::Result<Vec<loom_actor::builtin::BehaviorInfo>> {
        Ok(vec![loom_actor::builtin::BehaviorInfo {
            hash: self.probe.hash().to_owned(),
            description: "actor operation probe".to_owned(),
        }])
    }
}

#[tokio::test]
async fn unknown_actor_op_is_a_trap() {
    let fixtures = fixtures().await;
    let directory = tempfile::tempdir().unwrap();
    let definition = Arc::new(
        loom_behavior::LoomBehavior::new(fixtures.store.clone(), &fixtures.handler)
            .await
            .unwrap(),
    );
    let probe = Arc::new(ActorOpProbe {
        definition,
        attempts: AtomicUsize::new(0),
        effect_calls: AtomicUsize::new(0),
    });
    let registry: Arc<dyn Registry> = Arc::new(ProbeRegistry {
        probe: probe.clone(),
    });
    let node = Node::new(directory.path(), registry, probe.clone(), Config::default())
        .await
        .unwrap();
    let id = node
        .spawn_root(
            &fixtures.handler,
            &message(json!({"action":"unknown_actor"})),
        )
        .await
        .unwrap();
    node.run_until_idle().await.unwrap();
    let actor = node.open(&id).await.unwrap();
    let letters = actor
        .sql("SELECT error FROM dead_letters", ())
        .await
        .unwrap();
    assert_eq!(letters.rows.len(), 1);
    let error: String = letters.rows[0].get(0).unwrap();
    assert!(error.contains("actor.nope"), "missing effect name: {error}");
    assert_eq!(actor.cursor().await.unwrap(), 0);
    assert_eq!(
        probe.attempts.load(Ordering::SeqCst),
        1,
        "guest was retried"
    );
    assert_eq!(probe.effect_calls.load(Ordering::SeqCst), 0);
    assert_eq!(integer(&actor, "SELECT count(*) FROM effects").await, 0);
    fixtures.assert_no_legacy_execution();
}

fn message(value: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&value).unwrap()
}

#[tokio::test]
async fn definition_handles_message_and_writes_sql() {
    let fixtures = fixtures().await;
    let directory = tempfile::tempdir().unwrap();
    let node = fixtures.node(directory.path()).await;
    let first = message(json!({"action":"row","number":1}));
    let second = message(json!({"action":"row","number":2,"scoped":true}));
    let third = message(json!({"action":"row","number":3}));
    let id = node.spawn_root(&fixtures.handler, &first).await.unwrap();
    node.send(&id, "second", &second).await.unwrap();
    node.send(&id, "third", &third).await.unwrap();
    node.run_until_idle().await.unwrap();

    let actor = node.open(&id).await.unwrap();
    assert_eq!(integer(&actor, "SELECT count(*) FROM entries").await, 3);
    assert_eq!(actor.cursor().await.unwrap(), 3);
    let rows = actor
        .sql("SELECT body FROM entries ORDER BY rowid", ())
        .await
        .unwrap();
    let bodies: Vec<Vec<u8>> = rows.rows.iter().map(|row| row.get(0).unwrap()).collect();
    assert_eq!(bodies, vec![first, second, third]);
    assert_eq!(
        integer(&actor, "SELECT count(*) FROM dead_letters").await,
        0
    );
    fixtures.assert_no_legacy_execution();
}

#[tokio::test]
async fn definition_send_goes_through_outbox() {
    let fixtures = fixtures().await;
    let directory = tempfile::tempdir().unwrap();
    let node = fixtures.node(directory.path()).await;
    let receiver = node
        .spawn_root(&fixtures.handler, &message(json!({"action":"init"})))
        .await
        .unwrap();
    node.run_until_idle().await.unwrap();
    let cap = node.cap_for(&receiver, Rights::SEND).await.unwrap();
    let token = serde_json::to_vec(&cap).unwrap();
    let receiving = node.open(&receiver).await.unwrap();
    let baseline = integer(&receiving, "SELECT count(*) FROM inbox").await;
    let failed = node
        .spawn_root(
            &fixtures.handler,
            &message(json!({"action":"send","cap":token,"trap":true})),
        )
        .await
        .unwrap();
    node.run_until_idle().await.unwrap();
    let failed_actor = node.open(&failed).await.unwrap();
    assert_eq!(
        integer(&receiving, "SELECT count(*) FROM inbox").await,
        baseline
    );
    assert_eq!(integer(&receiving, "SELECT count(*) FROM entries").await, 0);
    assert!(
        failed_actor
            .sql("SELECT seq FROM outbox WHERE target=?", [receiver.as_str()])
            .await
            .unwrap()
            .rows
            .is_empty()
    );
    assert_eq!(
        integer(&failed_actor, "SELECT count(*) FROM dead_letters").await,
        1
    );
    assert_eq!(failed_actor.cursor().await.unwrap(), 0);

    let sender = node
        .spawn_root(
            &fixtures.handler,
            &message(json!({"action":"send","cap":token,"trap":false})),
        )
        .await
        .unwrap();
    node.run_until_idle().await.unwrap();
    let sending = node.open(&sender).await.unwrap();
    assert_eq!(
        integer(&receiving, "SELECT count(*) FROM inbox").await,
        baseline + 1
    );
    assert_eq!(integer(&receiving, "SELECT count(*) FROM entries").await, 1);
    let delivered = sending
        .sql(
            "SELECT msg,delivered FROM outbox WHERE target=?",
            [receiver.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(delivered.rows.len(), 1);
    assert_eq!(delivered.rows[0].get::<i64>(1).unwrap(), 1);
    sending
        .sql(
            "UPDATE outbox SET delivered=0 WHERE target=?",
            [receiver.as_str()],
        )
        .await
        .unwrap();
    assert!(node.pump(&sender).await.unwrap());
    node.run_until_idle().await.unwrap();
    assert_eq!(
        integer(&receiving, "SELECT count(*) FROM inbox").await,
        baseline + 1
    );
    assert_eq!(integer(&receiving, "SELECT count(*) FROM entries").await, 1);
    assert_eq!(integer(&sending, "SELECT count(*) FROM caps").await, 1);

    let spawning = node
        .spawn_root(
            &fixtures.handler,
            &message(json!({"action":"spawn_send","behavior_hash":fixtures.handler})),
        )
        .await
        .unwrap();
    node.run_until_idle().await.unwrap();
    let parent = node.open(&spawning).await.unwrap();
    assert_eq!(
        integer(&parent, "SELECT count(*) FROM dead_letters").await,
        0
    );
    let children = parent
        .sql(
            "SELECT target FROM caps WHERE target != ?",
            [spawning.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(children.rows.len(), 1);
    let child_id: String = children.rows[0].get(0).unwrap();
    let child = node.open(&child_id).await.unwrap();
    assert_eq!(integer(&child, "SELECT count(*) FROM entries").await, 1);
    assert_eq!(integer(&child, "SELECT count(*) FROM revoked").await, 1);
    fixtures.assert_no_legacy_execution();
}

#[tokio::test]
async fn guest_without_cap_cannot_send() {
    let fixtures = fixtures().await;
    let directory = tempfile::tempdir().unwrap();
    let node = fixtures.node(directory.path()).await;
    let receiver = node
        .spawn_root(&fixtures.handler, &message(json!({"action":"init"})))
        .await
        .unwrap();
    node.run_until_idle().await.unwrap();
    let mut forged = node.cap_for(&receiver, Rights::SEND).await.unwrap();
    forged.mac[0] ^= 1;
    let sender = node
        .spawn_root(
            &fixtures.handler,
            &message(json!({"action":"forged_send","cap":serde_json::to_vec(&forged).unwrap()})),
        )
        .await
        .unwrap();
    node.run_until_idle().await.unwrap();
    let sending = node.open(&sender).await.unwrap();
    let letters = sending
        .sql("SELECT error FROM dead_letters", ())
        .await
        .unwrap();
    assert_eq!(letters.rows.len(), 1);
    let error: String = letters.rows[0].get(0).unwrap();
    assert!(error.contains("send"), "missing operation: {error}");
    assert!(
        error.contains(&forged.cap_id.to_string()),
        "missing cap_id: {error}"
    );
    assert_eq!(sending.cursor().await.unwrap(), 0);
    let outbound = sending
        .sql(
            "SELECT count(*) FROM outbox WHERE target=?",
            [receiver.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(outbound.rows[0].get::<i64>(0).unwrap(), 0);
    let receiving = node.open(&receiver).await.unwrap();
    assert_eq!(integer(&receiving, "SELECT count(*) FROM inbox").await, 1);
    assert_eq!(integer(&receiving, "SELECT count(*) FROM entries").await, 0);
    fixtures.assert_no_legacy_execution();
}

#[tokio::test]
async fn unknown_effect_is_a_trap() {
    let fixtures = fixtures().await;
    let directory = tempfile::tempdir().unwrap();
    let node = fixtures.node(directory.path()).await;
    let id = node
        .spawn_root(&fixtures.handler, &message(json!({"action":"unknown"})))
        .await
        .unwrap();
    node.run_until_idle().await.unwrap();
    let actor = node.open(&id).await.unwrap();
    let letters = actor
        .sql("SELECT error FROM dead_letters", ())
        .await
        .unwrap();
    assert_eq!(letters.rows.len(), 1);
    let error: String = letters.rows[0].get(0).unwrap();
    assert!(error.contains("nope"), "missing effect name: {error}");
    assert_eq!(actor.cursor().await.unwrap(), 0);
    assert_eq!(integer(&actor, "SELECT count(*) FROM effects").await, 0);
    fixtures.assert_no_legacy_execution();
}

#[tokio::test]
async fn promote_new_definition_changes_behavior() {
    let fixtures = fixtures().await;
    let directory = tempfile::tempdir().unwrap();
    let node = fixtures.node(directory.path()).await;
    let before = message(json!({"action":"row","version":1}));
    let after = message(json!({"action":"row","version":2}));
    let id = node.spawn_root(&fixtures.handler, &before).await.unwrap();
    node.run_until_idle().await.unwrap();
    node.promote(&id, &fixtures.promoted, "test", "new compiled definition")
        .await
        .unwrap();
    node.send(&id, "promoted-message", &after).await.unwrap();
    node.run_until_idle().await.unwrap();

    let actor = node.open(&id).await.unwrap();
    assert_eq!(actor.cursor().await.unwrap(), 2);
    assert_eq!(
        integer(
            &actor,
            "SELECT count(*) FROM entries WHERE revision IS NULL"
        )
        .await,
        1
    );
    let changed = actor
        .sql(
            "SELECT body,revision FROM entries WHERE revision IS NOT NULL",
            (),
        )
        .await
        .unwrap();
    assert_eq!(changed.rows.len(), 1);
    assert_eq!(changed.rows[0].get::<Vec<u8>>(0).unwrap(), after);
    assert_eq!(changed.rows[0].get::<String>(1).unwrap(), "v2");
    let lineage = actor
        .sql("SELECT behavior_hash FROM code_changes ORDER BY seq", ())
        .await
        .unwrap();
    let hashes: Vec<String> = lineage.rows.iter().map(|row| row.get(0).unwrap()).collect();
    assert_eq!(
        hashes,
        vec![fixtures.handler.clone(), fixtures.promoted.clone()]
    );
    fixtures.assert_no_legacy_execution();
}
