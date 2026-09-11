use async_trait::async_trait;
use loom_actor::{Behavior, ChildSpec, Config, Ctx, DefaultEffects, Node, Registry, RestartPolicy, Shutdown, Status, Trap};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio::sync::Notify;

struct Busy {
    started: Arc<Notify>,
    cancelled: Arc<AtomicBool>,
}
struct CancellationWitness {
    cancelled: Arc<AtomicBool>,
}
impl Drop for CancellationWitness {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}
#[async_trait]
impl Behavior for Busy {
    fn hash(&self) -> &str {
        "shutdown-busy-v1"
    }
    fn schema(&self) -> &str {
        ""
    }
    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        if msg == b"init" {
            return cx.trap_exit(true).await;
        }
        if msg == b"busy" {
            let _witness = CancellationWitness { cancelled: self.cancelled.clone() };
            self.started.notify_one();
            std::future::pending::<()>().await;
        }
        // A graceful exit is deliberately ignored; only the timeout stops X.
        Ok(())
    }
}
struct Receiver {
    processed: Arc<Notify>,
}
#[async_trait]
impl Behavior for Receiver {
    fn hash(&self) -> &str {
        "shutdown-receiver-v1"
    }
    fn schema(&self) -> &str {
        "CREATE TABLE received(msg BLOB)"
    }
    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        if msg == b"ping" {
            cx.sql("INSERT INTO received(msg) VALUES (?)", [msg]).await?;
            self.processed.notify_one();
        }
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_wait_does_not_stall_node() {
    let dir = tempfile::tempdir().unwrap();
    let started = Arc::new(Notify::new());
    let cancelled = Arc::new(AtomicBool::new(false));
    let processed = Arc::new(Notify::new());
    let mut registry = Registry::new();
    registry.insert("shutdown-busy-v1".into(), Arc::new(Busy { started: started.clone(), cancelled: cancelled.clone() }));
    registry.insert("shutdown-receiver-v1".into(), Arc::new(Receiver { processed: processed.clone() }));
    let node = Node::new(dir.path(), registry, Arc::new(DefaultEffects), Config::default()).await.unwrap();
    let parent = node.root();
    let y = node.spawn_root("shutdown-receiver-v1", b"init").await.unwrap();
    let mut spec = ChildSpec::new("shutdown-busy-v1", b"init");
    spec.restart = RestartPolicy::Temporary;
    spec.shutdown = Shutdown::TimeoutMs(300);
    node.send(&parent, "start-x", &serde_json::to_vec(&serde_json::json!({"type":"start_child","spec":spec})).unwrap()).await.unwrap();
    node.run_until_idle().await.unwrap();
    let parent_actor = node.open(&parent).await.unwrap();
    let rows = parent_actor.sql("SELECT id FROM children WHERE behavior_hash='shutdown-busy-v1'", ()).await.unwrap();
    let x: String = rows.rows[0].get(0).unwrap();

    node.send(&x, "busy", b"busy").await.unwrap();
    let runner = {
        let node = node.clone();
        tokio::spawn(async move { node.run_until_idle().await })
    };
    tokio::time::timeout(Duration::from_secs(2), started.notified()).await.unwrap();
    node.send(&parent, "stop-x", &serde_json::to_vec(&serde_json::json!({"type":"terminate_child","id":x})).unwrap()).await.unwrap();
    // Wait for the production shutdown control to exist, while X owns its SQL transaction.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let pending = parent_actor.sql("SELECT child FROM shutdowns WHERE child=?", [x.as_str()]).await.unwrap();
            if !pending.rows.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert!(!cancelled.load(Ordering::SeqCst));
    let receiver = node.open(&y).await.unwrap();
    let before = receiver.cursor().await.unwrap();
    tokio::time::timeout(Duration::from_millis(50), async {
        node.send(&y, "ping", b"ping").await.unwrap();
        processed.notified().await;
        let rows = receiver.sql("SELECT COUNT(*) FROM received", ()).await.unwrap();
        assert_eq!(rows.rows[0].get::<i64>(0).unwrap(), 1);
        assert_eq!(receiver.cursor().await.unwrap(), before + 1);
    })
    .await
    .expect("Y must commit while X is still waiting for its 300ms shutdown timeout");
    assert!(!cancelled.load(Ordering::SeqCst), "X was killed before Y committed");
    tokio::time::timeout(Duration::from_secs(3), runner).await.unwrap().unwrap().unwrap();
    assert!(cancelled.load(Ordering::SeqCst));
    assert_eq!(node.open(&x).await.unwrap().status().await.unwrap(), Status::Stopped);
    assert_eq!(node.info(&x).await.unwrap().reason, "killed");
}
