use crate::registry::Registry;
mod registry;
use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use loom_actor::{
    Actor, Behavior, Cap, ChildSpec, ChildType, Config, Ctx, DefaultEffects, Node, RestartVerb, Rights, Status, Trap, Verdict,
};
use serde_json::{Value, json};

struct Probe;
#[async_trait]
impl Behavior for Probe {
    fn hash(&self) -> &str {
        "caps-probe"
    }
    fn schema(&self) -> &str {
        "CREATE TABLE received(msg BLOB); CREATE TABLE saved(cap BLOB)"
    }
    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        let value: Value = serde_json::from_slice(msg).map_err(|e| Trap::new(e.to_string()))?;
        let op = value["op"].as_str().unwrap_or("record");
        if op == "record" {
            cx.sql("INSERT INTO received VALUES (?)", [msg]).await?;
            return Ok(());
        }
        if op == "sql_escape" {
            let sql = value["sql"].as_str().unwrap();
            let _ignored = cx.sql(sql, ()).await;
            cx.sql("INSERT INTO received VALUES ('ignored-error')", ()).await?;
            return Ok(());
        }
        if op == "spawn" {
            let mut spec = ChildSpec::new("caps-probe", b"{}", ChildType::Worker);
            spec.link = false;
            let cap = cx.spawn(&spec).await?;
            cx.sql("INSERT INTO saved VALUES (?)", [serde_json::to_vec(&cap).unwrap()]).await?;
            return cx.send(&cap, b"{\"minted\":true}").await;
        }
        let cap: Cap = if op == "stored" {
            cx.cap(value["cap_id"].as_u64().unwrap()).await?
        } else {
            serde_json::from_value(value["cap"].clone()).map_err(|e| Trap::new(e.to_string()))?
        };
        match op {
            "send" | "stored" => cx.send(&cap, b"{\"delivered\":true}").await?,
            "stop" => cx.stop(&cap, "test").await?,
            "promote" => cx.promote(&cap, "caps-probe", "test", "cap replay").await?,
            "attenuate" => {
                let reduced = cx.attenuate(&cap, Rights::SEND).await?;
                cx.sql("INSERT INTO saved VALUES (?)", [serde_json::to_vec(&reduced).unwrap()]).await?;
                cx.send(&reduced, b"{\"attenuated\":true}").await?;
            }
            "accept" => {
                cx.accept(cap.clone()).await?;
                cx.send(&cap, b"{\"delegated\":true}").await?;
            }
            "delegate" => cx.send(&cap, &serde_json::to_vec(&json!({"op":"accept","cap":value["delegated"]})).unwrap()).await?,
            "revoke_send" => {
                cx.accept(cap.clone()).await?;
                cx.revoke(cap.cap_id).await?;
                cx.send(&cap, b"{\"revoked\":true}").await?;
            }
            "revoke" => {
                cx.accept(cap.clone()).await?;
                cx.revoke(cap.cap_id).await?;
            }
            "monitor" => {
                cx.monitor(&cap).await?;
            }
            "link" => cx.link(&cap).await?,
            "unlink" => cx.unlink(&cap).await?,
            "shutdown" => cx.shutdown(&cap).await?,
            "restart" => cx.restart(&cap, RestartVerb::Resume).await?,
            "inspect" => {
                cx.inspect(&cap).await?;
            }
            "inspect_sql" => {
                cx.inspect_sql(&cap, "SELECT count(*) FROM received", Vec::new()).await?;
            }
            "inspect_write" => {
                cx.inspect_sql(&cap, "DELETE FROM received", Vec::new()).await?;
            }
            "call" => {
                cx.call(&cap, b"{}", 1).await?;
            }
            "reply" => cx.reply(&cap, "test-reference", b"{}").await?,
            "send_after" => {
                cx.send_after(&cap, 1, b"{}").await?;
            }
            other => return Err(Trap::new(format!("unknown operation {other}"))),
        }
        Ok(())
    }
}

async fn node(path: &std::path::Path) -> Node {
    let mut registry = Registry::new();
    registry.insert("caps-probe".into(), Arc::new(Probe));
    Node::new(path, Arc::new(registry), Arc::new(DefaultEffects), Config::default()).await.unwrap()
}
async fn drain(node: &Node) {
    tokio::time::timeout(Duration::from_secs(60), node.run_until_idle()).await.unwrap().unwrap();
}
async fn spawn(node: &Node) -> String {
    let id = node.spawn_root("caps-probe", b"{}").await.unwrap();
    drain(node).await;
    id
}
async fn command(node: &Node, id: &str, key: &str, value: Value) {
    node.send(id, key, &serde_json::to_vec(&value).unwrap()).await.unwrap();
    drain(node).await;
}
async fn count(actor: &Actor, table: &str) -> i64 {
    actor.sql(&format!("SELECT count(*) FROM {table}"), ()).await.unwrap().rows[0].get(0).unwrap()
}
async fn saved(actor: &Actor) -> Cap {
    let rows = actor.sql("SELECT cap FROM saved ORDER BY rowid DESC LIMIT 1", ()).await.unwrap();
    serde_json::from_slice(&rows.rows[0].get::<Vec<u8>>(0).unwrap()).unwrap()
}
async fn rejects(node: &Node, cap: &Cap, op: &str) {
    let sender = spawn(node).await;
    command(node, &sender, "reject", json!({"op":op,"cap":cap})).await;
    let actor = node.open(&sender).await.unwrap();
    assert_eq!(actor.status().await.unwrap(), Status::Parked);
    let rows = actor.sql("SELECT error FROM dead_letters", ()).await.unwrap();
    assert_eq!(rows.rows.len(), 1);
    let error: String = rows.rows[0].get(0).unwrap();
    assert!(error.contains(op) && error.contains(&cap.cap_id.to_string()), "{error}");
    let outgoing = actor.sql("SELECT count(*) FROM outbox WHERE target=?", [cap.target.as_str()]).await.unwrap();
    assert_eq!(outgoing.rows[0].get::<i64>(0).unwrap(), 0);
}

#[tokio::test]
async fn spawn_returns_cap_and_send_needs_it() {
    let dir = tempfile::tempdir().unwrap();
    let node = node(dir.path()).await;
    let parent = spawn(&node).await;
    command(&node, &parent, "spawn", json!({"op":"spawn"})).await;
    let cap = saved(&node.open(&parent).await.unwrap()).await;
    assert_eq!(cap.rights, Rights::ALL);
    let target = node.open(&cap.target).await.unwrap();
    assert_eq!(count(&target, "received").await, 2);
    let mut forged = cap.clone();
    forged.mac = [0x97; 32];
    rejects(&node, &forged, "send").await;
    assert_eq!(count(&target, "received").await, 2);
    for sql in [
        "DELETE FROM caps",
        "INSERT INTO outbox(seq,idx,target,msg) VALUES (9,9,'victim',x'00')",
        "ATTACH ':memory:' AS attached_db",
        "SELECT load_extension('escape')",
        "SELECT \"load_extension\"('escape')",
    ] {
        let sender = spawn(&node).await;
        command(&node, &sender, "sql-escape", json!({"op":"sql_escape","sql":sql})).await;
        let actor = node.open(&sender).await.unwrap();
        assert_eq!(actor.status().await.unwrap(), Status::Parked, "{sql}");
        let error: String = actor.sql("SELECT error FROM dead_letters", ()).await.unwrap().rows[0].get(0).unwrap();
        assert!(error.contains("guest SQL"), "guard must reject {sql}: {error}");
        assert_eq!(count(&actor, "received").await, 1, "caught SQL refusal must roll back: {sql}");
    }
    let safe = spawn(&node).await;
    command(&node, &safe, "sql-literal", json!({"op":"sql_escape","sql":"SELECT 'outbox caps revoked'"})).await;
    let actor = node.open(&safe).await.unwrap();
    assert_eq!(actor.status().await.unwrap(), Status::Running);
    assert_eq!(count(&actor, "received").await, 2);
}

#[tokio::test]
async fn attenuated_cap_cannot_stop() {
    let dir = tempfile::tempdir().unwrap();
    let node = node(dir.path()).await;
    let target = spawn(&node).await;
    let sender = spawn(&node).await;
    let full = node.cap_for(&target, Rights::ALL).await.unwrap();
    command(&node, &sender, "attenuate", json!({"op":"attenuate","cap":full})).await;
    let cap = saved(&node.open(&sender).await.unwrap()).await;
    assert_eq!(cap.rights, Rights::SEND);
    rejects(&node, &cap, "stop").await;
    rejects(&node, &cap, "inspect_sql").await;
    let reader = node.cap_for(&target, Rights::INSPECT).await.unwrap();
    let writer = spawn(&node).await;
    command(&node, &writer, "inspect-write", json!({"op":"inspect_write","cap":reader})).await;
    let writer = node.open(&writer).await.unwrap();
    assert_eq!(writer.status().await.unwrap(), Status::Parked);
    let error: String = writer.sql("SELECT error FROM dead_letters", ()).await.unwrap().rows[0].get(0).unwrap();
    assert!(error.contains("inspect_sql"), "{error}");
    let actor = node.open(&target).await.unwrap();
    assert_eq!(actor.status().await.unwrap(), Status::Running);
    assert_eq!(count(&actor, "received").await, 2);
}

#[tokio::test]
async fn delegated_cap_works_after_accept() {
    let dir = tempfile::tempdir().unwrap();
    let node = node(dir.path()).await;
    let a = spawn(&node).await;
    let b = spawn(&node).await;
    let c = spawn(&node).await;
    let b_cap = node.cap_for(&b, Rights::SEND).await.unwrap();
    let c_cap = node.cap_for(&c, Rights::SEND).await.unwrap();
    command(&node, &a, "delegate", json!({"op":"delegate","cap":b_cap,"delegated":c_cap})).await;
    assert_eq!(count(&node.open(&c).await.unwrap(), "received").await, 2);
    let accepted = node.open(&b).await.unwrap().sql("SELECT target FROM caps WHERE cap_id=?", [c_cap.cap_id as i64]).await.unwrap();
    assert_eq!(accepted.rows[0].get::<String>(0).unwrap(), c);
}

#[tokio::test]
async fn revoke_one_and_bump_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let node = node(dir.path()).await;
    let target = spawn(&node).await;
    let sender = spawn(&node).await;
    let first = node.cap_for(&target, Rights::ALL).await.unwrap();
    let second = node.cap_for(&target, Rights::ALL).await.unwrap();
    let rollback = spawn(&node).await;
    command(&node, &rollback, "revoke-send", json!({"op":"revoke_send","cap":first})).await;
    let rollback = node.open(&rollback).await.unwrap();
    assert_eq!(rollback.status().await.unwrap(), Status::Parked);
    let error: String = rollback.sql("SELECT error FROM dead_letters", ()).await.unwrap().rows[0].get(0).unwrap();
    assert!(error.contains("send") && error.contains(&first.cap_id.to_string()), "{error}");
    assert_eq!(count(&node.open(&target).await.unwrap(), "revoked").await, 0);
    command(&node, &sender, "rollback-live", json!({"op":"send","cap":first})).await;
    assert_eq!(count(&node.open(&target).await.unwrap(), "received").await, 2);
    command(&node, &sender, "revoke", json!({"op":"revoke","cap":first})).await;
    rejects(&node, &first, "send").await;
    node.restart(&target, RestartVerb::Reset).await.unwrap();
    drain(&node).await;
    assert_eq!(count(&node.open(&target).await.unwrap(), "received").await, 1);
    assert_eq!(count(&node.open(&target).await.unwrap(), "revoked").await, 1);
    rejects(&node, &first, "send").await;
    command(&node, &sender, "second", json!({"op":"send","cap":second})).await;
    assert_eq!(count(&node.open(&target).await.unwrap(), "received").await, 2);
    node.bump_epoch(&target).await.unwrap();
    node.restart(&target, RestartVerb::Reset).await.unwrap();
    drain(&node).await;
    assert_eq!(count(&node.open(&target).await.unwrap(), "received").await, 1);
    rejects(&node, &first, "send").await;
    rejects(&node, &second, "send").await;
    let fresh = node.cap_for(&target, Rights::SEND).await.unwrap();
    command(&node, &sender, "fresh", json!({"op":"send","cap":fresh})).await;
    assert_eq!(count(&node.open(&target).await.unwrap(), "received").await, 2);
}

#[tokio::test]
async fn caps_survive_reset_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let original = node(dir.path()).await;
    let target = spawn(&original).await;
    let sender = spawn(&original).await;
    let cap = original.cap_for(&target, Rights::SEND).await.unwrap();
    command(&original, &sender, "accept", json!({"op":"accept","cap":cap})).await;
    original.restart(&sender, RestartVerb::Reset).await.unwrap();
    drain(&original).await;
    command(&original, &sender, "reset-send", json!({"op":"stored","cap_id":cap.cap_id})).await;
    assert_eq!(count(&original.open(&target).await.unwrap(), "received").await, 3);
    drop(original);
    let reopened = node(dir.path()).await;
    command(&reopened, &sender, "restart-send", json!({"op":"stored","cap_id":cap.cap_id})).await;
    assert_eq!(count(&reopened.open(&target).await.unwrap(), "received").await, 4);
}

#[tokio::test]
async fn fork_cannot_use_caps() {
    let dir = tempfile::tempdir().unwrap();
    let node = node(dir.path()).await;
    let sender = spawn(&node).await;
    let operations = [
        "send",
        "attenuate",
        "accept",
        "monitor",
        "link",
        "unlink",
        "inspect",
        "inspect_sql",
        "call",
        "reply",
        "send_after",
        "stop",
        "shutdown",
        "restart",
        "promote",
        "revoke",
    ];
    let mut targets = Vec::new();
    for op in operations {
        let target = spawn(&node).await;
        let cap = node.cap_for(&target, Rights::ALL).await.unwrap();
        command(&node, &sender, op, json!({"op":op,"cap":cap})).await;
        targets.push(target);
    }
    let actor = node.open(&sender).await.unwrap();
    assert_eq!(actor.status().await.unwrap(), Status::Running);
    let effects = actor.sql("SELECT request FROM effects WHERE kind='__cap'", ()).await.unwrap();
    let captured: Vec<Value> = effects.rows.iter().map(|row| serde_json::from_slice(&row.get::<Vec<u8>>(0).unwrap()).unwrap()).collect();
    for operation in operations {
        assert!(
            captured.iter().any(|request| request["name"] == operation
                || request["operation"].as_str().is_some_and(|name| name.eq_ignore_ascii_case(&operation.replace('_', "")))),
            "missing captured {operation}: {captured:?}"
        );
    }
    let mut before = Vec::new();
    for target in &targets {
        node.bump_epoch(target).await.unwrap();
        let actor = node.open(target).await.unwrap();
        before.push(actor.sql("SELECT * FROM inbox ORDER BY seq", ()).await.unwrap().rows);
    }
    let history = actor.cursor().await.unwrap() - 1;
    let verdict = node.validate(&sender, "caps-probe", history).await.unwrap();
    assert!(matches!(verdict, Verdict::Matched { .. }), "{verdict:?}");
    for (index, target) in targets.iter().enumerate() {
        let actor = node.open(target).await.unwrap();
        assert_eq!(actor.sql("SELECT * FROM inbox ORDER BY seq", ()).await.unwrap().rows, before[index]);
    }
}

struct HeldWriter {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}
#[async_trait]
impl Behavior for HeldWriter {
    fn hash(&self) -> &str {
        "caps-held-writer"
    }
    fn schema(&self) -> &str {
        "CREATE TABLE counter(value INTEGER); INSERT INTO counter VALUES (1)"
    }
    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        if msg == b"hold" {
            cx.sql("UPDATE counter SET value=2", ()).await?;
            // Notification follows the write, so the test cannot mistake an
            // idle connection for an independently readable WAL snapshot.
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(())
    }
}
struct ConcurrentInspector {
    observed: tokio::sync::mpsc::UnboundedSender<i64>,
}
#[async_trait]
impl Behavior for ConcurrentInspector {
    fn hash(&self) -> &str {
        "caps-concurrent-inspector"
    }
    fn schema(&self) -> &str {
        ""
    }
    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        if msg.is_empty() {
            return Ok(());
        }
        let cap: Cap = serde_json::from_slice(msg).map_err(|error| Trap::new(error.to_string()))?;
        cx.accept(cap.clone()).await?;
        let result = cx.inspect_sql(&cap, "SELECT value FROM counter", vec![]).await?;
        let loom_actor::SqlValue::Integer(value) = &result.rows[0].values[0] else {
            return Err(Trap::new("expected integer counter"));
        };
        self.observed.send(*value).map_err(|error| Trap::new(error.to_string()))?;
        Ok(())
    }
}

#[tokio::test]
async fn capability_inspection_reads_committed_authority_during_writer_transaction() {
    let directory = tempfile::tempdir().unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (observed, mut observations) = tokio::sync::mpsc::unbounded_channel();
    let mut registry = Registry::new();
    registry.insert("caps-held-writer".into(), Arc::new(HeldWriter { entered: entered.clone(), release: release.clone() }));
    registry.insert("caps-concurrent-inspector".into(), Arc::new(ConcurrentInspector { observed }));
    let node = Node::new(
        directory.path(),
        Arc::new(registry),
        Arc::new(DefaultEffects),
        Config { io: loom_actor::Io::Syscall, ..Config::default() },
    )
    .await
    .unwrap();
    let writer = node.spawn_root("caps-held-writer", b"").await.unwrap();
    let inspector = node.spawn_root("caps-concurrent-inspector", b"").await.unwrap();
    drain(&node).await;
    let cap = node.cap_for(&writer, Rights::INSPECT).await.unwrap();
    node.check_cap(&cap, Rights::INSPECT, "before-writer").await.unwrap();
    node.send(&writer, "held-write", b"hold").await.unwrap();
    let running_node = node.clone();
    let running = tokio::spawn(async move { running_node.run_until_idle().await });
    tokio::time::timeout(Duration::from_secs(5), entered.notified()).await.expect("writer did not acquire transaction");

    let authority = tokio::time::timeout(Duration::from_secs(2), node.check_cap(&cap, Rights::INSPECT, "concurrent-check")).await;
    node.send(&inspector, "read-before-commit", &serde_json::to_vec(&cap).unwrap()).await.unwrap();
    let before = tokio::time::timeout(Duration::from_secs(2), observations.recv()).await;
    // Always release the writer before asserting, including the old connector's
    // database-locked error path, so a failed regression cannot strand the task.
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), running).await.unwrap().unwrap().unwrap();
    authority.expect("authority read waited for writer commit").expect("authority read tried to mutate the locked actor database");
    assert_eq!(before.expect("cross-actor inspection waited for writer commit"), Some(1));
    node.send(&inspector, "read-after-commit", &serde_json::to_vec(&cap).unwrap()).await.unwrap();
    drain(&node).await;
    assert_eq!(tokio::time::timeout(Duration::from_secs(2), observations.recv()).await.unwrap(), Some(2));
    node.close().await.unwrap();
}

/// Delivery dedup across a reset, a question raised by `verify/outbox`: `reset.rs` carries
/// `applied:` receipts and cap tables into the new incarnation but not the inbox, whose
/// unique `key` is what deduplicates deliveries. So a sender that redelivers a key after
/// the receiver reset (it crashed before marking the row delivered) gets it applied again.
#[tokio::test]
async fn delivery_key_is_forgotten_by_reset() {
    let dir = tempfile::tempdir().unwrap();
    let node = node(dir.path()).await;
    let target = spawn(&node).await;
    let before = count(&node.open(&target).await.unwrap(), "received").await;
    command(&node, &target, "k1", json!({"op":"record"})).await;
    command(&node, &target, "k1", json!({"op":"record"})).await;
    assert_eq!(count(&node.open(&target).await.unwrap(), "received").await, before + 1, "same key is deduplicated within an incarnation");
    node.restart(&target, RestartVerb::Reset).await.unwrap();
    drain(&node).await;
    let after_reset = count(&node.open(&target).await.unwrap(), "received").await;
    command(&node, &target, "k1", json!({"op":"record"})).await;
    let redelivered = count(&node.open(&target).await.unwrap(), "received").await - after_reset;
    println!("REDELIVERED-AFTER-RESET {redelivered}");
    assert_eq!(redelivered, 1, "the reset incarnation applies a key the previous one already applied");
}
