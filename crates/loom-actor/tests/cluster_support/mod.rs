use std::{
    path::Path,
    sync::{Arc, atomic::{AtomicBool, AtomicU64, Ordering}},
    time::Duration,
};

use async_trait::async_trait;
use axum::{Json, Router, extract::State, http::{HeaderMap, StatusCode}, response::{IntoResponse, Response}, routing::post};
use loom_actor::{
    Actor, Behavior, Cap, ChildSpec, ChildType, Clock, ClusterConfig, Config, Ctx, DefaultEffects, Durability, EffectError,
    EffectHandler, EffectKey, IngressRequest, Node, Rights, StoreConfig, Trap,
};
use serde_json::Value;
use tokio::sync::Notify;

pub const KEY: [u8; 32] = [0x43; 32];
pub const USER_TOKEN: &str = "cluster-test-user";

#[derive(Debug, Default)]
pub struct ManualClock {
    millis: AtomicU64,
}
impl Clock for ManualClock {
    fn now_ms(&self) -> anyhow::Result<u64> {
        Ok(self.millis.load(Ordering::SeqCst))
    }
}
impl ManualClock {
    pub async fn expire_except(&self, survivor: &Node) {
        self.millis.fetch_add(1_800_000, Ordering::SeqCst);
        survivor.renew_leases().await.unwrap();
        self.millis.fetch_add(1_800_001, Ordering::SeqCst);
    }
}

#[derive(Default)]
pub struct GatedEffects {
    pub armed: AtomicBool,
    pub entered: Notify,
    pub release: Notify,
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

struct BlockThenForward;
#[async_trait]
impl Behavior for BlockThenForward {
    fn hash(&self) -> &str { "blocked-forwarder-v1" }
    fn schema(&self) -> &str { "CREATE TABLE received(msg BLOB)" }
    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        cx.sql("INSERT INTO received VALUES (?)", [msg]).await?;
        if msg == b"init" { return Ok(()); }
        let cap: Cap = serde_json::from_slice(msg).map_err(|error| Trap::new(error.to_string()))?;
        cx.accept(cap.clone()).await?;
        cx.effect("echo", msg).await?;
        cx.send(&cap, b"stale-output").await
    }
}

struct Fanout;
#[async_trait]
impl Behavior for Fanout {
    fn hash(&self) -> &str { "fanout-v1" }
    fn schema(&self) -> &str { "" }
    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        let caps: Vec<Cap> = serde_json::from_slice(msg).map_err(|error| Trap::new(error.to_string()))?;
        for cap in caps {
            cx.accept(cap.clone()).await?;
            cx.send(&cap, b"fanout").await?;
        }
        Ok(())
    }
}

#[derive(Clone)]
struct IngressState {
    node: Node,
    // Server::crash releases held requests before dropping the listener's Node.
    stopped: Arc<Notify>,
}
// The production middleware's route-exclusive auth controls are also tested in
// loom-api: importing that crate here would create an actor -> API -> actor cycle.
async fn ingress(State(state): State<IngressState>, headers: HeaderMap, Json(request): Json<IngressRequest>) -> Response {
    let bearer = headers.get("authorization").and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer ")).unwrap_or("");
    if bearer == USER_TOKEN {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !state.node.is_ingress_bearer(bearer) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    tokio::select! {
        acks = state.node.apply_ingress(request.ops) => {
            let status = if acks.iter().all(|ack| ack.ok) { StatusCode::OK } else { StatusCode::CONFLICT };
            let mut response = Json(loom_actor::IngressResponse::new(acks)).into_response();
            *response.status_mut() = status;
            response
        },
        _ = state.stopped.notified() => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

async fn command(State(state): State<IngressState>, headers: HeaderMap) -> StatusCode {
    let bearer = headers.get("authorization").and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer ")).unwrap_or("");
    if state.node.is_ingress_bearer(bearer) { StatusCode::FORBIDDEN } else { StatusCode::UNAUTHORIZED }
}

pub struct Server {
    pub addr: String,
    stopped: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}
impl Server {
    pub fn crash(&self) {
        self.stopped.notify_waiters();
        self.task.abort();
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.crash();
    }
}

pub struct RunningNode {
    pub node: Node,
    pub server: Server,
}
pub async fn node(dir: &Path, store: &Path, id: &str, clock: Arc<ManualClock>, effects: Arc<dyn EffectHandler>) -> RunningNode {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let mut registry = crate::registry::Registry::new();
    registry.insert("blocked-forwarder-v1".into(), Arc::new(BlockThenForward));
    registry.insert("fanout-v1".into(), Arc::new(Fanout));
    let node = Node::new(
        dir, Arc::new(registry), effects,
        Config {
            store: Some(StoreConfig::Local { path: store.to_owned() }),
            cluster: Some(ClusterConfig { node_id: id.into(), addr: addr.clone(), key: KEY }),
            ship_interval: Duration::from_millis(20),
            lease_ttl: Duration::from_secs(3600),
            lease_clock: clock,
            ..Config::default()
        },
    ).await.unwrap();
    let stopped = Arc::new(Notify::new());
    let app = Router::new().route("/v1/ingress", post(ingress)).route("/v1/command", post(command))
        .with_state(IngressState { node: node.clone(), stopped: stopped.clone() });
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
    RunningNode { node, server: Server { addr, stopped, task } }
}

pub struct Pair {
    pub clock: Arc<ManualClock>,
    pub n1: RunningNode,
    pub n2: RunningNode,
    pub a: String,
    pub b: String,
    pub cap: Cap,
    // Drop node handles/listeners before the temporary directory removes files.
    pub workspace: tempfile::TempDir,
}
impl Pair {
    pub async fn new(sender_durability: Durability) -> Self {
        let workspace = tempfile::tempdir().unwrap();
        let store = workspace.path().join("store");
        let clock = Arc::new(ManualClock::default());
        let n1 = node(&workspace.path().join("n1"), &store, "n1", clock.clone(), Arc::new(DefaultEffects)).await;
        let n2 = node(&workspace.path().join("n2"), &store, "n2", clock.clone(), Arc::new(DefaultEffects)).await;
        let b = spawn(&n2.node, "counter-v1", b"init", Durability::Local).await;
        drain(&n2.node).await;
        let cap = n2.node.cap_for(&b, Rights::SEND).await.unwrap();
        // A's init is its first send, kept pending until each test starts it.
        let a = spawn(&n1.node, "forwarder-v1", &serde_json::to_vec(&cap).unwrap(), sender_durability).await;
        Self { workspace, clock, n1, n2, a, b, cap }
    }
    pub fn store(&self) -> std::path::PathBuf {
        self.workspace.path().join("store")
    }
    pub async fn send(&self, key: &str) {
        self.n1.node.send(&self.a, key, &serde_json::to_vec(&self.cap).unwrap()).await.unwrap();
    }
}

pub async fn spawn(node: &Node, hash: &str, init: &[u8], durability: Durability) -> String {
    let mut spec = ChildSpec::new(hash, init, ChildType::Worker);
    spec.link = false;
    spec.restart = loom_actor::RestartPolicy::Temporary;
    spec.durability = durability;
    node.spawn(&node.root(), &spec).await.unwrap()
}
pub async fn drain(node: &Node) {
    tokio::time::timeout(Duration::from_secs(30), node.run_until_idle()).await.unwrap().unwrap();
}
pub async fn integer(actor: &Actor, sql: &str) -> i64 {
    let rows = actor.sql(sql, ()).await.unwrap();
    assert_eq!(rows.rows.len(), 1);
    rows.rows[0].get(0).unwrap()
}
pub async fn forwarded(node: &Node, id: &str) -> i64 {
    integer(&node.open(id).await.unwrap(), "SELECT COUNT(*) FROM inbox WHERE key != 'init' AND sender != 'external'").await
}
pub async fn wait_forwarded(node: &Node, id: &str, count: i64) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if forwarded(node, id).await == count { break; }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
}
pub fn lease(store: &Path, id: &str) -> Value {
    serde_json::from_slice(&std::fs::read(store.join(format!("actors/{id}/lease"))).unwrap()).unwrap()
}

pub async fn held_ack_does_not_block_another_pair() {
    let pair = Pair::new(Durability::Local).await;
    drain(&pair.n1.node).await;
    let local = spawn(&pair.n1.node, "counter-v1", b"", Durability::Local).await;
    let local_cap = pair.n1.node.cap_for(&local, Rights::SEND).await.unwrap();
    // The held remote destination is first in outbox order: a serial dispatcher
    // cannot reach the later local row until the test explicitly resumes shipping.
    let caps = vec![pair.cap.clone(), local_cap];
    let fanout = spawn(&pair.n1.node, "fanout-v1", &serde_json::to_vec(&caps).unwrap(), Durability::Local).await;
    pair.n2.node.ship(&pair.b).await.unwrap();
    pair.n2.node.pause_shipping();
    let runner = pair.n1.node.clone();
    let mut attempt = tokio::spawn(async move { runner.run_until_idle().await });
    wait_forwarded(&pair.n2.node, &pair.b, 2).await;
    wait_forwarded(&pair.n1.node, &local, 1).await;
    assert!(tokio::time::timeout(Duration::from_millis(100), &mut attempt).await.is_err());
    let sender = pair.n1.node.open(&fanout).await.unwrap();
    assert_eq!(integer(&sender, "SELECT COUNT(*) FROM outbox").await, 2);
    let held = sender.sql("SELECT delivered FROM outbox WHERE target=?", [pair.b.as_str()]).await.unwrap();
    assert_eq!(held.rows.len(), 1);
    assert_eq!(held.rows[0].get::<i64>(0).unwrap(), 0);
    assert_eq!(forwarded(&pair.n1.node, &local).await, 1);
    pair.n2.node.resume_shipping();
    tokio::time::timeout(Duration::from_secs(10), attempt).await.unwrap().unwrap().unwrap();
    assert_eq!(integer(&sender, "SELECT COUNT(*) FROM outbox WHERE delivered=1").await, 2);
}
