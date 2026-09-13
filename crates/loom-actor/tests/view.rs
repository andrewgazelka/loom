mod view_support;
use loom_actor::{ChildSpec, ChildType, Config, Durability, Rights, Status, StoreConfig, Verdict};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use view_support::*;

#[tokio::test]
async fn cdc_replay_reproduces_tables() {
    let dir = tempfile::tempdir().unwrap();
    let node_path = dir.path().join("node");
    let node = node(&node_path, Config { snapshot_every: 100, ..Config::default() }).await;
    let id = spawn(&node, "counter-v1", b"zero", Durability::Local).await;
    let actor = node.open(&id).await.unwrap();
    let path: String = actor.sql("SELECT path FROM snapshots WHERE seq=0", ()).await.unwrap().rows[0].get(0).unwrap();
    let good_path = dir.path().join("replay-good.db");
    let bad_path = dir.path().join("replay-bad.db");
    std::fs::copy(&path, &good_path).unwrap();
    std::fs::copy(&path, &bad_path).unwrap();
    drain(&node).await;
    for seq in 2..=10 { command(&node, &id, &format!("message-{seq}"), b"effect").await; }
    assert_eq!(actor.cursor().await.unwrap(), 10);
    let source = connection(&node_path.join(format!("{id}.db"))).await;
    let good = connection(&good_path).await;
    loom_actor::cdc::replay_domain_cdc(&source, &good, 0).await.unwrap();
    assert_eq!(entries_hash(&source).await, entries_hash(&good).await);
    // Counter has exactly one domain table; removing one native CDC image must break its table hash.
    let count = integer(&actor, "SELECT count(*) FROM turso_cdc WHERE table_name='entries'").await;
    assert_eq!(count, 10);
    source.execute("DELETE FROM turso_cdc WHERE change_id=(SELECT MIN(change_id) FROM turso_cdc WHERE table_name='entries')", ())
        .await.unwrap();
    let bad = connection(&bad_path).await;
    loom_actor::cdc::replay_domain_cdc(&source, &bad, 0).await.unwrap();
    assert_ne!(entries_hash(&source).await, entries_hash(&bad).await);
    node.close().await.unwrap();
}

#[tokio::test]
async fn cdc_compacts_at_snapshot_and_floor_is_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let node = node(dir.path(), Config { snapshot_every: 3, ..Config::default() }).await;
    let id = spawn(&node, "counter-v1", b"one", Durability::Local).await;
    drain(&node).await;
    let cap = node.cap_for(&id, Rights::INSPECT).await.unwrap();
    let mut stream = node.open_stream().await.unwrap();
    let sub = node.subscribe_stream(&stream.id, &cap, "entries").await.unwrap();
    assert_eq!(next(&mut stream).await["type"], "snapshot");
    for seq in 2..=3 { command(&node, &id, &format!("message-{seq}"), b"effect").await; }
    while stream.receiver.try_recv().is_ok() {}
    let actor = node.open(&id).await.unwrap();
    let floor = integer(&actor, "SELECT CAST(value AS INTEGER) FROM meta WHERE key='cdc_floor'").await;
    assert!(floor > 0);
    let below = integer(&actor,
        "SELECT COUNT(*) FROM turso_cdc WHERE change_id < (SELECT CAST(value AS INTEGER) FROM meta WHERE key='cdc_floor')").await;
    assert_eq!(below, 0);
    actor.sql("UPDATE subscribers SET after_change_id=0 WHERE id=?", [sub]).await.unwrap();
    node.pump(&id).await.unwrap();
    assert_eq!(next(&mut stream).await["type"], "resnapshot");
    let snapshot = next(&mut stream).await;
    assert_eq!(snapshot["type"], "snapshot");
    assert_eq!(snapshot["rows"].as_array().unwrap().len(), 3);
    assert!(snapshot["change_id"].as_i64().unwrap() >= floor);
    assert!(stream.receiver.try_recv().is_err());
    node.close_stream(&stream.id).await.unwrap();
    assert!(node.subscriptions(&id).await.unwrap().is_empty());
    node.close().await.unwrap();
}

#[tokio::test]
async fn ephemeral_leaves_no_file_no_object_no_lease() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("node");
    let store = dir.path().join("store");
    let config = Config { store: Some(StoreConfig::Local { path: store.clone() }), ..Config::default() };
    let first = node(&path, config.clone()).await;
    drain(&first).await;
    first.ship(&first.root()).await.unwrap();
    let files = listing(&path);
    let objects = listing(&store.join("actors"));
    let root = first.root();
    let id = spawn(&first, "counter-v1", b"one", Durability::Ephemeral).await;
    drain(&first).await;
    first.ship(&id).await.unwrap();
    first.renew_leases().await.unwrap();
    assert_eq!(listing(&path), files);
    assert_eq!(listing(&store.join("actors")), objects);
    assert!(!path.join(format!("{id}.db")).exists());
    assert!(!store.join(format!("actors/{id}")).exists());
    let parent = first.open(&root).await.unwrap();
    assert_eq!(parent.sql("SELECT id FROM children WHERE id=?", [id.as_str()]).await.unwrap().rows.len(), 1);
    first.close().await.unwrap();
    drop(parent);
    drop(first);
    let restarted = node(&path, config).await;
    assert!(restarted.open(&id).await.is_err());
    assert_eq!(restarted.open(&root).await.unwrap().sql("SELECT id FROM children WHERE id=?", [id]).await.unwrap().rows.len(), 1);
    restarted.close().await.unwrap();
}

#[tokio::test]
async fn ephemeral_supports_fork_and_validate() {
    let dir = tempfile::tempdir().unwrap();
    let node = node(dir.path(), Config::default()).await;
    let id = spawn(&node, "counter-v1", b"one", Durability::Ephemeral).await;
    drain(&node).await;
    for seq in 2..=4 { command(&node, &id, &format!("message-{seq}"), b"effect").await; }
    let verdict = node.validate(&id, "counter-v1", 3).await.unwrap();
    assert!(matches!(verdict, Verdict::Matched { .. }), "{verdict:?}");
    let fork = node.fork(&id, 3).await.unwrap();
    let actor = node.open(&fork).await.unwrap();
    assert_eq!(actor.status().await.unwrap(), Status::Fork);
    assert_eq!(actor.cursor().await.unwrap(), 3);
    assert_eq!(integer(&actor, "SELECT COUNT(*) FROM entries").await, 3);
    let durability = actor.sql("SELECT value FROM meta WHERE key='durability'", ()).await.unwrap();
    assert_eq!(durability.rows[0].get::<String>(0).unwrap(), "ephemeral");
    assert!(!dir.path().join(format!("{fork}.db")).exists());
    node.close().await.unwrap();
}

#[tokio::test]
async fn subscribe_snapshot_then_deltas_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let node = node(dir.path(), Config::default()).await;
    let source = spawn(&node, "counter-v1", b"one", Durability::Local).await;
    let subscriber = spawn(&node, "subscriber-test", b"{}", Durability::Local).await;
    drain(&node).await;
    let cap = node.cap_for(&source, Rights::INSPECT).await.unwrap();
    command(&node, &subscriber, "subscribe", &serde_json::to_vec(&json!({"subscribe":cap})).unwrap()).await;
    for seq in 2..=4 { command(&node, &source, &format!("message-{seq}"), b"effect").await; }
    let frames = frames(&node, &subscriber).await;
    assert_eq!(frames.len(), 4);
    assert_eq!(frames[0]["type"], "snapshot");
    assert_eq!(frames[0]["rows"].as_array().unwrap().len(), 1);
    let mut previous = frames[0]["change_id"].as_i64().unwrap();
    for (offset, frame) in frames.iter().skip(1).enumerate() {
        let seq = offset + 2;
        assert_eq!(frame["type"], "delta");
        assert_eq!(frame["source"], source);
        assert_eq!(frame["seq"], seq);
        assert_eq!(frame["key"], format!("message-{seq}"));
        let rows = frame["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        let change = rows[0]["change_id"].as_i64().unwrap();
        assert!(change > previous);
        previous = change;
        for field in ["change_type", "table", "id", "before", "after", "updates"] { assert!(rows[0].get(field).is_some()); }
    }
    command(&node, &subscriber, "unsubscribe", b"{\"unsubscribe\":true}").await;
    assert!(node.subscriptions(&source).await.unwrap().is_empty());
    node.close().await.unwrap();
}

#[tokio::test]
async fn subscribe_requires_inspect() {
    let dir = tempfile::tempdir().unwrap();
    let node = node(dir.path(), Config::default()).await;
    let source = spawn(&node, "counter-v1", b"one", Durability::Local).await;
    let subscriber = spawn(&node, "subscriber-test", b"{}", Durability::Local).await;
    drain(&node).await;
    let cap = node.cap_for(&source, Rights::SEND).await.unwrap();
    command(&node, &subscriber, "denied", &serde_json::to_vec(&json!({"subscribe":cap})).unwrap()).await;
    let actor = node.open(&subscriber).await.unwrap();
    assert_eq!(actor.status().await.unwrap(), Status::Parked);
    let errors = actor.sql("SELECT error FROM dead_letters", ()).await.unwrap();
    assert_eq!(errors.rows.len(), 1);
    let error: String = errors.rows[0].get(0).unwrap();
    for text in [subscriber.as_str(), "seq 2", "subscribe", "missing right"] { assert!(error.contains(text), "{error}"); }
    assert!(node.subscriptions(&source).await.unwrap().is_empty());
    node.close().await.unwrap();
}

#[tokio::test]
async fn view_renders_keyed_trees_and_promote_rerenders_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let node = node(dir.path(), Config::default()).await;
    let source = spawn(&node, "counter-v1", b"one", Durability::Local).await;
    drain(&node).await;
    let init = view_init(&node, &source, "template-a").await;
    let view = spawn(&node, "view-v1", &init, Durability::Ephemeral).await;
    drain(&node).await;
    for seq in 2..=3 { command(&node, &source, &format!("message-{seq}"), b"effect").await; }
    let actor = node.open(&view).await.unwrap();
    assert_eq!(integer(&actor, "SELECT COUNT(*) FROM tree").await, 3);
    let before = actor.sql("SELECT rowid,key,tree FROM tree ORDER BY key", ()).await.unwrap();
    for row in &before.rows {
        let tree: Value = serde_json::from_slice(&row.get::<Vec<u8>>(2).unwrap()).unwrap();
        assert_eq!(tree["attrs"]["class"], "template-a");
    }
    let mut stream = node.open_stream().await.unwrap();
    let cap = node.cap_for(&view, Rights::INSPECT).await.unwrap();
    node.subscribe_stream(&stream.id, &cap, "tree").await.unwrap();
    assert_eq!(next(&mut stream).await["type"], "snapshot");
    node.promote(&view, "template-b", "test", "rerender every key").await.unwrap();
    drain(&node).await;
    let delta = next(&mut stream).await;
    assert_eq!(delta["type"], "delta");
    let rows = delta["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    let keys: BTreeSet<_> = rows.iter().map(|row| row["after"]["key"].as_str().unwrap()).collect();
    assert_eq!(keys, BTreeSet::from(["1", "2", "3"]));
    assert!(stream.receiver.try_recv().is_err(), "promote emits one commit frame");
    assert_eq!(integer(&actor, "SELECT COUNT(*) FROM turso_cdc WHERE table_name='tree' AND change_type=0").await, 3);
    let transactions = integer(&actor,
        "SELECT COUNT(DISTINCT change_txn_id) FROM turso_cdc WHERE table_name='tree' AND change_type=0").await;
    assert_eq!(transactions, 1);
    let after = actor.sql("SELECT rowid,key,tree FROM tree ORDER BY key", ()).await.unwrap();
    for (old, new) in before.rows.iter().zip(&after.rows) {
        assert_eq!(old.get::<i64>(0).unwrap(), new.get::<i64>(0).unwrap());
        assert_eq!(old.get::<String>(1).unwrap(), new.get::<String>(1).unwrap());
        let tree: Value = serde_json::from_slice(&new.get::<Vec<u8>>(2).unwrap()).unwrap();
        assert_eq!(tree["attrs"]["class"], "template-b");
    }
    let hashes = actor.sql("SELECT behavior_hash FROM code_changes ORDER BY seq", ()).await.unwrap();
    let hashes: Vec<String> = hashes.rows.iter().map(|row| row.get(0).unwrap()).collect();
    assert_eq!(hashes, ["template-a", "template-b"]);
    node.close_stream(&stream.id).await.unwrap();
    node.close().await.unwrap();
}

#[tokio::test]
async fn view_refuses_effectful_template() {
    let dir = tempfile::tempdir().unwrap();
    let node = node(dir.path(), Config::default()).await;
    let source = spawn(&node, "counter-v1", b"one", Durability::Local).await;
    drain(&node).await;
    let init = view_init(&node, &source, "template-sleep").await;
    let mut spec = ChildSpec::new("view-v1", &init, ChildType::Worker);
    spec.durability = Durability::Ephemeral;
    let error = node.spawn(&node.root(), &spec).await.unwrap_err();
    assert!(format!("{error:#}").contains("sleep"), "{error:#}");
    let init = view_init(&node, &source, "template-a").await;
    let view = spawn(&node, "view-v1", &init, Durability::Ephemeral).await;
    drain(&node).await;
    let actor = node.open(&view).await.unwrap();
    let before = actor.sql("SELECT * FROM tree ORDER BY key", ()).await.unwrap().rows;
    let error = node.promote(&view, "template-sleep", "test", "must refuse").await.unwrap_err();
    assert!(format!("{error:#}").contains("sleep"), "{error:#}");
    assert_eq!(actor.sql("SELECT * FROM tree ORDER BY key", ()).await.unwrap().rows, before);
    assert_eq!(integer(&actor, "SELECT COUNT(*) FROM code_changes").await, 1);
    node.close().await.unwrap();
}

#[tokio::test]
async fn subscription_crosses_pump_redelivery_once() {
    let dir = tempfile::tempdir().unwrap();
    let node = node(dir.path(), Config::default()).await;
    let source = spawn(&node, "counter-v1", b"one", Durability::Local).await;
    let subscriber = spawn(&node, "subscriber-test", b"{}", Durability::Local).await;
    drain(&node).await;
    let cap = node.cap_for(&source, Rights::INSPECT).await.unwrap();
    command(&node, &subscriber, "subscribe", &serde_json::to_vec(&json!({"subscribe":cap})).unwrap()).await;
    command(&node, &source, "message-2", b"two").await;
    let key = format!("delta:{source}:2:{subscriber}");
    let receiver = node.open(&subscriber).await.unwrap();
    assert_eq!(receiver.sql("SELECT seq FROM inbox WHERE key=?", [key.as_str()]).await.unwrap().rows.len(), 1);
    // Restore exactly the durable crash boundary: receiver committed, sender acknowledgment absent.
    let actor = node.open(&source).await.unwrap();
    actor.sql("UPDATE outbox SET delivered=0 WHERE target=?", [format!("frame:{subscriber}")]).await.unwrap();
    node.pump(&source).await.unwrap();
    drain(&node).await;
    assert_eq!(receiver.sql("SELECT seq FROM inbox WHERE key=?", [key]).await.unwrap().rows.len(), 1);
    assert_eq!(frames(&node, &subscriber).await.len(), 2);
    assert_eq!(integer(&actor, "SELECT COUNT(*) FROM outbox WHERE delivered=0").await, 0);
    node.close().await.unwrap();
}
