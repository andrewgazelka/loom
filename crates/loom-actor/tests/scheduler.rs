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

/// Exercise a fresh root and a nonempty child initialization without a warm-up
/// drain. Check committed domain rows and history, since poison also counts as
/// scheduler progress and run_until_idle may return Ok after parking an actor.
#[tokio::test]
async fn fresh_root_and_child_commit_all_three_messages() {
    for io in [Io::Memory, Io::Syscall] {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = Registry::new();
        registry.insert("scheduler-selective".into(), Arc::new(Selective));
        let node = Node::new(dir.path(), Arc::new(registry), Arc::new(DefaultEffects), Config { io, ..Config::default() }).await.unwrap();
        let id = node.spawn_root("scheduler-selective", b"one").await.unwrap();
        node.send(&id, "two", b"two").await.unwrap();
        node.send(&id, "three", b"three").await.unwrap();
        let processed = node.run_until_idle().await.unwrap();
        for actor_id in [node.root(), id.clone()] {
            let actor = node.open(&actor_id).await.unwrap();
            let errors = actor.sql("SELECT error FROM dead_letters ORDER BY seq", ()).await.unwrap();
            let errors: Vec<String> = errors.rows.iter().map(|row| row.get(0).unwrap()).collect();
            assert!(errors.is_empty(), "actor {actor_id}: {errors:?}");
            assert_eq!(actor.status().await.unwrap(), crate::Status::Running);
        }
        assert_eq!(processed, 4, "root configure plus three fresh child messages");
        assert_eq!(node.open(&node.root()).await.unwrap().cursor().await.unwrap(), 1);
        let actor = node.open(&id).await.unwrap();
        assert_eq!(actor.cursor().await.unwrap(), 3);
        let rows = actor.sql("SELECT value FROM received ORDER BY rowid", ()).await.unwrap();
        let received: Vec<String> = rows.rows.iter().map(|row| row.get(0).unwrap()).collect();
        assert_eq!(received, ["one", "two", "three"]);
        let rows = actor.sql("SELECT seq FROM inbox WHERE state='done' ORDER BY seq", ()).await.unwrap();
        let committed: Vec<i64> = rows.rows.iter().map(|row| row.get(0).unwrap()).collect();
        assert_eq!(committed, [1, 2, 3]);
        for seq in 1..=3 {
            let conn = actor.conn.lock().await;
            assert_eq!(crate::actor::meta(&conn, &format!("commit_order:{seq}")).await.unwrap(), seq.to_string());
            assert_eq!(crate::actor::meta(&conn, &format!("boundary:{seq}")).await.unwrap(), seq.to_string());
            assert_eq!(crate::actor::meta(&conn, &format!("code_at:{seq}")).await.unwrap(), "0");
        }
        assert_eq!(node.run_until_idle().await.unwrap(), 0);
    }
}

async fn mailbox_fixture(depth: i64) -> turso::Connection {
    let mut conn = crate::actor::connect(std::path::Path::new(":memory:"), Io::Memory).await.unwrap();
    let tx = conn.transaction().await.unwrap();
    tx.execute_batch(crate::SCHEMA).await.unwrap();
    for seq in 1..=depth {
        tx.execute(
            "INSERT INTO inbox(seq,key,sender,msg) VALUES (?,?,?,?)",
            turso::params![seq, seq.to_string(), "host", b"message".as_slice()],
        )
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    conn
}

#[tokio::test]
async fn completion_seeks_have_bounded_statement_counts_at_two_thousand_rows() {
    for depth in [2, 2_000] {
        let mut conn = mailbox_fixture(depth).await;
        for seq in 1..=depth {
            crate::mailbox::STATEMENTS
                .scope(std::cell::Cell::new(0), async {
                    let tx = conn.transaction().await.unwrap();
                    assert_eq!(crate::mailbox::next_at(&tx, seq - 1).await.unwrap().unwrap().seq, seq);
                    assert_eq!(crate::mailbox::STATEMENTS.with(|count| count.replace(0)), 2);
                    let completion = crate::mailbox::complete_at(&tx, seq, seq - 1, Some(0)).await.unwrap();
                    assert_eq!(completion.cursor, seq);
                    assert_eq!(completion.epoch, seq);
                    assert!(completion.boundary);
                    let statements = crate::mailbox::STATEMENTS.with(|count| count.get());
                    assert_eq!(statements, if seq == depth { 6 } else { 5 }, "depth {depth}, seq {seq}");
                    tx.commit().await.unwrap();
                })
                .await;
        }
        assert_eq!(crate::actor::cursor(&conn).await.unwrap(), depth);
        let rows = crate::actor::query(&conn, "SELECT COUNT(*) FROM inbox WHERE state='done'", ()).await.unwrap();
        assert_eq!(rows.rows[0].get::<i64>(0).unwrap(), depth);
    }
}

#[tokio::test]
async fn completion_matches_aggregate_with_deferred_holes_and_out_of_order_done_rows() {
    let mut conn = mailbox_fixture(12).await;
    conn.execute("UPDATE inbox SET state='deferred',defer_epoch=0,defer_count=1 WHERE seq=2 OR seq=5", ()).await.unwrap();
    for (epoch, seq) in [12, 3, 1, 4, 6, 2, 11, 7, 8, 10, 5, 9].into_iter().enumerate() {
        let tx = conn.transaction().await.unwrap();
        let completion = crate::mailbox::complete_at(&tx, seq, epoch as i64, Some(0)).await.unwrap();
        let reference = crate::actor::query(&tx, "SELECT COALESCE(MIN(CASE WHEN state!='done' THEN seq END)-1,MAX(seq),0), COALESCE(MAX(CASE WHEN state='done' THEN seq END),0), COUNT(CASE WHEN state='done' THEN 1 END) FROM inbox", ()).await.unwrap();
        let row = &reference.rows[0];
        let cursor = row.get::<i64>(0).unwrap();
        let boundary = row.get::<i64>(2).unwrap() == 0 || row.get::<i64>(1).unwrap() <= cursor;
        assert_eq!(completion.cursor, cursor);
        assert_eq!(completion.boundary, boundary);
        assert_eq!(crate::actor::meta(&tx, &format!("commit_order:{}", epoch + 1)).await.unwrap(), seq.to_string());
        assert_eq!(crate::actor::meta(&tx, &format!("code_at:{seq}")).await.unwrap(), "0");
        if boundary {
            assert_eq!(crate::actor::meta(&tx, &format!("boundary:{cursor}")).await.unwrap(), (epoch + 1).to_string());
        }
        tx.commit().await.unwrap();
    }
}

/// A constant statement count alone also passes the old quadratic aggregate.
/// Pin the native query plans as well: explicit index searches, no temp sort.
#[tokio::test]
async fn mailbox_queries_use_ordered_index_searches() {
    let conn = mailbox_fixture(2_000).await;
    for sql in [
        crate::mailbox::FIRST_PENDING.to_owned(),
        crate::mailbox::FIRST_DEFERRED.to_owned(),
        crate::mailbox::NEXT_PENDING.to_owned(),
        crate::mailbox::NEXT_OTHER_DEFERRED.to_owned(),
        crate::mailbox::DONE_ABOVE.replace('?', "1000"),
        "SELECT MIN(seq) FROM inbox INDEXED BY inbox_state_seq WHERE state='pending'".into(),
        "SELECT MAX(seq) FROM inbox INDEXED BY inbox_state_seq WHERE state='done'".into(),
        crate::pump::UNDELIVERED.into(),
    ] {
        let rows = crate::actor::query(&conn, &format!("EXPLAIN QUERY PLAN {sql}"), ()).await.unwrap();
        let details: Vec<String> = rows.rows.iter().map(|row| row.get(3).unwrap()).collect();
        let plan = details.join("\n");
        assert!(plan.contains("SEARCH"), "{sql}: {plan}");
        assert!(plan.contains("inbox_state_seq") || plan.contains("outbox_delivered_seq_idx"), "{sql}: {plan}");
        assert!(!plan.contains("TEMP B-TREE"), "{sql}: {plan}");
        let rows = crate::actor::query(&conn, &format!("EXPLAIN {sql}"), ()).await.unwrap();
        let opcodes: Vec<String> = rows.rows.iter().map(|row| row.get(1).unwrap()).collect();
        assert!(!opcodes.iter().any(|opcode| opcode == "SorterOpen" || opcode == "OpenEphemeral"), "{sql}: {opcodes:?}");
    }
    // The eligible-deferred query ranges over defer_epoch, so the planner may sort
    // the matching rows (bounded by eligible deferred rows, never the whole inbox);
    // it must still search the epoch index rather than scan the table.
    let sql = crate::mailbox::NEXT_DEFERRED.replace('?', "1");
    let rows = crate::actor::query(&conn, &format!("EXPLAIN QUERY PLAN {sql}"), ()).await.unwrap();
    let plan: Vec<String> = rows.rows.iter().map(|row| row.get(3).unwrap()).collect();
    let plan = plan.join("\n");
    assert!(plan.contains("SEARCH") && plan.contains("inbox_state_epoch_seq"), "{sql}: {plan}");
    assert!(!plan.contains("SCAN"), "{sql}: {plan}");
    // Missing indexes must be an error, never an unnoticed return to table scans.
    conn.execute("DROP INDEX inbox_state_seq", ()).await.unwrap();
    assert!(crate::actor::query(&conn, crate::mailbox::FIRST_PENDING, ()).await.is_err());
}

#[tokio::test]
async fn indexed_selection_matches_case_order_for_mixed_deferred_epochs() {
    for epoch in [-1, 0, 1, 2, 3, 10] {
        let conn = mailbox_fixture(12).await;
        conn.execute("UPDATE inbox SET state='done' WHERE seq=4 OR seq=11", ()).await.unwrap();
        conn.execute("UPDATE inbox SET state='deferred',defer_epoch=2 WHERE seq=1 OR seq=7", ()).await.unwrap();
        conn.execute("UPDATE inbox SET state='deferred',defer_epoch=0 WHERE seq=3 OR seq=9", ()).await.unwrap();
        loop {
            let expected = crate::actor::query(&conn, "SELECT seq,msg,sender FROM inbox WHERE state!='done' ORDER BY CASE WHEN state='deferred' AND defer_epoch<? THEN 0 WHEN state='pending' THEN 1 ELSE 2 END,seq LIMIT 1", [epoch]).await.unwrap();
            let next = crate::mailbox::next_at(&conn, epoch).await.unwrap();
            let Some(row) = expected.rows.first() else {
                assert!(next.is_none());
                break;
            };
            let next = next.unwrap();
            assert_eq!(next.seq, row.get::<i64>(0).unwrap());
            assert_eq!(next.msg, row.get::<Vec<u8>>(1).unwrap());
            assert!(next.sender.is_none());
            conn.execute("UPDATE inbox SET state='done' WHERE seq=?", [next.seq]).await.unwrap();
        }
    }
}

#[tokio::test]
async fn outbox_index_skips_delivered_history_and_keeps_sequence_index_order() {
    let mut conn = mailbox_fixture(0).await;
    let tx = conn.transaction().await.unwrap();
    for seq in 1..=2_000 {
        tx.execute("INSERT INTO outbox(seq,idx,target,msg,delivered) VALUES (?,0,'target',X'00',1)", [seq]).await.unwrap();
    }
    tx.execute_batch(
        "INSERT INTO outbox(seq,idx,target,msg) VALUES (2002,0,'target',X'03'),(2001,2,'target',X'02'),(2001,1,'target',X'01');",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let rows = crate::actor::query(&conn, crate::pump::UNDELIVERED, ()).await.unwrap();
    let payloads: Vec<Vec<u8>> = rows.rows.iter().map(|row| row.get(3).unwrap()).collect();
    assert_eq!(payloads, [vec![1], vec![2], vec![3]]);
}

#[tokio::test]
async fn opening_a_restored_actor_installs_missing_mailbox_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("restored.db");
    let conn = crate::actor::connect(&path, Io::Syscall).await.unwrap();
    conn.execute_batch(crate::SCHEMA).await.unwrap();
    conn.execute_batch("DROP INDEX inbox_state_seq; DROP INDEX inbox_state_epoch_seq; DROP INDEX outbox_delivered_seq_idx;").await.unwrap();
    drop(conn);
    for _ in 0..2 {
        let conn = crate::actor::connect(&path, Io::Syscall).await.unwrap();
        let rows = crate::actor::query(&conn, "SELECT name FROM sqlite_schema WHERE type='index' AND name IN ('inbox_state_seq','inbox_state_epoch_seq','outbox_delivered_seq_idx')", ()).await.unwrap();
        assert_eq!(rows.rows.len(), 3);
    }
}

/// The indexed selection and cursor queries enumerate the three states by name;
/// a fourth state would be an unschedulable row the cursor skips, so the schema
/// refuses it at the write.
#[tokio::test]
async fn inbox_state_domain_is_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = Registry::new();
    registry.insert("scheduler-noop".into(), Arc::new(Noop));
    let node =
        Node::new(dir.path(), Arc::new(registry), Arc::new(DefaultEffects), Config { io: Io::Memory, ..Config::default() }).await.unwrap();
    let id = node.spawn_root("scheduler-noop", b"init").await.unwrap();
    let actor = node.open(&id).await.unwrap();
    let error = actor.sql("UPDATE inbox SET state='blocked' WHERE seq=1", ()).await.unwrap_err();
    assert!(format!("{error:#}").to_lowercase().contains("check"), "expected a CHECK violation, got: {error:#}");
    let rows = actor.sql("SELECT state FROM inbox WHERE seq=1", ()).await.unwrap();
    assert_eq!(rows.rows[0].get::<String>(0).unwrap(), "pending");
}
