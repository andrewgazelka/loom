use super::memo::{self, MemoConfig};
use crate::test_registry::Registry;
use crate::{Behavior, Cap, Config, Ctx, DefaultEffects, Node, Rights, Trap, Verdict, actor};
use async_trait::async_trait;
use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

struct Sender {
    hash: &'static str,
    state: i64,
    send: &'static [u8],
    calls: Arc<AtomicUsize>,
}
#[async_trait]
impl Behavior for Sender {
    fn hash(&self) -> &str {
        self.hash
    }
    fn schema(&self) -> &str {
        "CREATE TABLE IF NOT EXISTS state(value INTEGER)"
    }
    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        cx.sql("INSERT INTO state VALUES (?)", [self.state]).await?;
        cx.effect("echo", b"input").await?;
        let target: Cap = serde_json::from_slice(msg).map_err(|e| Trap::new(e.to_string()))?;
        cx.send(&target, self.send).await
    }
}
struct Fixture {
    dir: tempfile::TempDir,
    node: Node,
    id: String,
    target: String,
    input: Vec<u8>,
    calls: Arc<AtomicUsize>,
}
impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = Registry::new();
        for behavior in [
            Sender { hash: "original", state: 1, send: b"same", calls: calls.clone() },
            Sender { hash: "internal", state: 2, send: b"same", calls: calls.clone() },
            Sender { hash: "changed", state: 1, send: b"different", calls: calls.clone() },
        ] {
            registry.insert(behavior.hash.to_owned(), Arc::new(behavior) as Arc<dyn Behavior>);
        }
        let node = Node::new(dir.path(), Arc::new(registry), Arc::new(DefaultEffects), Config::default()).await.unwrap();
        let target = node.spawn_root("counter-v1", &[]).await.unwrap();
        let input = serde_json::to_vec(&node.cap_for(&target, Rights::ALL).await.unwrap()).unwrap();
        let id = node.spawn_root("original", &input).await.unwrap();
        let cancellation = tokio::sync::Notify::new();
        assert!(node.step(&id, &cancellation).await.unwrap());
        Self { dir, node, id, target, input, calls }
    }
    fn files(&self) -> BTreeSet<String> {
        std::fs::read_dir(self.dir.path()).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect()
    }
    async fn key(&self, candidate: &str, assertions: &[String]) -> String {
        let actor = self.node.open(&self.id).await.unwrap();
        memo::key(&*actor.conn.lock().await, candidate, 0, 1, assertions).await.unwrap()
    }
}

#[tokio::test]
async fn memo_hit_skips_replay() {
    let f = Fixture::new().await;
    let assertions = vec!["SELECT COUNT(*) = 1 FROM state".to_owned()];
    let first = f.node.validate_assertions(&f.id, "original", 1, &assertions).await.unwrap();
    assert!(matches!(first.verdict, Verdict::Matched { .. }));
    assert!(first.assertions[0].passed);
    let files = f.files();
    let calls = f.calls.load(Ordering::SeqCst);
    let second = f.node.validate_assertions(&f.id, "original", 1, &assertions).await.unwrap();
    assert_eq!(serde_json::to_value(first).unwrap(), serde_json::to_value(second).unwrap());
    assert_eq!(files, f.files());
    assert_eq!(calls, f.calls.load(Ordering::SeqCst));
    let rows = actor::connect(&f.dir.path().join("_node.db"), f.node.config.io).await.unwrap();
    assert_eq!(actor::query(&rows, "SELECT key FROM validation_memo", ()).await.unwrap().rows.len(), 1);
    let reopened = Node::new(f.dir.path(), f.node.registry.clone(), Arc::new(DefaultEffects), Config::default()).await.unwrap();
    let calls = f.calls.load(Ordering::SeqCst);
    assert!(matches!(reopened.validate_assertions(&f.id, "original", 1, &assertions).await.unwrap().verdict, Verdict::Matched { .. }));
    assert_eq!(calls, f.calls.load(Ordering::SeqCst));
    let differs = f.node.validate(&f.id, "internal", 1).await.unwrap();
    assert!(matches!(differs, Verdict::Differs { .. }));
    let calls = f.calls.load(Ordering::SeqCst);
    assert_eq!(differs, f.node.validate(&f.id, "internal", 1).await.unwrap());
    assert_eq!(calls, f.calls.load(Ordering::SeqCst));
}

#[tokio::test]
async fn memo_key_moves_with_inputs() {
    let f = Fixture::new().await;
    let original = f.key("original", &[]).await;
    let mut changed = BTreeSet::new();
    changed.insert(f.key("internal", &[]).await);
    changed.insert(f.key("original", &["SELECT 1".into()]).await);
    let actor = f.node.open(&f.id).await.unwrap();
    actor.conn.lock().await.execute("UPDATE inbox SET msg=? WHERE seq=1", [b"changed".as_slice()]).await.unwrap();
    changed.insert(f.key("original", &[]).await);
    actor.conn.lock().await.execute("UPDATE inbox SET msg=? WHERE seq=1", [f.input.as_slice()]).await.unwrap();
    assert_eq!(f.key("original", &[]).await, original);
    actor.conn.lock().await.execute("UPDATE effects SET result=? WHERE seq=1", [b"changed".as_slice()]).await.unwrap();
    changed.insert(f.key("original", &[]).await);
    assert_eq!(changed.len(), 4);
    assert!(!changed.contains(&original));
}

#[tokio::test]
async fn promote_report_cutoff() {
    for candidate in ["internal", "changed"] {
        let f = Fixture::new().await;
        let report = f.node.promote_report(&f.id, candidate, 1).await.unwrap();
        assert_eq!(report.downstream_unaffected, candidate == "internal");
        assert_eq!(report.receivers, vec![f.target.clone()]);
        assert_eq!(f.node.info(&f.id).await.unwrap().behavior_hash, candidate);
        if candidate == "internal" {
            assert!(matches!(report.verdict, Verdict::Differs { .. }));
        }
    }
}

#[tokio::test]
async fn memo_eviction_respects_max_rows() {
    let f = Fixture::new().await;
    let oldest = f.key("original", &[]).await;
    for candidate in ["original", "internal", "changed"] {
        f.node.validate_with_memo_config(&f.id, candidate, 1, &[], MemoConfig { max_rows: 2 }).await.unwrap();
    }
    let conn = actor::connect(&f.dir.path().join("_node.db"), f.node.config.io).await.unwrap();
    let rows = actor::query(&conn, "SELECT key FROM validation_memo ORDER BY created_at", ()).await.unwrap();
    assert_eq!(rows.rows.len(), 2);
    assert!(rows.rows.iter().all(|row| row.get::<String>(0).unwrap() != oldest));
    assert_eq!(rows.rows[0].get::<String>(0).unwrap(), f.key("internal", &[]).await);
}
