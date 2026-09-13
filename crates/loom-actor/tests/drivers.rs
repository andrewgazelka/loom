#[allow(dead_code, unused_imports)] // Reuse the shared fixture helpers across independently registered targets.
mod common;
mod registry;

use async_trait::async_trait;
use common::integer;
use loom_actor::{Actor, Behavior, Cap, Config, Ctx, DefaultEffects, Driver, DriverContext, DriverDelivery, Io, Node, Trap};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
};

const OWNER: &str = "driver-owner-v1";
struct Owner {
    trap: bool,
    hash: &'static str,
}
#[async_trait]
impl Behavior for Owner {
    fn hash(&self) -> &str {
        self.hash
    }
    fn schema(&self) -> &str {
        "CREATE TABLE IF NOT EXISTS driver_state(cap TEXT);"
    }
    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        if msg == b"start" {
            let cap = cx.spawn_driver(loom_actor::drivers::tcp::HASH, br#"{"bind":"127.0.0.1:0"}"#).await?;
            cx.sql("INSERT INTO driver_state VALUES (?)", [serde_json::to_string(&cap).unwrap()]).await?;
        } else if msg == b"reply" {
            let cap = cx.sender_cap().await?;
            cx.send(&cap, b"committed").await?;
            if self.trap {
                return Err(Trap::new("reply rolled back"));
            }
        } else if let Ok(value) = serde_json::from_slice::<serde_json::Value>(msg) {
            if value["type"] == "send" {
                let cap: Cap = serde_json::from_value(value["cap"].clone()).unwrap();
                cx.accept(cap.clone()).await?;
                cx.send(&cap, b"later").await?;
            } else if value["type"] == "revoke" {
                let cap: Cap = serde_json::from_value(value["cap"].clone()).unwrap();
                let attenuated = cx.attenuate(&cap, loom_actor::Rights::NONE).await?;
                cx.sql("INSERT INTO driver_state VALUES (?)", [serde_json::to_string(&attenuated).unwrap()]).await?;
                cx.revoke(cap.cap_id).await?;
            }
        }
        Ok(())
    }
}
struct Fixture {
    _dir: tempfile::TempDir,
    node: Node,
    owner: String,
    actor: Actor,
    addr: String,
}
impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = registry::Registry::new();
        registry.insert(OWNER.into(), Arc::new(Owner { trap: false, hash: OWNER }));
        registry.insert("trapping-owner".into(), Arc::new(Owner { trap: true, hash: "trapping-owner" }));
        registry.insert_driver(Arc::new(loom_actor::drivers::tcp::TcpListenerDriver));
        let config = Config { io: Io::Memory, ..Config::default() };
        let node = Node::new(dir.path(), Arc::new(registry), Arc::new(DefaultEffects), config).await.unwrap();
        let owner = node.spawn_root(OWNER, b"start").await.unwrap();
        node.run_until_idle().await.unwrap();
        let actor = node.open(&owner).await.unwrap();
        wait_rows(&actor, "SELECT msg FROM inbox WHERE sender LIKE 'drv:%' AND CAST(msg AS TEXT) LIKE '%\"listening\"%'").await;
        let rows =
            actor.sql("SELECT msg FROM inbox WHERE sender LIKE 'drv:%' AND CAST(msg AS TEXT) LIKE '%\"listening\"%'", ()).await.unwrap();
        let bytes: Vec<u8> = rows.rows[0].get(0).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        Self { _dir: dir, node, owner, actor, addr: value["addr"].as_str().unwrap().into() }
    }
    async fn frame(&self, bytes: &[u8]) -> TcpStream {
        let mut socket = TcpStream::connect(&self.addr).await.unwrap();
        write_frame(&mut socket, bytes).await;
        wait_rows(&self.actor, "SELECT key FROM inbox WHERE key LIKE 'conn:%:1'").await;
        socket
    }
    async fn cap(&self, key: &str) -> Cap {
        let rows = self
            .actor
            .sql("SELECT c.target,c.cap_id,c.epoch,c.rights,c.mac FROM caps c JOIN inbox i ON i.sender=c.target WHERE i.key=?", [key])
            .await
            .unwrap();
        let row = &rows.rows[0];
        Cap {
            target: row.get(0).unwrap(),
            cap_id: row.get::<i64>(1).unwrap() as u64,
            epoch: row.get::<String>(2).unwrap().parse().unwrap(),
            rights: loom_actor::Rights { bits: row.get::<i64>(3).unwrap() as u64 },
            mac: row.get::<Vec<u8>>(4).unwrap().try_into().unwrap(),
        }
    }
    async fn first_key(&self) -> String {
        self.actor.sql("SELECT key FROM inbox WHERE key LIKE 'conn:%:1' ORDER BY seq LIMIT 1", ()).await.unwrap().rows[0].get(0).unwrap()
    }
}
async fn wait_rows(actor: &Actor, sql: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if !actor.sql(sql, ()).await.unwrap().rows.is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("driver event did not arrive");
}
async fn write_frame(socket: &mut TcpStream, bytes: &[u8]) {
    socket.write_u32(u32::try_from(bytes.len()).unwrap()).await.unwrap();
    socket.write_all(bytes).await.unwrap();
}
async fn read_frame(socket: &mut TcpStream) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let length = socket.read_u32().await.unwrap();
        let mut bytes = vec![0; usize::try_from(length).unwrap()];
        socket.read_exact(&mut bytes).await.unwrap();
        bytes
    })
    .await
    .expect("reply did not arrive")
}
async fn no_frame(socket: &mut TcpStream) {
    let mut byte = [0];
    assert!(tokio::time::timeout(Duration::from_millis(100), socket.read(&mut byte)).await.is_err(), "unexpected socket data or EOF");
}

#[tokio::test]
async fn frame_in_becomes_keyed_message() {
    let f = Fixture::new().await;
    let _socket = f.frame(b"hello").await;
    let rows = f.actor.sql("SELECT key,sender,msg FROM inbox WHERE key LIKE 'conn:%:1'", ()).await.unwrap();
    assert_eq!(rows.rows.len(), 1);
    let key: String = rows.rows[0].get(0).unwrap();
    let sender: String = rows.rows[0].get(1).unwrap();
    assert!(sender.starts_with("drv:"));
    assert_eq!(key, format!("conn:{}:1", sender.rsplit(':').next().unwrap()));
    assert_eq!(rows.rows[0].get::<Vec<u8>>(2).unwrap(), b"hello");
    assert_eq!(f.cap(&key).await.target, sender);
    f.node.close().await.unwrap();
}

#[tokio::test]
async fn reply_after_commit_only() {
    let f = Fixture::new().await;
    f.node.promote(&f.owner, "trapping-owner", "test", "trap after send").await.unwrap();
    let mut socket = f.frame(b"reply").await;
    f.node.run_until_idle().await.unwrap();
    assert_eq!(integer(&f.actor, "SELECT COUNT(*) FROM outbox WHERE target LIKE 'drv:%' AND target NOT LIKE 'drv:spawn:%'").await, 0);
    no_frame(&mut socket).await;
    f.node.promote(&f.owner, OWNER, "test", "retry same inbox row").await.unwrap();
    f.node.run_until_idle().await.unwrap();
    assert_eq!(read_frame(&mut socket).await, b"committed");
    no_frame(&mut socket).await;
    f.node.close().await.unwrap();
}

#[tokio::test]
async fn redelivery_is_once() {
    let f = Fixture::new().await;
    let mut socket = f.frame(b"reply").await;
    f.node.run_until_idle().await.unwrap();
    assert_eq!(read_frame(&mut socket).await, b"committed");
    f.actor.sql("UPDATE outbox SET delivered=0 WHERE target LIKE 'drv:%' AND target NOT LIKE 'drv:spawn:%'", ()).await.unwrap();
    f.node.pump(&f.owner).await.unwrap();
    assert_eq!(integer(&f.actor, "SELECT COUNT(*) FROM outbox WHERE delivered=0").await, 0);
    no_frame(&mut socket).await;
    f.node.close().await.unwrap();
}

#[tokio::test]
async fn closed_handle_drops() {
    let f = Fixture::new().await;
    let socket = f.frame(b"hello").await;
    let closed_cap = f.cap(&f.first_key().await).await;
    drop(socket);
    wait_rows(&f.actor, "SELECT key FROM inbox WHERE key LIKE 'conn:%:closed'").await;
    let mut live = TcpStream::connect(&f.addr).await.unwrap();
    write_frame(&mut live, b"hello").await;
    wait_rows(&f.actor, "SELECT key FROM inbox WHERE key LIKE 'conn:%:1' ORDER BY seq LIMIT 1 OFFSET 1").await;
    let live_key: String =
        f.actor.sql("SELECT key FROM inbox WHERE key LIKE 'conn:%:1' ORDER BY seq DESC LIMIT 1", ()).await.unwrap().rows[0].get(0).unwrap();
    let live_cap = f.cap(&live_key).await;
    f.node.send(&f.owner, "send-closed", &serde_json::to_vec(&serde_json::json!({"type":"send","cap":closed_cap})).unwrap()).await.unwrap();
    f.node.send(&f.owner, "send-live", &serde_json::to_vec(&serde_json::json!({"type":"send","cap":live_cap})).unwrap()).await.unwrap();
    f.node.run_until_idle().await.unwrap();
    assert_eq!(integer(&f.actor, "SELECT COUNT(*) FROM meta WHERE key LIKE 'driver_drop:%'").await, 1);
    assert_eq!(integer(&f.actor, "SELECT COUNT(*) FROM outbox WHERE delivered=0").await, 0);
    assert_eq!(read_frame(&mut live).await, b"later");
    f.node.close().await.unwrap();
}

#[tokio::test]
async fn stop_by_driver_id_keeps_scheduler_usable() {
    let f = Fixture::new().await;
    let rows = f.actor.sql("SELECT sender FROM inbox WHERE sender LIKE 'drv:%' LIMIT 1", ()).await.unwrap();
    let driver: String = rows.rows[0].get(0).unwrap();
    let _control = TcpStream::connect(&f.addr).await.unwrap();
    f.node.stop(&driver, "normal").await.unwrap();
    // A driver id must not enter the actor wake set: the next drain still succeeds.
    f.node.send(&f.owner, "after-stop", b"noop").await.unwrap();
    f.node.run_until_idle().await.unwrap();
    assert!(TcpStream::connect(&f.addr).await.is_err());
    f.node.close().await.unwrap();
}

#[tokio::test]
async fn owner_stop_closes_driver() {

    let f = Fixture::new().await;
    let _control = TcpStream::connect(&f.addr).await.unwrap();
    f.node.stop(&f.owner, "normal").await.unwrap();
    assert!(TcpStream::connect(&f.addr).await.is_err());
    f.node.close().await.unwrap();
}

struct Missing;
#[async_trait]
impl Behavior for Missing {
    fn hash(&self) -> &str {
        "missing-driver-owner"
    }
    fn schema(&self) -> &str {
        ""
    }
    async fn handle(&self, cx: &mut Ctx<'_>, _: &[u8]) -> Result<(), Trap> {
        let _ignored = cx.spawn_driver("missing-driver-hash", b"{}").await;
        Ok(())
    }
}
#[tokio::test]
async fn unknown_hash_traps_even_if_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::new(
        dir.path(),
        common::registry(vec![Arc::new(Missing)]),
        Arc::new(DefaultEffects),
        Config { io: Io::Memory, ..Config::default() },
    )
    .await
    .unwrap();
    let owner = node.spawn_root("missing-driver-owner", b"start").await.unwrap();
    node.run_until_idle().await.unwrap();
    let actor = node.open(&owner).await.unwrap();
    assert_eq!(integer(&actor, "SELECT COUNT(*) FROM dead_letters WHERE error LIKE '%missing-driver-hash%'").await, 1);
    assert_eq!(integer(&actor, "SELECT COUNT(*) FROM outbox WHERE target LIKE 'drv:%'").await, 0);
    node.close().await.unwrap();
}

struct Failing;
#[async_trait]
impl Driver for Failing {
    fn hash(&self) -> &str {
        "failing-driver"
    }
    async fn run(&self, cx: DriverContext, _: &[u8], _: mpsc::Receiver<DriverDelivery>) -> anyhow::Result<()> {
        cx.inject(cx.owner(), "same-resource-frame", b"frame").await?;
        cx.inject(cx.owner(), "same-resource-frame", b"frame").await?;
        anyhow::bail!("resource failed")
    }
}
struct Reopen;
#[async_trait]
impl Behavior for Reopen {
    fn hash(&self) -> &str {
        "reopen-owner"
    }
    fn schema(&self) -> &str {
        "CREATE TABLE IF NOT EXISTS attempts(n INTEGER);"
    }
    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        let down = serde_json::from_slice::<serde_json::Value>(msg).is_ok_and(|v| v["type"] == "down");
        if msg == b"start" || down {
            let rows = cx.sql("SELECT COUNT(*) FROM attempts", ()).await?;
            if rows.rows[0].get::<i64>(0).unwrap() < 2 {
                cx.sql("INSERT INTO attempts VALUES (1)", ()).await?;
                cx.spawn_driver("failing-driver", b"{}").await?;
            }
        }
        Ok(())
    }
}
#[tokio::test]
async fn failure_down_reopens_from_owner_state_and_keys_survive() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = registry::Registry::new();
    registry.insert("reopen-owner".into(), Arc::new(Reopen));
    registry.insert_driver(Arc::new(Failing));
    let node =
        Node::new(dir.path(), Arc::new(registry), Arc::new(DefaultEffects), Config { io: Io::Memory, ..Config::default() }).await.unwrap();
    let owner = node.spawn_root("reopen-owner", b"start").await.unwrap();
    let actor = node.open(&owner).await.unwrap();
    for _ in 0..2 {
        node.run_until_idle().await.unwrap();
        let count = integer(&actor, "SELECT COUNT(*) FROM attempts").await;
        wait_rows(&actor, &format!("SELECT key FROM inbox WHERE key LIKE 'down:spawn:drv:%' LIMIT 1 OFFSET {}", count - 1)).await;
    }
    node.run_until_idle().await.unwrap();
    assert_eq!(integer(&actor, "SELECT COUNT(*) FROM attempts").await, 2);
    assert_eq!(integer(&actor, "SELECT COUNT(*) FROM inbox WHERE key='same-resource-frame'").await, 1);
    assert_eq!(integer(&actor, "SELECT COUNT(*) FROM inbox WHERE key LIKE 'down:spawn:drv:%'").await, 2);
    node.close().await.unwrap();
}

#[tokio::test]
async fn handle_caps_attenuate_and_revoke_through_owner_authority() {
    let f = Fixture::new().await;
    let _socket = f.frame(b"hello").await;
    let cap = f.cap(&f.first_key().await).await;
    f.node.check_cap(&cap, loom_actor::Rights::SEND, "before revoke").await.unwrap();
    f.node.send(&f.owner, "revoke-handle", &serde_json::to_vec(&serde_json::json!({"type":"revoke","cap":cap})).unwrap()).await.unwrap();
    f.node.run_until_idle().await.unwrap();
    assert!(f.node.check_cap(&cap, loom_actor::Rights::SEND, "after revoke").await.is_err());
    let rows = f.actor.sql("SELECT cap FROM driver_state ORDER BY rowid DESC LIMIT 1", ()).await.unwrap();
    let attenuated: Cap = serde_json::from_str(&rows.rows[0].get::<String>(0).unwrap()).unwrap();
    assert_eq!(attenuated.rights, loom_actor::Rights::NONE);
    assert!(f.node.check_cap(&attenuated, loom_actor::Rights::SEND, "attenuated send").await.is_err());
    f.node.close().await.unwrap();
}

#[tokio::test]
async fn node_restart_does_not_restore_attempted_driver_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = registry::Registry::new();
    registry.insert(OWNER.into(), Arc::new(Owner { trap: false, hash: OWNER }));
    registry.insert_driver(Arc::new(loom_actor::drivers::tcp::TcpListenerDriver));
    let registry = Arc::new(registry);
    let config = Config { io: Io::Syscall, ..Config::default() };
    let node = Node::new(dir.path(), registry.clone(), Arc::new(DefaultEffects), config.clone()).await.unwrap();
    let owner = node.spawn_root(OWNER, b"start").await.unwrap();
    node.run_until_idle().await.unwrap();
    let actor = node.open(&owner).await.unwrap();
    wait_rows(&actor, "SELECT msg FROM inbox WHERE key LIKE 'driver:%:listening'").await;
    let rows = actor.sql("SELECT msg FROM inbox WHERE key LIKE 'driver:%:listening'", ()).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&rows.rows[0].get::<Vec<u8>>(0).unwrap()).unwrap();
    let addr = value["addr"].as_str().unwrap().to_owned();
    let mut control = TcpStream::connect(&addr).await.unwrap();
    write_frame(&mut control, b"reply").await;
    wait_rows(&actor, "SELECT key FROM inbox WHERE key LIKE 'conn:%:1'").await;
    node.run_until_idle().await.unwrap();
    assert_eq!(read_frame(&mut control).await, b"committed");
    assert!(matches!(node.validate(&owner, OWNER, 1).await.unwrap(), loom_actor::Verdict::Matched { .. }));
    no_frame(&mut control).await; // Replay cannot write a second socket frame.
    drop(control);
    // Crash window: resource opened, pump's final delivered bit not persisted.
    actor.sql("UPDATE outbox SET delivered=0 WHERE target LIKE 'drv:spawn:%'", ()).await.unwrap();
    node.close().await.unwrap();
    drop(actor);
    drop(node);
    let reopened = Node::new(dir.path(), registry, Arc::new(DefaultEffects), config).await.unwrap();
    reopened.pump(&owner).await.unwrap();
    let actor = reopened.open(&owner).await.unwrap();
    assert_eq!(integer(&actor, "SELECT COUNT(*) FROM inbox WHERE key LIKE 'driver:%:listening'").await, 1);
    assert_eq!(integer(&actor, "SELECT COUNT(*) FROM outbox WHERE delivered=0 AND target LIKE 'drv:spawn:%'").await, 0);
    assert!(TcpStream::connect(&addr).await.is_err());
    reopened.close().await.unwrap();
}
