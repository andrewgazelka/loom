use crate::common;
use crate::registry::Registry;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use common::{Counter, EXTRA, Forwarder, H1, H2, RecordingEffects, integer, registry, table_fingerprints};
use loom_actor::{Cap, Config, DefaultEffects, Node, Rights, Status, Verdict};
use turso::Value;

#[tokio::test]
async fn three_messages_three_rows() {
    let dir = tempfile::tempdir().unwrap();
    let node =
        Node::new(dir.path(), registry(vec![Arc::new(Counter::plain())]), Arc::new(DefaultEffects), Config::default()).await.unwrap();
    let id = node.spawn_root(H1, b"one").await.unwrap();
    node.send(&id, "two", b"two").await.unwrap();
    node.send(&id, "three", b"three").await.unwrap();
    node.run_until_idle().await.unwrap();

    let actor = node.open(&id).await.unwrap();
    assert_eq!(integer(&actor, "SELECT count(*) FROM entries").await, 3);
    assert_eq!(actor.cursor().await.unwrap(), 3);
    assert!(dir.path().join(format!("{id}.db")).is_file());
}

#[tokio::test]
async fn crash_before_commit_reruns_once() {
    let dir = tempfile::tempdir().unwrap();
    let effects = Arc::new(RecordingEffects { fail_seq_two_once: AtomicBool::new(true), ..RecordingEffects::default() });
    let config = Config { max_retries: 2, retry_backoff: Duration::ZERO, ..Config::default() };
    let node = Node::new(dir.path(), registry(vec![Arc::new(Counter::plain())]), effects.clone(), config).await.unwrap();
    let id = node.spawn_root(H1, b"one").await.unwrap();
    node.send(&id, "two", b"effect").await.unwrap();
    node.send(&id, "three", b"three").await.unwrap();
    node.run_until_idle().await.unwrap();

    let actor = node.open(&id).await.unwrap();
    assert_eq!(integer(&actor, "SELECT count(*) FROM entries").await, 3);
    assert_eq!(integer(&actor, "SELECT count(*) FROM entries WHERE seq = 2").await, 1);
    assert_eq!(integer(&actor, "SELECT count(*) FROM effects WHERE seq = 2").await, 1);
    assert_eq!(integer(&actor, "SELECT count(*) FROM dead_letters").await, 0);
    assert_eq!(actor.cursor().await.unwrap(), 3);
    let calls = effects.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    for call in calls.iter() {
        assert_eq!(call.actor_id, id);
        assert_eq!(call.seq, 2);
        assert_eq!(call.idx, 0);
    }
}

#[tokio::test]
async fn trap_rolls_back_send() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::new(
        dir.path(),
        registry(vec![Arc::new(Counter::plain()), Arc::new(Forwarder { trap: true })]),
        Arc::new(DefaultEffects),
        Config::default(),
    )
    .await
    .unwrap();
    let receiver = node.spawn_root(H1, b"init").await.unwrap();
    node.run_until_idle().await.unwrap();
    let b = node.open(&receiver).await.unwrap();
    // Root creation contributes one init row; the application mailbox is empty.
    let baseline = integer(&b, "SELECT count(*) FROM inbox").await;
    let sender = node
        .spawn_root("forwarder-trap", &serde_json::to_vec(&node.cap_for(&receiver, Rights::ALL).await.unwrap()).unwrap())
        .await
        .unwrap();
    node.run_until_idle().await.unwrap();

    let a = node.open(&sender).await.unwrap();
    assert_eq!(integer(&b, "SELECT count(*) FROM inbox").await, baseline);
    assert!(b.sql("SELECT seq FROM inbox WHERE sender = ?1", [sender.as_str()]).await.unwrap().rows.is_empty());
    assert!(a.sql("SELECT seq FROM outbox WHERE target=?", [receiver.as_str()]).await.unwrap().rows.is_empty());
    let notifications = a.sql("SELECT target,msg FROM outbox", ()).await.unwrap();
    assert_eq!(notifications.rows.len(), 1);
    assert_eq!(notifications.rows[0].get::<String>(0).unwrap(), node.root());
    let notification: serde_json::Value = serde_json::from_slice(&notifications.rows[0].get::<Vec<u8>>(1).unwrap()).unwrap();
    assert_eq!(notification["type"], "poison");
    assert_eq!(notification["child"], sender);
    assert_eq!(integer(&a, "SELECT count(*) FROM dead_letters").await, 1);
    assert_eq!(a.status().await.unwrap(), Status::Parked);
    assert_eq!(a.cursor().await.unwrap(), 0);
}

#[tokio::test]
async fn send_delivers_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::new(
        dir.path(),
        registry(vec![Arc::new(Counter::plain()), Arc::new(Forwarder { trap: false })]),
        Arc::new(DefaultEffects),
        Config::default(),
    )
    .await
    .unwrap();
    let receiver = node.spawn_root(H1, b"init").await.unwrap();
    node.run_until_idle().await.unwrap();
    let b = node.open(&receiver).await.unwrap();
    let baseline = integer(&b, "SELECT count(*) FROM inbox").await;
    let sender =
        node.spawn_root("forwarder-v1", &serde_json::to_vec(&node.cap_for(&receiver, Rights::ALL).await.unwrap()).unwrap()).await.unwrap();
    node.run_until_idle().await.unwrap();
    assert_eq!(integer(&b, "SELECT count(*) FROM inbox").await, baseline + 1);

    let a = node.open(&sender).await.unwrap();
    a.sql("UPDATE outbox SET delivered = 0", ()).await.unwrap();
    assert!(node.pump(&sender).await.unwrap());
    node.run_until_idle().await.unwrap();
    assert_eq!(integer(&b, "SELECT count(*) FROM inbox").await, baseline + 1);
    assert_eq!(integer(&b, "SELECT count(*) FROM entries WHERE body = x'666f72776172646564'").await, 1);
    assert_eq!(integer(&a, "SELECT count(*) FROM outbox WHERE delivered = 0").await, 0);
}

#[tokio::test]
async fn short_effect_keyed_and_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let effects = Arc::new(RecordingEffects::default());
    let node = Node::new(dir.path(), registry(vec![Arc::new(Counter::plain())]), effects.clone(), Config::default()).await.unwrap();
    let id = node.spawn_root(H1, b"effect").await.unwrap();
    node.run_until_idle().await.unwrap();

    let actor = node.open(&id).await.unwrap();
    let rows = actor.sql("SELECT seq, idx, kind, request, result FROM effects", ()).await.unwrap();
    assert_eq!(rows.rows.len(), 1);
    let row = &rows.rows[0];
    assert_eq!(row.get_value(0).unwrap(), Value::Integer(1));
    assert_eq!(row.get_value(1).unwrap(), Value::Integer(0));
    assert_eq!(row.get_value(2).unwrap(), Value::Text("echo".into()));
    assert_eq!(row.get_value(3).unwrap(), Value::Blob(b"recorded".to_vec()));
    assert_eq!(row.get_value(4).unwrap(), Value::Blob(b"recorded".to_vec()));
    {
        let calls = effects.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].actor_id, id);
        assert_eq!(calls[0].seq, 1);
        assert_eq!(calls[0].idx, 0);
        assert_eq!(calls[0].kind, "echo");
        assert_eq!(calls[0].request, b"recorded");
    }
    for swallow_effect_errors in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let rejected = Arc::new(RecordingEffects { reject_deterministically: true, ..RecordingEffects::default() });
        let behavior = Counter { swallow_effect_errors, ..Counter::plain() };
        let config = Config { max_retries: 3, retry_backoff: Duration::ZERO, ..Config::default() };
        let runtime = Node::new(dir.path(), registry(vec![Arc::new(behavior)]), rejected.clone(), config).await.unwrap();
        let id = runtime.spawn_root(H1, b"effect").await.unwrap();
        runtime.run_until_idle().await.unwrap();
        let actor = runtime.open(&id).await.unwrap();
        assert_eq!(actor.status().await.unwrap(), Status::Parked);
        assert_eq!(actor.cursor().await.unwrap(), 0);
        assert_eq!(integer(&actor, "SELECT count(*) FROM entries").await, 0);
        assert_eq!(integer(&actor, "SELECT count(*) FROM effects").await, 0);
        assert_eq!(integer(&actor, "SELECT count(*) FROM dead_letters").await, 1);
        assert_eq!(rejected.calls.lock().unwrap().iter().filter(|call| call.actor_id == id).count(), 1);
    }
}

#[tokio::test]
async fn long_effect_result_arrives_as_message() {
    let dir = tempfile::tempdir().unwrap();
    let node =
        Node::new(dir.path(), registry(vec![Arc::new(Counter::plain())]), Arc::new(DefaultEffects), Config::default()).await.unwrap();
    let id = node.spawn_root(H1, b"request").await.unwrap();
    node.run_until_idle().await.unwrap();

    let actor = node.open(&id).await.unwrap();
    let rows = actor.sql("SELECT msg FROM inbox WHERE key = 'req:1:0'", ()).await.unwrap();
    assert_eq!(rows.rows.len(), 1);
    assert_eq!(rows.rows[0].get_value(0).unwrap(), Value::Blob(b"x".to_vec()));
    assert_eq!(integer(&actor, "SELECT count(*) FROM entries WHERE body = x'78'").await, 1);
    assert_eq!(actor.cursor().await.unwrap(), 2);
}

#[tokio::test]
async fn fork_matched() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config { snapshot_every: 4, ..Config::default() };
    let node = Node::new(dir.path(), registry(vec![Arc::new(Counter::plain())]), Arc::new(DefaultEffects), config).await.unwrap();
    let id = node.spawn_root(H1, b"effect").await.unwrap();
    for seq in 2..=6 {
        node.send(&id, &format!("input-{seq}"), b"effect").await.unwrap();
    }
    node.run_until_idle().await.unwrap();
    let actor = node.open(&id).await.unwrap();
    assert_eq!(actor.cursor().await.unwrap(), 6);
    assert_eq!(integer(&actor, "SELECT count(*) FROM snapshots WHERE seq = 4").await, 1);

    let verdict = node.validate(&id, H1, 2).await.unwrap();
    match verdict {
        Verdict::Matched { tables } => assert!(!tables.is_empty()),
        other => panic!("expected matched, got {other:?}"),
    }
    let fork_id = node.fork(&id, 5).await.unwrap();
    let fork = node.open(&fork_id).await.unwrap();
    assert_eq!(fork.status().await.unwrap(), Status::Fork);
    assert_eq!(fork.cursor().await.unwrap(), 5);
    assert_eq!(integer(&fork, "SELECT count(*) FROM entries").await, 5);
}

#[tokio::test]
async fn fork_diverged() {
    let dir = tempfile::tempdir().unwrap();
    let receiver_behavior = Arc::new(Counter { hash: "receiver", ..Counter::plain() });
    let bootstrap =
        Node::new(dir.path(), registry(vec![receiver_behavior.clone()]), Arc::new(DefaultEffects), Config::default()).await.unwrap();
    let receiver = bootstrap.spawn_root("receiver", b"init").await.unwrap();
    bootstrap.run_until_idle().await.unwrap();
    let receiver_cap = bootstrap.cap_for(&receiver, Rights::ALL).await.unwrap();
    drop(bootstrap);
    let original = Arc::new(Counter { target: Some(receiver_cap.clone()), ..Counter::plain() });
    let candidate = Arc::new(Counter { hash: EXTRA, extra_effect: true, target: Some(receiver_cap.clone()), ..Counter::plain() });
    let node = Node::new(
        dir.path(),
        registry(vec![receiver_behavior, original, candidate]),
        Arc::new(DefaultEffects),
        Config { snapshot_every: 4, ..Config::default() },
    )
    .await
    .unwrap();
    let id = node.spawn_root(H1, b"message").await.unwrap();
    for seq in 2..=6 {
        node.send(&id, &format!("input-{seq}"), b"message").await.unwrap();
    }
    node.run_until_idle().await.unwrap();
    let actor = node.open(&id).await.unwrap();
    let b = node.open(&receiver).await.unwrap();
    let before = table_fingerprints(&actor).await;
    let receiver_before = table_fingerprints(&b).await;

    match node.validate(&id, EXTRA, 2).await.unwrap() {
        Verdict::DivergedAt { seq, idx, expected, got } => {
            assert_eq!(seq, 5);
            // Capability verification is index 0, followed by the send at 1.
            assert_eq!(idx, 2);
            assert!(expected.is_empty(), "original log lacks this effect, expected must be empty: {expected:?}");
            let got: serde_json::Value = serde_json::from_slice(&got).unwrap();
            assert_eq!(got["kind"], "echo");
            assert_eq!(got["request"], serde_json::json!(b"recorded".to_vec()));
        }
        other => panic!("expected effect divergence, got {other:?}"),
    }
    node.run_until_idle().await.unwrap();
    assert_eq!(table_fingerprints(&actor).await, before);
    assert_eq!(table_fingerprints(&b).await, receiver_before);
}

#[tokio::test]
async fn promote_then_rollback_lineage() {
    let dir = tempfile::tempdir().unwrap();
    let h2 = Arc::new(Counter { hash: H2, upgraded: true, ..Counter::plain() });
    let node =
        Node::new(dir.path(), registry(vec![Arc::new(Counter::plain()), h2]), Arc::new(DefaultEffects), Config::default()).await.unwrap();
    let id = node.spawn_root(H1, b"poison").await.unwrap();
    node.run_until_idle().await.unwrap();
    let actor = node.open(&id).await.unwrap();
    assert_eq!(actor.status().await.unwrap(), Status::Parked);
    assert_eq!(actor.cursor().await.unwrap(), 0);
    node.promote(&id, H2, "test", "add revision and accept poison").await.unwrap();
    assert_eq!(actor.status().await.unwrap(), Status::Running);
    node.run_until_idle().await.unwrap();
    node.send(&id, "h2", b"two").await.unwrap();
    node.run_until_idle().await.unwrap();
    assert_eq!(integer(&actor, "SELECT count(*) FROM entries WHERE revision = 'added'").await, 2);

    node.promote(&id, H1, "test", "restore original handler").await.unwrap();
    node.send(&id, "h1", b"three").await.unwrap();
    node.run_until_idle().await.unwrap();
    let rows = actor.sql("SELECT implementation, revision FROM entries WHERE seq = 3", ()).await.unwrap();
    assert_eq!(rows.rows.len(), 1);
    assert_eq!(rows.rows[0].get_value(0).unwrap(), Value::Text(H1.into()));
    assert_eq!(rows.rows[0].get_value(1).unwrap(), Value::Null);
    let lineage = actor.sql("SELECT behavior_hash FROM code_changes ORDER BY seq", ()).await.unwrap();
    let hashes: Vec<Value> = lineage.rows.iter().map(|row| row.get_value(0).unwrap()).collect();
    assert_eq!(hashes, vec![Value::Text(H1.into()), Value::Text(H2.into()), Value::Text(H1.into())]);
    assert_eq!(actor.cursor().await.unwrap(), 3);
}

#[tokio::test]
async fn validate_differs_and_multi_promotion() {
    use async_trait::async_trait;
    use loom_actor::{Behavior, Ctx, Trap};

    struct Revision {
        hash: &'static str,
        upgraded: bool,
        changed: bool,
    }
    #[async_trait]
    impl Behavior for Revision {
        fn hash(&self) -> &str {
            self.hash
        }
        fn schema(&self) -> &str {
            match self.hash {
                "history-h1" => "CREATE TABLE measurements(seq INTEGER, value INTEGER)",
                "history-h2" | "history-h3" => "ALTER TABLE measurements ADD COLUMN upgraded INTEGER",
                _ => "",
            }
        }
        async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
            let result = cx.effect("echo", msg).await?;
            let value = i64::from(result[0]) + i64::from(self.changed);
            let seq = cx.seq();
            if self.upgraded {
                cx.sql("INSERT INTO measurements(seq,value,upgraded) VALUES (?,?,1)", turso::params![seq, value]).await?;
            } else {
                cx.sql("INSERT INTO measurements(seq,value) VALUES (?,?)", turso::params![seq, value]).await?;
            }
            Ok(())
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let node = Node::new(
        dir.path(),
        registry(vec![
            Arc::new(Revision { hash: "history-h1", upgraded: false, changed: false }),
            Arc::new(Revision { hash: "history-h2", upgraded: true, changed: false }),
            Arc::new(Revision { hash: "history-h3", upgraded: true, changed: true }),
        ]),
        Arc::new(DefaultEffects),
        Config { snapshot_every: 64, ..Config::default() },
    )
    .await
    .unwrap();
    let id = node.spawn_root("history-h1", &[1]).await.unwrap();
    for seq in 2..=3 {
        node.send(&id, &format!("input-{seq}"), &[seq]).await.unwrap();
    }
    node.run_until_idle().await.unwrap();
    node.promote(&id, "history-h2", "test", "add upgraded column after message 3").await.unwrap();
    for seq in 4..=6 {
        node.send(&id, &format!("input-{seq}"), &[seq]).await.unwrap();
    }
    node.run_until_idle().await.unwrap();
    let original = node.open(&id).await.unwrap();
    assert_eq!(original.cursor().await.unwrap(), 6);
    assert_eq!(integer(&original, "SELECT COUNT(*) FROM effects WHERE seq>0").await, 6);
    assert_eq!(integer(&original, "SELECT SUM(value) FROM measurements").await, 21);
    let before = table_fingerprints(&original).await;

    let pre_id = node.fork(&id, 3).await.unwrap();
    let pre = node.open(&pre_id).await.unwrap();
    let columns = pre.sql("PRAGMA table_info(measurements)", ()).await.unwrap();
    assert!(!columns.rows.iter().any(|row| row.get::<String>(1).unwrap() == "upgraded"));
    let post_id = node.fork(&id, 4).await.unwrap();
    let post = node.open(&post_id).await.unwrap();
    assert_eq!(post.cursor().await.unwrap(), 4);
    assert_eq!(integer(&post, "SELECT upgraded FROM measurements WHERE seq=4").await, 1);
    assert_eq!(integer(&post, "SELECT COUNT(*) FROM measurements WHERE seq<=3 AND upgraded IS NULL").await, 3);
    let lineage = post.sql("SELECT behavior_hash FROM code_changes ORDER BY seq", ()).await.unwrap();
    let hashes: Vec<String> = lineage.rows.iter().map(|row| row.get(0).unwrap()).collect();
    assert_eq!(hashes, ["history-h1", "history-h2"]);

    match node.validate(&id, "history-h3", 4).await.unwrap() {
        Verdict::Differs { tables } => {
            assert_eq!(tables.len(), 1);
            let difference = &tables[0];
            assert_eq!(difference.name, "measurements");
            assert_eq!(difference.original_hash.len(), 64);
            assert_eq!(difference.fork_hash.len(), 64);
            assert_ne!(difference.original_hash, difference.fork_hash);
        }
        other => panic!("expected changed domain table with matching effects, got {other:?}"),
    }
    assert_eq!(table_fingerprints(&original).await, before);
}

#[tokio::test]
async fn memory_io_is_wired() {
    struct Caller;
    #[async_trait::async_trait]
    impl loom_actor::Behavior for Caller {
        fn hash(&self) -> &str {
            "echo-caller-test"
        }
        fn schema(&self) -> &str {
            "CREATE TABLE replies(msg BLOB)"
        }
        async fn handle(&self, cx: &mut loom_actor::Ctx<'_>, msg: &[u8]) -> Result<(), loom_actor::Trap> {
            if let Ok(envelope) = serde_json::from_slice::<serde_json::Value>(msg)
                && envelope["type"] == "reply"
            {
                cx.sql("INSERT INTO replies(msg) VALUES (?)", [msg]).await?;
                return Ok(());
            }
            let target: Cap = serde_json::from_slice(msg).map_err(|error| loom_actor::Trap::new(error.to_string()))?;
            cx.call(&target, b"echo witness", 1000).await?;
            Ok(())
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let node = loom_actor::Node::new(
        dir.path(),
        registry(vec![Arc::new(Caller)]),
        Arc::new(loom_actor::DefaultEffects),
        Config { io: loom_actor::Io::Memory, ..Config::default() },
    )
    .await
    .unwrap();
    let id = node.spawn_root("counter-v1", b"init").await.unwrap();
    node.send(&id, "two", b"two").await.unwrap();
    node.send(&id, "three", b"three").await.unwrap();
    node.run_until_idle().await.unwrap();
    assert_eq!(node.open(&id).await.unwrap().cursor().await.unwrap(), 3);
    let echo = node.spawn_root("echo-v1", &[]).await.unwrap();
    let caller =
        node.spawn_root("echo-caller-test", &serde_json::to_vec(&node.cap_for(&echo, Rights::ALL).await.unwrap()).unwrap()).await.unwrap();
    node.run_until_idle().await.unwrap();
    let rows = node.open(&caller).await.unwrap().sql("SELECT msg FROM replies", ()).await.unwrap();
    assert_eq!(rows.rows.len(), 1);
    let envelope: serde_json::Value = serde_json::from_slice(&rows.rows[0].get::<Vec<u8>>(0).unwrap()).unwrap();
    assert_eq!(envelope["msg"], serde_json::json!(b"echo witness".to_vec()));
    // Memory I/O keeps history as logical in-memory images (docs/ui-view-actor.md, Durability::Ephemeral):
    // fork and validate work without any file, and the directory stays empty.
    let fork = node.fork(&id, 0).await.unwrap();
    assert_eq!(node.open(&fork).await.unwrap().cursor().await.unwrap(), 0);
    assert!(matches!(node.validate(&id, "counter-v1", 3).await.unwrap(), loom_actor::Verdict::Matched { .. }));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);

    #[cfg(not(target_os = "linux"))]
    {
        let result = loom_actor::Node::new(
            dir.path(),
            Arc::new(Registry::new()),
            Arc::new(loom_actor::DefaultEffects),
            Config { io: loom_actor::Io::IoUring, ..Config::default() },
        )
        .await;
        let error = match result {
            Ok(_) => panic!("io_uring unexpectedly opened on this platform"),
            Err(error) => error,
        };
        let message = format!("{error:#}");
        assert!(message.contains(std::env::consts::OS), "{message}");
        assert!(message.contains("io_uring"), "{message}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
