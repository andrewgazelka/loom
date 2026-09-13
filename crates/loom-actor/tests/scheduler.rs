//! Unit-test registration in lib.rs keeps the step-attempt hook out of production.
use crate::test_registry::Registry;
use crate::{Behavior, Config, Ctx, DefaultEffects, Io, Node, Trap};
use async_trait::async_trait;
use std::sync::Arc;

struct Noop;
#[async_trait]
impl Behavior for Noop {
    fn hash(&self) -> &str {
        "scheduler-noop"
    }
    fn schema(&self) -> &str {
        "CREATE TABLE unused(value INTEGER)"
    }
    async fn handle(&self, _cx: &mut Ctx<'_>, _msg: &[u8]) -> Result<(), Trap> {
        Ok(())
    }
}

#[tokio::test]
async fn messages_step_only_their_woken_actor_and_wake_an_idle_recipient() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = Registry::new();
    registry.insert("scheduler-noop".into(), Arc::new(Noop));
    let node =
        Node::new(dir.path(), Arc::new(registry), Arc::new(DefaultEffects), Config { io: Io::Memory, ..Config::default() }).await.unwrap();
    let busy = node.spawn_root("scheduler-noop", b"init").await.unwrap();
    let mut idle = Vec::new();
    for _ in 0..20 {
        idle.push(node.spawn_root("scheduler-noop", b"init").await.unwrap());
    }
    node.run_until_idle().await.unwrap();
    node.scheduling().unwrap().step_attempts.clear();

    for n in 0..32 {
        node.send(&busy, &format!("message:{n}"), b"message").await.unwrap();
        assert_eq!(node.run_until_idle().await.unwrap(), 1);
    }
    {
        let state = node.scheduling().unwrap();
        for id in &idle {
            assert_eq!(state.step_attempts.get(id).copied().unwrap_or(0), 0, "idle actor {id} was stepped");
        }
        assert_eq!(state.step_attempts.get(&node.root()).copied().unwrap_or(0), 0);
    }
    let recipient = &idle[0];
    let before = node.open(recipient).await.unwrap().cursor().await.unwrap();
    node.send(recipient, "wake-idle", b"message").await.unwrap();
    assert_eq!(node.run_until_idle().await.unwrap(), 1);
    assert_eq!(node.open(recipient).await.unwrap().cursor().await.unwrap(), before + 1);
    assert!(node.scheduling().unwrap().step_attempts[recipient] > 0);
}

#[tokio::test]
async fn unknown_wake_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::new(dir.path(), Arc::new(Registry::new()), Arc::new(DefaultEffects), Config { io: Io::Memory, ..Config::default() })
        .await
        .unwrap();
    node.run_until_idle().await.unwrap();
    let missing = crate::ids::root();
    node.wake_actor(&missing).unwrap();
    let error = node.run_until_idle().await.unwrap_err();
    assert!(format!("{error:#}").contains("actor file does not exist"));
    assert!(node.scheduling().unwrap().woken.contains(&missing));
}

#[tokio::test]
async fn one_hundred_messages_share_bounded_scheduler_batches() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = Registry::new();
    registry.insert("scheduler-noop".into(), Arc::new(Noop));
    let node =
        Node::new(dir.path(), Arc::new(registry), Arc::new(DefaultEffects), Config { io: Io::Memory, ..Config::default() }).await.unwrap();
    let id = node.spawn_root("scheduler-noop", b"").await.unwrap();
    node.run_until_idle().await.unwrap();
    node.scheduling().unwrap().step_attempts.clear();
    for n in 0..100 {
        node.send(&id, &format!("batch:{n}"), b"message").await.unwrap();
    }
    assert_eq!(node.run_until_idle().await.unwrap(), 100);
    assert_eq!(node.open(&id).await.unwrap().cursor().await.unwrap(), 100);
    let actor = node.open(&id).await.unwrap();
    let rows = actor.sql("SELECT COUNT(*) FROM inbox WHERE state='done'", ()).await.unwrap();
    assert_eq!(rows.rows[0].get::<i64>(0).unwrap(), 100);
    let attempts = node.scheduling().unwrap().step_attempts[&id];
    assert!((2..=3).contains(&attempts), "100 messages used {attempts} scheduler tasks");
}

struct Selective;
#[async_trait]
impl Behavior for Selective {
    fn hash(&self) -> &str {
        "scheduler-selective"
    }
    fn schema(&self) -> &str {
        "CREATE TABLE received(value TEXT)"
    }
    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        if msg == b"defer" && cx.sql("SELECT value FROM received WHERE value='release'", ()).await?.rows.is_empty() {
            return cx.defer();
        }
        cx.sql("INSERT INTO received(value) VALUES (?)", [std::str::from_utf8(msg).unwrap()]).await?;
        Ok(())
    }
}

#[tokio::test]
async fn deferral_ends_batch_then_retries_after_intervening_commit() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = Registry::new();
    registry.insert("scheduler-selective".into(), Arc::new(Selective));
    let node =
        Node::new(dir.path(), Arc::new(registry), Arc::new(DefaultEffects), Config { io: Io::Memory, ..Config::default() }).await.unwrap();
    let id = node.spawn_root("scheduler-selective", b"").await.unwrap();
    node.run_until_idle().await.unwrap();
    node.scheduling().unwrap().step_attempts.clear();
    for msg in ["first", "defer", "release", "last"] {
        node.send(&id, msg, msg.as_bytes()).await.unwrap();
    }
    // The deferred attempt counts as a processed turn, as before batching.
    assert_eq!(node.run_until_idle().await.unwrap(), 5);
    assert_eq!(node.scheduling().unwrap().step_attempts[&id], 2);
    let actor = node.open(&id).await.unwrap();
    assert_eq!(actor.cursor().await.unwrap(), 4);
    let rows = actor.sql("SELECT value FROM received ORDER BY rowid", ()).await.unwrap();
    let received: Vec<String> = rows.rows.iter().map(|row| row.get(0).unwrap()).collect();
    assert_eq!(received, ["first", "release", "defer", "last"]);
}
