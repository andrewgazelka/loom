use crate::registry::Registry;
mod registry;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use loom_actor::{
    Actor, Behavior, ChildSpec, ChildType, Clock, Config, Ctx, DefaultEffects, Durability, EffectError, EffectHandler, EffectKey, Node,
    ClusterConfig, Status, StoreConfig, Trap,
};
use serde_json::Value;

#[derive(Debug, Default)]
struct ManualClock {
    millis: AtomicU64,
}

impl Clock for ManualClock {
    fn now_ms(&self) -> anyhow::Result<u64> {
        Ok(self.millis.load(Ordering::SeqCst))
    }
}

impl ManualClock {
    fn expire(&self) {
        self.millis.fetch_add(3_600_001, Ordering::SeqCst);
    }
}

async fn node(dir: &Path, store: Option<&Path>, clock: Arc<ManualClock>) -> Node {
    node_with_effects(dir, store, clock, Arc::new(DefaultEffects)).await
}

async fn node_with_effects(dir: &Path, store: Option<&Path>, clock: Arc<ManualClock>, effects: Arc<dyn EffectHandler>) -> Node {
    let mut registry = Registry::new();
    registry.insert("terminating-counter".into(), Arc::new(TerminatingCounter));
    Node::new(
        dir,
        Arc::new(registry),
        effects,
        Config {
            store: store.map(|path| StoreConfig::Local { path: path.to_owned() }),
            cluster: store.map(|_| ClusterConfig {
                node_id: dir.file_name().unwrap().to_str().unwrap().to_owned(),
                addr: "127.0.0.1:1".into(),
                key: [0x43; 32],
            }),
            ship_interval: Duration::from_secs(3600),
            lease_ttl: Duration::from_secs(3600),
            lease_clock: clock,
            ..Config::default()
        },
    )
    .await
    .unwrap()
}

struct TerminatingCounter;

#[async_trait]
impl Behavior for TerminatingCounter {
    fn hash(&self) -> &str {
        "terminating-counter"
    }
    fn schema(&self) -> &str {
        "CREATE TABLE events(kind TEXT, body BLOB)"
    }
    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        cx.sql("INSERT INTO events VALUES ('message', ?)", [msg]).await?;
        cx.effect("echo", msg).await?;
        Ok(())
    }
    async fn terminate(&self, cx: &mut Ctx<'_>, reason: &str) -> Result<(), Trap> {
        let response = cx.effect("echo", reason.as_bytes()).await?;
        cx.sql("INSERT INTO events VALUES ('terminate', ?)", [response]).await?;
        Ok(())
    }
}

#[derive(Default)]
struct GatedEffects {
    armed: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait]
impl EffectHandler for GatedEffects {
    async fn call(&self, key: &EffectKey, kind: &str, request: &[u8]) -> Result<Vec<u8>, EffectError> {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        DefaultEffects.call(key, kind, request).await
    }
}

async fn spawn(node: &Node, durability: Durability) -> String {
    let mut spec = ChildSpec::new("counter-v1", b"effect", ChildType::Worker);
    spec.durability = durability;
    spec.link = false;
    node.spawn(&node.root(), &spec).await.unwrap()
}

async fn integer(actor: &Actor, sql: &str) -> i64 {
    let result = actor.sql(sql, ()).await.unwrap();
    assert_eq!(result.rows.len(), 1);
    result.rows[0].get(0).unwrap()
}

async fn process(node: &Node, id: &str, total: u64) {
    for seq in 2..=total {
        node.send(id, &format!("message-{seq}"), b"effect").await.unwrap();
    }
    node.run_until_idle().await.unwrap();
    assert_eq!(node.open(id).await.unwrap().cursor().await.unwrap(), i64::try_from(total).unwrap());
}

fn head(store: &Path, id: &str) -> Value {
    serde_json::from_slice(&std::fs::read(store.join(format!("actors/{id}/head"))).unwrap()).unwrap()
}

#[tokio::test]
async fn ship_and_restore_on_fresh_node() {
    let workspace = tempfile::tempdir().unwrap();
    let store = workspace.path().join("store");
    let clock = Arc::new(ManualClock::default());
    let a = node(&workspace.path().join("a"), Some(&store), clock.clone()).await;
    let id = spawn(&a, Durability::Local).await;
    process(&a, &id, 5).await;
    a.ship(&id).await.unwrap();
    assert_eq!(head(&store, &id)["seq"], 5);
    assert_eq!(head(&store, &id)["snapshot"], format!("actors/{id}/snapshots/1-0.db"));
    assert!(!head(&store, &id)["segments"].as_array().unwrap().is_empty());
    let mut spec = ChildSpec::new("terminating-counter", b"before-stop", ChildType::Worker);
    spec.link = false;
    let terminated_id = a.spawn(&a.root(), &spec).await.unwrap();
    a.run_until_idle().await.unwrap();
    a.ship(&terminated_id).await.unwrap();
    a.stop(&terminated_id, "shutdown").await.unwrap();
    a.ship(&terminated_id).await.unwrap();
    a.close().await.unwrap();

    let b_dir = workspace.path().join("b");
    let b = node(&b_dir, Some(&store), clock).await;
    assert!(!b_dir.join(format!("{id}.db")).exists());
    let restored = b.open(&id).await.unwrap();
    assert_eq!(restored.cursor().await.unwrap(), 5);
    assert_eq!(integer(&restored, "SELECT COUNT(*) FROM entries").await, 5);
    assert_eq!(integer(&restored, "SELECT COUNT(*) FROM inbox WHERE state='done'").await, 5);
    assert_eq!(integer(&restored, "SELECT COUNT(*) FROM effects WHERE kind='echo'").await, 5);
    assert_eq!(integer(&restored, "SELECT COUNT(*) FROM code_changes").await, 1);
    assert_eq!(restored.status().await.unwrap(), Status::Running);
    let terminated = b.open(&terminated_id).await.unwrap();
    assert_eq!(terminated.status().await.unwrap(), Status::Stopped);
    assert_eq!(integer(&terminated, "SELECT COUNT(*) FROM events WHERE kind='message'").await, 1);
    assert_eq!(integer(&terminated, "SELECT COUNT(*) FROM events WHERE kind='terminate'").await, 1);
    let reason = terminated.sql("SELECT body FROM events WHERE kind='terminate'", ()).await.unwrap();
    assert_eq!(reason.rows[0].get::<Vec<u8>>(0).unwrap(), b"shutdown");
    b.close().await.unwrap();
}

#[tokio::test]
async fn remote_durability_waits_for_store() {
    for durability in [Durability::Remote, Durability::Local] {
        let workspace = tempfile::tempdir().unwrap();
        let store = workspace.path().join("store");
        let saved_store = workspace.path().join("saved-store");
        let node = node(&workspace.path().join("node"), Some(&store), Arc::new(ManualClock::default())).await;
        let id = spawn(&node, durability).await;
        process(&node, &id, 1).await;
        node.ship(&id).await.unwrap();
        node.send(&id, "refused", b"effect").await.unwrap();
        std::fs::rename(&store, &saved_store).unwrap();
        std::fs::write(&store, b"puts cannot create children beneath a regular file").unwrap();

        let result = node.run_until_idle().await;
        let actor = node.open(&id).await.unwrap();
        if durability == Durability::Remote {
            assert!(result.is_err(), "remote commit must await the failed object-store put");
            assert_eq!(actor.cursor().await.unwrap(), 1);
            assert_eq!(integer(&actor, "SELECT COUNT(*) FROM inbox WHERE key='refused' AND state='done'").await, 0);
            assert_eq!(integer(&actor, "SELECT COUNT(*) FROM entries").await, 1);
            assert_eq!(integer(&actor, "SELECT COUNT(*) FROM dead_letters").await, 0);
        } else {
            result.unwrap();
            assert_eq!(actor.cursor().await.unwrap(), 2);
            assert!(node.ship(&id).await.is_err());
            assert!(node.shipping_failures().iter().any(|failure| failure.actor_id == id && !failure.error.is_empty()));
        }
        assert!(node.close().await.is_err(), "failed flush must leave the node available for retry");
        std::fs::remove_file(&store).unwrap();
        std::fs::rename(&saved_store, &store).unwrap();
        node.run_until_idle().await.unwrap();
        assert_eq!(actor.cursor().await.unwrap(), 2);
        assert_eq!(integer(&actor, "SELECT COUNT(*) FROM entries").await, 2);
        node.close().await.unwrap();
        let recovered = self::node(&workspace.path().join("recovered"), Some(&store), Arc::new(ManualClock::default())).await;
        let restored = recovered.open(&id).await.unwrap();
        assert_eq!(restored.cursor().await.unwrap(), 2);
        assert_eq!(integer(&restored, "SELECT COUNT(*) FROM entries").await, 2);
        recovered.close().await.unwrap();
    }
}

#[tokio::test]
async fn lease_fences_stale_owner() {
    let workspace = tempfile::tempdir().unwrap();
    let store = workspace.path().join("store");
    let a_dir = workspace.path().join("a");
    let clock = Arc::new(ManualClock::default());
    let a = node(&a_dir, Some(&store), clock.clone()).await;
    let id = spawn(&a, Durability::Local).await;
    process(&a, &id, 3).await;
    a.ship(&id).await.unwrap();
    let stale = a.open(&id).await.unwrap();
    let old_epoch = head(&store, &id)["epoch"].as_u64().unwrap();
    assert_eq!(old_epoch, 1);
    a.send(&id, "unshipped", b"unshipped").await.unwrap();
    a.run_until_idle().await.unwrap();
    assert_eq!(stale.cursor().await.unwrap(), 4);
    clock.expire();

    let b = node(&workspace.path().join("b"), Some(&store), clock.clone()).await;
    let current = b.open(&id).await.unwrap();
    assert_eq!(current.cursor().await.unwrap(), 3);
    b.send(&id, "new-owner", b"new-owner").await.unwrap();
    b.run_until_idle().await.unwrap();
    b.ship(&id).await.unwrap();
    let winner = head(&store, &id);
    assert!(winner["epoch"].as_u64().unwrap() > old_epoch);
    let error = a.ship(&id).await.unwrap_err();
    assert!(!error.to_string().is_empty());
    assert_eq!(stale.status().await.unwrap(), Status::Stopped);
    let reason = stale.sql("SELECT value FROM meta WHERE key='reason'", ()).await.unwrap();
    assert_eq!(reason.rows[0].get::<String>(0).unwrap(), "lease_lost");
    assert!(a_dir.join(format!("{id}.stale.{old_epoch}.db")).is_file());
    assert!(!a_dir.join(format!("{id}.db")).exists());
    assert_eq!(integer(&stale, "SELECT COUNT(*) FROM entries WHERE body=X'756e73686970706564'").await, 1);
    assert_eq!(integer(&current, "SELECT COUNT(*) FROM inbox WHERE key='unshipped'").await, 0);
    assert_eq!(head(&store, &id), winner, "stale publication must not change the winning head");
    b.close().await.unwrap();

    let c = node(&workspace.path().join("c"), Some(&store), clock.clone()).await;
    let d = node(&workspace.path().join("d"), Some(&store), clock.clone()).await;
    struct Claim {
        node: Node,
        acquired: bool,
    }
    let mut claims = tokio::task::JoinSet::new();
    for contender in [c, d] {
        let id = id.clone();
        claims.spawn(async move {
            let acquired = contender.open(&id).await.is_ok();
            Claim { node: contender, acquired }
        });
    }
    let mut results = Vec::new();
    while let Some(result) = claims.join_next().await {
        results.push(result.unwrap());
    }
    assert_eq!(results.iter().filter(|claim| claim.acquired).count(), 1, "conditional lease claim admits one concurrent owner");
    for claim in results {
        claim.node.close().await.unwrap();
    }

    let effects = Arc::new(GatedEffects::default());
    let slow = node_with_effects(&workspace.path().join("slow"), Some(&store), clock.clone(), effects.clone()).await;
    let slow_id = spawn(&slow, Durability::Local).await;
    process(&slow, &slow_id, 1).await;
    let slow_actor = slow.open(&slow_id).await.unwrap();
    slow.send(&slow_id, "expires-in-handler", b"effect").await.unwrap();
    effects.armed.store(true, Ordering::SeqCst);
    let runner = slow.clone();
    let attempt = tokio::spawn(async move { runner.run_until_idle().await });
    tokio::time::timeout(Duration::from_secs(5), effects.entered.notified()).await.unwrap();
    clock.expire();
    effects.release.notify_one();
    assert!(attempt.await.unwrap().is_err(), "lease expiry during the handler must prevent COMMIT");
    assert_eq!(slow_actor.cursor().await.unwrap(), 1);
    assert_eq!(integer(&slow_actor, "SELECT COUNT(*) FROM entries").await, 1);
    assert_eq!(integer(&slow_actor, "SELECT COUNT(*) FROM inbox WHERE key='expires-in-handler' AND state='done'").await, 0);
    renewal_survives_blocked_handler(workspace.path()).await;
}

#[tokio::test]
async fn takeover_resumes_at_cursor() {
    let workspace = tempfile::tempdir().unwrap();
    let store = workspace.path().join("store");
    let clock = Arc::new(ManualClock::default());
    let a = node(&workspace.path().join("a"), Some(&store), clock.clone()).await;
    let id = spawn(&a, Durability::Local).await;
    process(&a, &id, 3).await;
    a.ship(&id).await.unwrap();
    clock.expire();
    let b = node(&workspace.path().join("b"), Some(&store), clock).await;
    let restored = b.open(&id).await.unwrap();
    assert_eq!(restored.cursor().await.unwrap(), 3);
    assert!(a.ship(&id).await.is_err());
    b.send(&id, "message-2", b"effect").await.unwrap();
    b.send(&id, "message-3", b"effect").await.unwrap();
    b.run_until_idle().await.unwrap();
    assert_eq!(restored.cursor().await.unwrap(), 3);
    b.send(&id, "message-4", b"effect").await.unwrap();
    b.run_until_idle().await.unwrap();
    assert_eq!(restored.cursor().await.unwrap(), 4);
    assert_eq!(integer(&restored, "SELECT COUNT(*) FROM entries").await, 4);
    assert_eq!(integer(&restored, "SELECT COUNT(*) FROM effects").await, 4);
    assert_eq!(integer(&restored, "SELECT COUNT(*) FROM inbox WHERE key='message-4' AND seq=4 AND state='done'").await, 1);
    b.ship(&id).await.unwrap();
    assert_eq!(head(&store, &id)["seq"], 4);
    b.close().await.unwrap();
}

#[tokio::test]
async fn no_store_configured_changes_nothing() {
    let workspace = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock::default());
    let a = node(workspace.path(), None, clock.clone()).await;
    let id = spawn(&a, Durability::Local).await;
    process(&a, &id, 3).await;
    clock.expire();
    a.renew_leases().await.unwrap();
    a.send(&id, "message-4", b"effect").await.unwrap();
    a.run_until_idle().await.unwrap();
    a.ship(&id).await.unwrap();
    assert!(a.shipping_failures().is_empty());
    a.close().await.unwrap();
    drop(a);
    let reopened = node(workspace.path(), None, clock).await;
    let actor = reopened.open(&id).await.unwrap();
    assert_eq!(actor.cursor().await.unwrap(), 4);
    assert_eq!(actor.status().await.unwrap(), Status::Running);
    assert_eq!(integer(&actor, "SELECT COUNT(*) FROM entries").await, 4);
    reopened.send(&id, "message-4", b"effect").await.unwrap();
    reopened.run_until_idle().await.unwrap();
    assert_eq!(actor.cursor().await.unwrap(), 4);
    reopened.close().await.unwrap();
}

async fn renewal_survives_blocked_handler(workspace: &Path) {
    let effects = Arc::new(GatedEffects::default());
    let clock = Arc::new(ManualClock::default());
    let node = Node::new(
        workspace.join("renewal-node"),
        Arc::new(Registry::new()),
        effects.clone(),
        Config {
            store: Some(StoreConfig::Local { path: workspace.join("renewal-store") }),
            cluster: Some(ClusterConfig { node_id: "renewal-node".into(), addr: "127.0.0.1:1".into(), key: [0x43; 32] }),
            ship_interval: Duration::from_millis(10),
            lease_ttl: Duration::from_millis(60),
            lease_clock: clock.clone(),
            ..Config::default()
        },
    )
    .await
    .unwrap();
    let blocked = spawn(&node, Durability::Local).await;
    let independent = spawn(&node, Durability::Local).await;
    node.run_until_idle().await.unwrap();
    node.send(&blocked, "blocked", b"effect").await.unwrap();
    effects.armed.store(true, Ordering::SeqCst);
    let runner = node.clone();
    let attempt = tokio::spawn(async move { runner.run_until_idle().await });
    tokio::time::timeout(Duration::from_secs(5), effects.entered.notified()).await.unwrap();
    for _ in 0..4 {
        clock.millis.fetch_add(20, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let renewed = [&blocked, &independent, &node.root()].iter().all(|id| {
                    let lease: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(workspace.join("renewal-store").join(format!("actors/{id}/lease"))).unwrap())
                            .unwrap();
                    lease["expires_at"].as_u64().unwrap() >= clock.millis.load(Ordering::SeqCst) + 60
                });
                if renewed {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    node.send(&independent, "independent", b"effect").await.unwrap();
    effects.release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), attempt).await.unwrap().unwrap().unwrap();
    node.run_until_idle().await.unwrap();
    for id in [&blocked, &independent] {
        let actor = node.open(id).await.unwrap();
        assert_eq!(actor.cursor().await.unwrap(), 2);
        assert_eq!(actor.status().await.unwrap(), Status::Running);
        assert_eq!(integer(&actor, "SELECT COUNT(*) FROM entries").await, 2);
    }
    assert!(node.shipping_failures().is_empty(), "a handler holding its connection must not starve renewal");
    node.close().await.unwrap();
}
