//! These tests execute real V8 guests through the durable actor transaction.
use loom_actor::{Actor, Config, DefaultEffects, Node, Rights};
use loom_behavior::StoreRegistry;
use loom_proto::{Def, Lang};
use loom_store::Store;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

const SOURCE: &str = r#"
const LOOM_SCHEMA = "CREATE TABLE entries(body BLOB NOT NULL);";
async function main(bytes) {
  const msg = JSON.parse(String.fromCharCode(...bytes));
  if (msg.action === 'init') return null;
  await loom.perform('sql', {sql:'INSERT INTO entries(body) VALUES (?)', params:[{type:'blob',value:bytes}]});
  if (msg.cap) {
    await loom.perform('actor.accept', {cap:msg.cap});
    await loom.perform('actor.send', {cap:msg.cap, msg:Array.from('{"action":"row"}', c=>c.charCodeAt(0))});
  }
  if (msg.trap) throw new Error('rollback witness');
  return null;
}
"#;

struct Fixture {
    directory: tempfile::TempDir,
    store: Store,
    hash: String,
}
impl Fixture {
    fn new(allowed: Option<Vec<String>>) -> Self {
        Self::with_source(SOURCE, allowed)
    }
    fn with_source(source: &str, allowed: Option<Vec<String>>) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("definitions.sqlite")).unwrap();
        let identity = loom_proto::javascript_definition_identity(
            source,
            &BTreeMap::new(),
            allowed.as_deref(),
            loom_v8::ABI_VERSION,
        )
        .unwrap();
        let hash = blake3::hash(&identity).to_hex().to_string();
        let def = Def {
            hash: hash.clone(), lang: Lang::JavaScript,
            component_hash: Some(store.put("javascript_source", source.as_bytes()).unwrap()),
            sig: serde_json::from_value(json!({"exports":[{"name":"main","params":[{"name":"msg","shape":{"type":"value"}}],"returns":{"type":"value"}}]})).unwrap(),
            allowed_effects: allowed, observed_effects: vec![],
        };
        store
            .define(&def, Some("javascript_actor"), source, &BTreeMap::new())
            .unwrap();
        Self {
            directory,
            store,
            hash,
        }
    }
    async fn node(&self) -> Node {
        Node::new(
            self.directory.path().join("actors"),
            Arc::new(StoreRegistry::new(self.store.clone())),
            Arc::new(DefaultEffects),
            Config::default(),
        )
        .await
        .unwrap()
    }
}
fn message(value: Value) -> Vec<u8> {
    serde_json::to_vec(&value).unwrap()
}
async fn count(actor: &Actor, table: &str) -> i64 {
    actor
        .sql(&format!("SELECT count(*) FROM {table}"), ())
        .await
        .unwrap()
        .rows[0]
        .get(0)
        .unwrap()
}

#[tokio::test]
async fn v8_sql_survives_actor_reopen() {
    let fixture = Fixture::new(None);
    let id;
    {
        let node = fixture.node().await;
        id = node
            .spawn_root(&fixture.hash, &message(json!({"action":"row"})))
            .await
            .unwrap();
        node.run_until_idle().await.unwrap();
        assert_eq!(count(&node.open(&id).await.unwrap(), "entries").await, 1);
        node.close().await.unwrap();
    }
    let reopened = fixture.node().await;
    reopened
        .send(&id, "second", &message(json!({"action":"row"})))
        .await
        .unwrap();
    reopened.run_until_idle().await.unwrap();
    let actor = reopened.open(&id).await.unwrap();
    assert_eq!(count(&actor, "entries").await, 2);
    assert_eq!(actor.cursor().await.unwrap(), 2);
}

#[tokio::test]
async fn v8_send_commits_and_exception_rolls_back_sql_and_outbox() {
    let fixture = Fixture::new(None);
    let node = fixture.node().await;
    let receiver = node
        .spawn_root(&fixture.hash, &message(json!({"action":"init"})))
        .await
        .unwrap();
    node.run_until_idle().await.unwrap();
    let cap = serde_json::to_vec(&node.cap_for(&receiver, Rights::SEND).await.unwrap()).unwrap();
    let failed = node
        .spawn_root(
            &fixture.hash,
            &message(json!({"action":"send", "cap":cap, "trap":true})),
        )
        .await
        .unwrap();
    node.run_until_idle().await.unwrap();
    let failed_actor = node.open(&failed).await.unwrap();
    assert_eq!(count(&failed_actor, "entries").await, 0);
    assert_eq!(count(&failed_actor, "dead_letters").await, 1);
    assert_eq!(failed_actor.cursor().await.unwrap(), 0);
    assert_eq!(
        failed_actor
            .sql(
                "SELECT count(*) FROM outbox WHERE target=?",
                [receiver.as_str()]
            )
            .await
            .unwrap()
            .rows[0]
            .get::<i64>(0)
            .unwrap(),
        0
    );
    assert_eq!(
        count(&node.open(&receiver).await.unwrap(), "entries").await,
        0
    );
    let sender = node
        .spawn_root(&fixture.hash, &message(json!({"action":"send", "cap":cap})))
        .await
        .unwrap();
    node.run_until_idle().await.unwrap();
    assert_eq!(
        count(&node.open(&receiver).await.unwrap(), "entries").await,
        1
    );
    let actor = node.open(&sender).await.unwrap();
    assert_eq!(count(&actor, "entries").await, 1);
    assert_eq!(
        actor
            .sql(
                "SELECT delivered FROM outbox WHERE target=?",
                [receiver.as_str()]
            )
            .await
            .unwrap()
            .rows[0]
            .get::<i64>(0)
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn v8_forged_capability_is_rejected() {
    let fixture = Fixture::new(None);
    let node = fixture.node().await;
    let receiver = node
        .spawn_root(&fixture.hash, &message(json!({"action":"init"})))
        .await
        .unwrap();
    node.run_until_idle().await.unwrap();
    let mut cap = node.cap_for(&receiver, Rights::SEND).await.unwrap();
    cap.mac[0] ^= 1;
    let sender = node
        .spawn_root(
            &fixture.hash,
            &message(json!({"action":"send", "cap":serde_json::to_vec(&cap).unwrap()})),
        )
        .await
        .unwrap();
    node.run_until_idle().await.unwrap();
    let actor = node.open(&sender).await.unwrap();
    assert_eq!(count(&actor, "dead_letters").await, 1);
    assert_eq!(count(&actor, "entries").await, 0);
    assert_eq!(actor.cursor().await.unwrap(), 0);
    assert_eq!(
        count(&node.open(&receiver).await.unwrap(), "inbox").await,
        1
    );
}

#[tokio::test]
async fn v8_definition_effect_policy_is_enforced() {
    let fixture = Fixture::new(Some(vec![]));
    let node = fixture.node().await;
    let id = node
        .spawn_root(&fixture.hash, &message(json!({"action":"row"})))
        .await
        .unwrap();
    node.run_until_idle().await.unwrap();
    let actor = node.open(&id).await.unwrap();
    assert_eq!(count(&actor, "entries").await, 0);
    let letters = actor
        .sql("SELECT error FROM dead_letters", ())
        .await
        .unwrap();
    assert_eq!(letters.rows.len(), 1);
    let error: String = letters.rows[0].get(0).unwrap();
    assert!(
        error.contains("sql") && error.contains("not allowed"),
        "{error}"
    );
}

struct MeteredSandbox {
    inner: Arc<dyn loom_sandbox::Sandbox>,
    calls: std::sync::atomic::AtomicUsize,
}
impl loom_sandbox::Sandbox for MeteredSandbox {
    fn call<'a>(
        &'a self,
        args: Value,
        effects: &'a mut dyn loom_sandbox::CallEffects,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<Value>> + Send + 'a>>
    {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.call(args, effects)
    }
}
struct ComposedRegistry {
    behavior: Arc<dyn loom_actor::Behavior>,
}
#[async_trait::async_trait]
impl loom_actor::Registry for ComposedRegistry {
    async fn resolve(&self, reference: &str) -> anyhow::Result<Arc<dyn loom_actor::Behavior>> {
        anyhow::ensure!(
            reference == self.behavior.hash(),
            "unknown composed behavior"
        );
        Ok(self.behavior.clone())
    }
    async fn behaviors(&self) -> anyhow::Result<Vec<loom_actor::builtin::BehaviorInfo>> {
        Ok(vec![])
    }
}

#[tokio::test]
async fn sandbox_wrapper_composes_with_durable_actor_effects() {
    let directory = tempfile::tempdir().unwrap();
    let engine = loom_v8::V8Engine::new(loom_v8::Limits::default()).unwrap();
    let guest = engine.compile(SOURCE).await.unwrap();
    let schema = guest.schema().to_owned();
    let metered = Arc::new(MeteredSandbox {
        inner: Arc::new(guest),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let behavior = Arc::new(loom_behavior::LoomBehavior::from_sandbox(
        "composed".into(),
        schema,
        metered.clone(),
    ));
    let node = Node::new(
        directory.path(),
        Arc::new(ComposedRegistry { behavior }),
        Arc::new(DefaultEffects),
        Config::default(),
    )
    .await
    .unwrap();
    let id = node
        .spawn_root("composed", &message(json!({"action":"row"})))
        .await
        .unwrap();
    node.run_until_idle().await.unwrap();
    assert_eq!(metered.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(count(&node.open(&id).await.unwrap(), "entries").await, 1);
}

#[tokio::test]
async fn javascript_actor_refs_send_json_unicode_and_roll_back() {
    let fixture = Fixture::with_source(
        r#"
const LOOM_SCHEMA = 'CREATE TABLE records(amount INTEGER, label TEXT, peer BLOB);';
const main = loom.messages.json(async message => {
  if (message.type === 'init') return;
  if (message.type === 'send') {
    const peer = await loom.actors.accept(message.peer);
    const token = peer.toJSON();
    if (!Object.isFrozen(token)) throw new Error('capability bytes must be frozen');
    await loom.sql('INSERT INTO records(amount,label) VALUES (?,?)', [1, 'sender']);
    await peer.send({type:'increment', amount:2, label:'雪😀 café', peer:token});
    if (message.fail) throw new Error('high-level send rollback');
    return;
  }
  if (message.type === 'increment') {
    const peer = await loom.actors.accept(message.peer);
    await loom.sql('INSERT INTO records(amount,label,peer) VALUES (?,?,?)', [
      message.amount, message.label, loom.sql.blob(peer.toJSON())
    ]);
  }
});
"#,
        None,
    );
    let node = fixture.node().await;
    let receiver = node
        .spawn_root(&fixture.hash, &message(json!({"type":"init"})))
        .await
        .unwrap();
    node.run_until_idle().await.unwrap();
    let cap = serde_json::to_vec(&node.cap_for(&receiver, Rights::SEND).await.unwrap()).unwrap();
    let failed = node
        .spawn_root(
            &fixture.hash,
            &message(json!({"type":"send","peer":cap,"fail":true})),
        )
        .await
        .unwrap();
    node.run_until_idle().await.unwrap();
    let failed_actor = node.open(&failed).await.unwrap();
    assert_eq!(count(&failed_actor, "records").await, 0);
    assert_eq!(count(&failed_actor, "dead_letters").await, 1);
    let failure = failed_actor
        .sql("SELECT error FROM dead_letters", ())
        .await
        .unwrap();
    let reason: String = failure.rows[0].get(0).unwrap();
    assert!(
        reason.contains("high-level send rollback"),
        "unexpected failure: {reason}"
    );
    let receiving = node.open(&receiver).await.unwrap();
    assert_eq!(count(&receiving, "records").await, 0);
    assert_eq!(count(&receiving, "inbox").await, 1);
    let sender = node
        .spawn_root(&fixture.hash, &message(json!({"type":"send","peer":cap})))
        .await
        .unwrap();
    node.run_until_idle().await.unwrap();
    let sending = node.open(&sender).await.unwrap();
    assert_eq!(count(&sending, "records").await, 1);
    assert_eq!(count(&sending, "dead_letters").await, 0);
    assert_eq!(count(&receiving, "dead_letters").await, 0);
    let rows = receiving
        .sql("SELECT amount,label,peer FROM records", ())
        .await
        .unwrap();
    assert_eq!(rows.rows.len(), 1);
    assert_eq!(rows.rows[0].get::<i64>(0).unwrap(), 2);
    assert_eq!(rows.rows[0].get::<String>(1).unwrap(), "雪😀 café");
    assert_eq!(rows.rows[0].get::<Vec<u8>>(2).unwrap(), cap);
}
