mod registry;

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use loom_actor::{Actor, Behavior, Cap, Config, Ctx, DefaultEffects, Node, Rights, Status, Trap, Verdict};
use serde_json::{Value, json};

struct Probe;

#[async_trait]
impl Behavior for Probe {
    fn hash(&self) -> &str {
        "named-probe"
    }

    fn schema(&self) -> &str {
        "CREATE TABLE received(msg BLOB); CREATE TABLE saved(cap BLOB)"
    }

    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        let value: Value = serde_json::from_slice(msg).map_err(|error| Trap::new(error.to_string()))?;
        match value["op"].as_str().unwrap_or("record") {
            "record" => {
                cx.sql("INSERT INTO received VALUES (?)", [msg]).await?;
            }
            "lookup" => {
                let cap = cx.resolve_name(value["name"].as_str().unwrap()).await?;
                cx.sql("INSERT INTO saved VALUES (?)", [serde_json::to_vec(&cap).unwrap()]).await?;
                // Exercise the durable capability table, not just the returned value.
                let stored = cx.cap(cap.cap_id).await?;
                if stored != cap {
                    return Err(Trap::new("stored capability differs from named lookup"));
                }
                cx.send(&stored, b"{\"resolved\":true}").await?;
            }
            "send" | "stop" => {
                let cap: Cap = serde_json::from_value(value["cap"].clone()).map_err(|error| Trap::new(error.to_string()))?;
                if value["op"] == "stop" {
                    cx.stop(&cap, "must be denied").await?;
                } else {
                    cx.send(&cap, b"{\"foreign\":true}").await?;
                }
            }
            other => return Err(Trap::new(format!("unknown operation {other}"))),
        }
        Ok(())
    }
}

async fn node(path: &std::path::Path) -> Node {
    let mut registry = registry::Registry::new();
    registry.insert("named-probe".into(), Arc::new(Probe));
    Node::new(path, Arc::new(registry), Arc::new(DefaultEffects), Config::default()).await.unwrap()
}

async fn drain(node: &Node) {
    tokio::time::timeout(Duration::from_secs(60), node.run_until_idle()).await.unwrap().unwrap();
}

async fn spawn(node: &Node) -> String {
    let id = node.spawn_root("named-probe", b"{}").await.unwrap();
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

async fn rejection(node: &Node, sender: &str) -> String {
    let actor = node.open(sender).await.unwrap();
    assert_eq!(actor.status().await.unwrap(), Status::Parked);
    assert_eq!(count(&actor, "saved").await, 0);
    let errors = actor.sql("SELECT error,seq FROM dead_letters", ()).await.unwrap();
    assert_eq!(errors.rows.len(), 1);
    let error: String = errors.rows[0].get(0).unwrap();
    let failed_seq: i64 = errors.rows[0].get(1).unwrap();

    // spawn_root creates a child of the node supervisor. actor::poison rolls
    // back guest work, then commits a poison notification to that parent.
    // Require exactly that notification, so any leaked guest send still fails.
    let outbox = actor.sql("SELECT seq,target,msg,delivered FROM outbox", ()).await.unwrap();
    assert_eq!(outbox.rows.len(), 1);
    let notification = &outbox.rows[0];
    assert_eq!(notification.get::<i64>(0).unwrap(), failed_seq);
    assert_eq!(notification.get::<String>(1).unwrap(), node.root());
    assert_eq!(notification.get::<i64>(3).unwrap(), 1);
    let payload: Value = serde_json::from_slice(&notification.get::<Vec<u8>>(2).unwrap()).unwrap();
    assert_eq!(payload["type"], "poison");
    assert_eq!(payload["reason"], "poison");
    assert_eq!(payload["from"], sender);
    assert_eq!(payload["child"], sender);
    assert_eq!(payload["seq"], failed_seq);
    assert_eq!(payload["error"], error);
    error
}

#[tokio::test]
async fn lookup_stores_send_only_capability_and_delivers() {
    let dir = tempfile::tempdir().unwrap();
    let node = node(dir.path()).await;
    let target = spawn(&node).await;
    let sender = spawn(&node).await;
    node.register("service", &target).await.unwrap();
    command(&node, &sender, "lookup", json!({"op":"lookup","name":"service"})).await;

    let actor = node.open(&sender).await.unwrap();
    assert_eq!(actor.status().await.unwrap(), Status::Running);
    let cap = saved(&actor).await;
    assert_eq!(cap.target, target);
    assert_eq!(cap.rights, Rights::SEND);
    assert_eq!(count(&node.open(&target).await.unwrap(), "received").await, 2);

    let stopper = spawn(&node).await;
    command(&node, &stopper, "stop", json!({"op":"stop","cap":cap})).await;
    assert!(rejection(&node, &stopper).await.contains("stop"));
    assert_eq!(node.open(&target).await.unwrap().status().await.unwrap(), Status::Running);
}

#[tokio::test]
async fn missing_name_is_a_repeatable_trap_without_guest_outbox() {
    let dir = tempfile::tempdir().unwrap();
    let node = node(dir.path()).await;
    let first = spawn(&node).await;
    let second = spawn(&node).await;
    command(&node, &first, "missing", json!({"op":"lookup","name":"missing-service"})).await;
    command(&node, &second, "missing", json!({"op":"lookup","name":"missing-service"})).await;
    let first_error = rejection(&node, &first).await;
    let second_error = rejection(&node, &second).await;
    assert!(first_error.contains("missing-service"), "{first_error}");
    // Runtime diagnostics can identify the caller; the lookup refusal must agree.
    assert_eq!(first_error.replace(&first, "<caller>"), second_error.replace(&second, "<caller>"));
}

#[tokio::test]
async fn stopped_names_and_unpublished_actor_ids_do_not_grant_authority() {
    let dir = tempfile::tempdir().unwrap();
    let node = node(dir.path()).await;
    let target = spawn(&node).await;
    let literal = spawn(&node).await;
    command(&node, &literal, "literal", json!({"op":"lookup","name":target})).await;
    assert!(rejection(&node, &literal).await.contains(&target));
    assert_eq!(count(&node.open(&target).await.unwrap(), "received").await, 1);

    node.register("stopped-service", &target).await.unwrap();
    node.stop(&target, "test").await.unwrap();
    drain(&node).await;
    let sender = spawn(&node).await;
    command(&node, &sender, "stopped", json!({"op":"lookup","name":"stopped-service"})).await;
    assert!(rejection(&node, &sender).await.contains("stopped-service"));
}

#[tokio::test]
async fn names_and_capabilities_are_scoped_to_node_authority() {
    let first_dir = tempfile::tempdir().unwrap();
    let second_dir = tempfile::tempdir().unwrap();
    let first = node(first_dir.path()).await;
    let second = node(second_dir.path()).await;
    let first_target = spawn(&first).await;
    let second_target = spawn(&second).await;
    let first_sender = spawn(&first).await;
    let second_sender = spawn(&second).await;
    first.register("service", &first_target).await.unwrap();
    second.register("service", &second_target).await.unwrap();
    command(&first, &first_sender, "lookup", json!({"op":"lookup","name":"service"})).await;
    command(&second, &second_sender, "lookup", json!({"op":"lookup","name":"service"})).await;
    let first_cap = saved(&first.open(&first_sender).await.unwrap()).await;
    let second_cap = saved(&second.open(&second_sender).await.unwrap()).await;
    assert_eq!(first_cap.target, first_target);
    assert_eq!(second_cap.target, second_target);

    let foreign_sender = spawn(&second).await;
    command(&second, &foreign_sender, "foreign", json!({"op":"send","cap":first_cap})).await;
    assert!(rejection(&second, &foreign_sender).await.contains("send"));
    assert_eq!(count(&first.open(&first_target).await.unwrap(), "received").await, 2);
    assert_eq!(count(&second.open(&second_target).await.unwrap(), "received").await, 2);
}

#[tokio::test]
async fn replay_keeps_recorded_capability_after_directory_changes() {
    let dir = tempfile::tempdir().unwrap();
    let node = node(dir.path()).await;
    let target = spawn(&node).await;
    let replacement = spawn(&node).await;
    let sender = spawn(&node).await;
    node.register("service", &target).await.unwrap();
    command(&node, &sender, "lookup", json!({"op":"lookup","name":"service"})).await;
    let actor = node.open(&sender).await.unwrap();
    let before = actor.sql("SELECT * FROM effects ORDER BY seq,idx", ()).await.unwrap().rows;
    let saved_before = actor.sql("SELECT cap FROM saved", ()).await.unwrap().rows;
    let history = actor.cursor().await.unwrap() - 1;

    node.unregister("service").await.unwrap();
    let absent = node.validate(&sender, "named-probe", history).await.unwrap();
    assert!(matches!(absent, Verdict::Matched { .. }), "{absent:?}");
    node.register("service", &replacement).await.unwrap();
    let changed = node.validate(&sender, "named-probe", history).await.unwrap();
    assert!(matches!(changed, Verdict::Matched { .. }), "{changed:?}");
    assert_eq!(actor.sql("SELECT * FROM effects ORDER BY seq,idx", ()).await.unwrap().rows, before);
    assert_eq!(actor.sql("SELECT cap FROM saved", ()).await.unwrap().rows, saved_before);
    assert_eq!(count(&node.open(&target).await.unwrap(), "received").await, 2);
    assert_eq!(count(&node.open(&replacement).await.unwrap(), "received").await, 1);
}

#[tokio::test]
async fn self_lookup_uses_current_transaction_without_relocking() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = registry::Registry::new();
    registry.insert("named-probe".into(), Arc::new(Probe));
    let node =
        Node::new(dir.path(), Arc::new(registry), Arc::new(DefaultEffects), Config { io: loom_actor::Io::Memory, ..Config::default() })
            .await
            .unwrap();
    let sender = spawn(&node).await;
    node.register("self", &sender).await.unwrap();

    // Reopening an in-memory database loses its state; locking the actor again
    // deadlocks. Self resolution must use the invocation's existing connection.
    tokio::time::timeout(Duration::from_secs(10), command(&node, &sender, "self", json!({"op":"lookup","name":"self"})))
        .await
        .expect("named self lookup must complete without reacquiring the actor lock");
    let actor = node.open(&sender).await.unwrap();
    assert_eq!(actor.status().await.unwrap(), Status::Running);
    let cap = saved(&actor).await;
    assert_eq!(cap.target, sender);
    assert_eq!(cap.rights, Rights::SEND);
    assert_eq!(count(&actor, "received").await, 2);
}
