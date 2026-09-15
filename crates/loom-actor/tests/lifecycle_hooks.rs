mod registry;
use async_trait::async_trait;
use loom_actor::{Actor, Behavior, Config, Ctx, DefaultEffects, Node, Status, Trap, Verdict};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

struct Lifecycle {
    fail_start: Arc<AtomicBool>,
    fail_stop: Arc<AtomicBool>,
}
#[async_trait]
impl Behavior for Lifecycle {
    fn hash(&self) -> &str {
        "lifecycle-hooks"
    }
    fn schema(&self) -> &str {
        "CREATE TABLE events(kind TEXT);"
    }
    fn has_startup(&self) -> bool {
        true
    }
    fn has_shutdown(&self) -> bool {
        true
    }
    async fn startup(&self, cx: &mut Ctx<'_>) -> Result<(), Trap> {
        cx.sql("INSERT INTO events VALUES ('start')", ()).await?;
        if self.fail_start.load(Ordering::SeqCst) {
            return Err(Trap::new("start refused"));
        }
        Ok(())
    }
    async fn handle(&self, cx: &mut Ctx<'_>, _: &[u8]) -> Result<(), Trap> {
        cx.sql("INSERT INTO events VALUES ('message')", ()).await?;
        Ok(())
    }
    async fn terminate(&self, cx: &mut Ctx<'_>, reason: &str) -> Result<(), Trap> {
        cx.sql("INSERT INTO events VALUES (?)", [reason]).await?;
        if self.fail_stop.load(Ordering::SeqCst) {
            return Err(Trap::new("stop refused"));
        }
        Ok(())
    }
}
fn registry(fail_start: Arc<AtomicBool>, fail_stop: Arc<AtomicBool>) -> Arc<registry::Registry> {
    let mut registry = registry::Registry::new();
    registry.insert("lifecycle-hooks".into(), Arc::new(Lifecycle { fail_start, fail_stop }));
    Arc::new(registry)
}
async fn count(actor: &Actor, kind: &str) -> i64 {
    actor.sql("SELECT COUNT(*) FROM events WHERE kind=?", [kind]).await.unwrap().rows[0].get(0).unwrap()
}

#[tokio::test]
async fn graceful_close_reopen_runs_once_and_replays() {
    let dir = tempfile::tempdir().unwrap();
    let registry = registry(Default::default(), Default::default());
    let node = Node::new(dir.path(), registry.clone(), Arc::new(DefaultEffects), Config::default()).await.unwrap();
    let id = node.spawn_root("lifecycle-hooks", b"init").await.unwrap();
    node.run_until_idle().await.unwrap();
    let actor = node.open(&id).await.unwrap();
    assert_eq!(count(&actor, "start").await, 1);
    node.open(&id).await.unwrap();
    node.send(&id, "forged-start", br#"{"type":"startup"}"#).await.unwrap();
    node.run_until_idle().await.unwrap();
    assert_eq!(count(&actor, "start").await, 1);
    assert_eq!(count(&actor, "message").await, 2);
    let cursor = actor.cursor().await.unwrap();
    assert!(matches!(node.validate(&id, "lifecycle-hooks", cursor).await.unwrap(), Verdict::Matched { .. }));
    let fork = node.fork(&id, cursor).await.unwrap();
    assert_eq!(count(&node.open(&fork).await.unwrap(), "start").await, 1);
    node.close().await.unwrap();
    node.close().await.unwrap();
    assert_eq!(count(&actor, "node_shutdown").await, 1);
    assert_eq!(actor.status().await.unwrap(), Status::Running);
    drop(actor);
    drop(node);
    let node = Node::new(dir.path(), registry, Arc::new(DefaultEffects), Config::default()).await.unwrap();
    let actor = node.open(&id).await.unwrap();
    assert_eq!(count(&actor, "start").await, 2);
    assert_eq!(count(&actor, "node_shutdown").await, 1);
    assert!(matches!(node.validate(&id, "lifecycle-hooks", actor.cursor().await.unwrap()).await.unwrap(), Verdict::Matched { .. }));
    node.close().await.unwrap();
}

#[tokio::test]
async fn failed_shutdown_rolls_back_and_can_retry() {
    let dir = tempfile::tempdir().unwrap();
    let fail = Arc::new(AtomicBool::new(true));
    let node =
        Node::new(dir.path(), registry(Default::default(), fail.clone()), Arc::new(DefaultEffects), Config::default()).await.unwrap();
    let id = node.spawn_root("lifecycle-hooks", b"init").await.unwrap();
    node.run_until_idle().await.unwrap();
    let actor = node.open(&id).await.unwrap();
    assert!(node.close().await.unwrap_err().to_string().contains("stop refused"));
    assert_eq!(count(&actor, "node_shutdown").await, 0);
    assert_eq!(actor.status().await.unwrap(), Status::Running);
    fail.store(false, Ordering::SeqCst);
    node.close().await.unwrap();
    assert_eq!(count(&actor, "node_shutdown").await, 1);
}

#[tokio::test]
async fn failed_reactivation_rolls_back_and_node_open_can_retry() {
    let dir = tempfile::tempdir().unwrap();
    let fail = Arc::new(AtomicBool::new(false));
    let registry = registry(fail.clone(), Default::default());
    let node = Node::new(dir.path(), registry.clone(), Arc::new(DefaultEffects), Config::default()).await.unwrap();
    let id = node.spawn_root("lifecycle-hooks", b"init").await.unwrap();
    node.run_until_idle().await.unwrap();
    node.close().await.unwrap();
    drop(node);
    fail.store(true, Ordering::SeqCst);
    let error = Node::new(dir.path(), registry.clone(), Arc::new(DefaultEffects), Config::default()).await.err().unwrap();
    assert!(format!("{error:#}").contains("start refused"));
    fail.store(false, Ordering::SeqCst);
    let node = Node::new(dir.path(), registry, Arc::new(DefaultEffects), Config::default()).await.unwrap();
    assert_eq!(count(&node.open(&id).await.unwrap(), "start").await, 2);
    node.close().await.unwrap();
}

struct CleanupOwner {
    fail_stop: bool,
}
#[async_trait]
impl Behavior for CleanupOwner {
    fn hash(&self) -> &str {
        "cleanup-owner"
    }
    fn schema(&self) -> &str {
        "CREATE TABLE resource(cap BLOB)"
    }
    fn has_startup(&self) -> bool {
        true
    }
    fn has_shutdown(&self) -> bool {
        true
    }
    async fn startup(&self, cx: &mut Ctx<'_>) -> Result<(), Trap> {
        let cap = cx.spawn_driver("cleanup-driver", b"").await?;
        cx.sql("DELETE FROM resource", ()).await?;
        cx.sql("INSERT INTO resource VALUES (?)", [serde_json::to_vec(&cap).unwrap()]).await?;
        Ok(())
    }
    async fn handle(&self, _: &mut Ctx<'_>, _: &[u8]) -> Result<(), Trap> {
        Ok(())
    }
    async fn terminate(&self, cx: &mut Ctx<'_>, _: &str) -> Result<(), Trap> {
        let rows = cx.sql("SELECT cap FROM resource", ()).await?;
        let bytes: Vec<u8> = rows.rows[0].get(0).unwrap();
        let cap = serde_json::from_slice(&bytes).unwrap();
        cx.send(&cap, b"cleanup").await?;
        if self.fail_stop {
            return Err(Trap::new("cleanup refused"));
        }
        Ok(())
    }
}
struct CleanupDriver {
    log: Arc<std::sync::Mutex<Vec<String>>>,
}
struct DriverDrop {
    log: Arc<std::sync::Mutex<Vec<String>>>,
}
impl Drop for DriverDrop {
    fn drop(&mut self) {
        self.log.lock().unwrap().push("drop".into());
    }
}
#[async_trait]
impl loom_actor::Driver for CleanupDriver {
    fn hash(&self) -> &str {
        "cleanup-driver"
    }
    async fn run(
        &self,
        cx: loom_actor::DriverContext,
        _: &[u8],
        mut deliveries: tokio::sync::mpsc::Receiver<loom_actor::DriverDelivery>,
    ) -> anyhow::Result<()> {
        let _drop = DriverDrop { log: self.log.clone() };
        self.log.lock().unwrap().push(cx.id().to_owned());
        while let Some(delivery) = deliveries.recv().await {
            self.log.lock().unwrap().push(String::from_utf8(delivery.bytes.clone())?);
            delivery.acknowledge(Ok(loom_actor::DriverAck::Delivered));
        }
        Ok(())
    }
}
#[tokio::test]
async fn shutdown_delivers_cleanup_before_driver_drop_and_reopen_uses_new_identity() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut registry = registry::Registry::new();
    registry.insert("cleanup-owner".into(), Arc::new(CleanupOwner { fail_stop: false }));
    registry.insert_driver(Arc::new(CleanupDriver { log: log.clone() }));
    let registry = Arc::new(registry);
    let node = Node::new(dir.path(), registry.clone(), Arc::new(DefaultEffects), Config::default()).await.unwrap();
    let id = node.spawn_root("cleanup-owner", b"init").await.unwrap();
    node.run_until_idle().await.unwrap();
    node.close().await.unwrap();
    let first = log.lock().unwrap().clone();
    assert_eq!(first.len(), 3);
    assert_eq!(&first[1..], &["cleanup", "drop"]);
    drop(node);
    let node = Node::new(dir.path(), registry, Arc::new(DefaultEffects), Config::default()).await.unwrap();
    node.run_until_idle().await.unwrap();
    let actor = node.open(&id).await.unwrap();
    assert!(matches!(node.validate(&id, "cleanup-owner", actor.cursor().await.unwrap()).await.unwrap(), Verdict::Matched { .. }));
    node.close().await.unwrap();
    let log = log.lock().unwrap();
    assert_eq!(log.len(), 6);
    assert_ne!(log[0], log[3], "activation must create a new resource identity");
    assert_eq!(&log[4..], &["cleanup", "drop"]);
}

#[tokio::test]
async fn shutdown_trap_still_closes_native_resources() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut registry = registry::Registry::new();
    registry.insert("cleanup-owner".into(), Arc::new(CleanupOwner { fail_stop: true }));
    registry.insert_driver(Arc::new(CleanupDriver { log: log.clone() }));
    let node = Node::new(dir.path(), Arc::new(registry), Arc::new(DefaultEffects), Config::default()).await.unwrap();
    let id = node.spawn_root("cleanup-owner", b"init").await.unwrap();
    node.run_until_idle().await.unwrap();
    let actor = node.open(&id).await.unwrap();
    assert!(node.close().await.unwrap_err().to_string().contains("cleanup refused"));
    assert_eq!(actor.status().await.unwrap(), Status::Running);
    let log = log.lock().unwrap();
    assert_eq!(log.len(), 2);
    assert_eq!(log[1], "drop");
    assert!(!log.iter().any(|event| event == "cleanup"), "trapped cleanup send must roll back");
}

struct CandidateLifecycle {
    hash: &'static str,
    changed: bool,
    extra_effect: bool,
}
#[async_trait]
impl Behavior for CandidateLifecycle {
    fn hash(&self) -> &str {
        self.hash
    }
    fn schema(&self) -> &str {
        "CREATE TABLE IF NOT EXISTS events(kind TEXT)"
    }
    fn has_startup(&self) -> bool {
        true
    }
    fn has_shutdown(&self) -> bool {
        true
    }
    async fn startup(&self, cx: &mut Ctx<'_>) -> Result<(), Trap> {
        if self.extra_effect {
            cx.effect("echo", b"new startup effect").await?;
        }
        cx.sql("INSERT INTO events VALUES (?)", [if self.changed { "changed-start" } else { "start" }]).await?;
        Ok(())
    }
    async fn handle(&self, cx: &mut Ctx<'_>, _: &[u8]) -> Result<(), Trap> {
        cx.sql("INSERT INTO events VALUES ('message')", ()).await?;
        Ok(())
    }
    async fn terminate(&self, cx: &mut Ctx<'_>, _: &str) -> Result<(), Trap> {
        cx.sql("INSERT INTO events VALUES (?)", [if self.changed { "changed-stop" } else { "stop" }]).await?;
        Ok(())
    }
}
#[tokio::test]
async fn changed_candidate_replays_lifecycle_without_synthetic_upgrade_collision() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = registry::Registry::new();
    for behavior in [
        CandidateLifecycle { hash: "original", changed: false, extra_effect: false },
        CandidateLifecycle { hash: "equivalent", changed: false, extra_effect: false },
        CandidateLifecycle { hash: "changed-state", changed: true, extra_effect: false },
        CandidateLifecycle { hash: "changed-effect", changed: false, extra_effect: true },
    ] {
        registry.insert(behavior.hash.into(), Arc::new(behavior));
    }
    let registry = Arc::new(registry);
    let node = Node::new(dir.path(), registry.clone(), Arc::new(DefaultEffects), Config::default()).await.unwrap();
    let id = node.spawn_root("original", b"init").await.unwrap();
    node.run_until_idle().await.unwrap();
    node.close().await.unwrap();
    drop(node);
    let node = Node::new(dir.path(), registry, Arc::new(DefaultEffects), Config::default()).await.unwrap();
    node.send(&id, "after-restart", b"message").await.unwrap();
    node.run_until_idle().await.unwrap();
    let actor = node.open(&id).await.unwrap();
    let cursor = actor.cursor().await.unwrap();
    let snapshot_path: String = actor.sql("SELECT path FROM snapshots ORDER BY seq LIMIT 1", ()).await.unwrap().rows[0].get(0).unwrap();
    let before = snapshot_cdc_boundary(&snapshot_path).await;
    assert!(matches!(node.validate(&id, "equivalent", cursor).await.unwrap(), Verdict::Matched { .. }));
    assert_eq!(snapshot_cdc_boundary(&snapshot_path).await, before, "validation mutated the immutable snapshot CDC boundary");
    assert!(matches!(node.validate(&id, "changed-state", cursor).await.unwrap(), Verdict::Differs { .. }));
    let control_verdict = node.validate(&id, "original", cursor).await.unwrap();
    assert!(matches!(control_verdict, Verdict::Matched { .. }), "source replay changed after candidate validation: {control_verdict:?}");
    let effect_verdict = node.validate(&id, "changed-effect", cursor).await.unwrap();
    assert!(matches!(effect_verdict, Verdict::DivergedAt { seq, .. } if seq < 0 && seq != i64::MIN), "{effect_verdict:?}");
    assert_eq!(count(&actor, "start").await, 2, "validation must not activate the source again");
    node.close().await.unwrap();
}

async fn snapshot_cdc_boundary(path: &str) -> i64 {
    let db = turso::Builder::new_local(path).build().await.unwrap();
    let conn = db.connect().unwrap();
    conn.execute("PRAGMA query_only=1", ()).await.unwrap();
    let mut rows = conn.query("SELECT COALESCE(MAX(change_id),0) FROM turso_cdc", ()).await.unwrap();
    rows.next().await.unwrap().unwrap().get(0).unwrap()
}
