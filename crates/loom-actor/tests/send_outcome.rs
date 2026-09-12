use crate::common::{Counter, H1, registry};
use loom_actor::{ChildSpec, ChildType, Config, DefaultEffects, Node, SendOutcome};
use std::sync::Arc;

#[tokio::test]
async fn concurrent_sends_report_their_own_committed_sequences() {
    let directory = tempfile::tempdir().unwrap();
    let node =
        Node::new(directory.path(), registry(vec![Arc::new(Counter::plain())]), Arc::new(DefaultEffects), Config::default()).await.unwrap();
    let id = node.spawn_root(H1, b"").await.unwrap();
    let mut jobs = Vec::new();
    for key in ["first", "second"] {
        let node = node.clone();
        let id = id.clone();
        jobs.push(tokio::spawn(async move { node.send_with_outcome(&id, key, key.as_bytes()).await.unwrap() }));
    }
    let mut sequences = Vec::new();
    for job in jobs {
        match job.await.unwrap() {
            SendOutcome::Complete { id: received, seq, .. } => {
                assert_eq!(received, id);
                sequences.push(seq);
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }
    sequences.sort();
    assert_eq!(sequences, vec![1, 2]);
    let repeated = node.send_with_outcome(&id, "first", b"first").await.unwrap();
    let actor = node.open(&id).await.unwrap();
    let rows = actor.sql("SELECT seq FROM inbox WHERE key='first'", ()).await.unwrap();
    let expected: i64 = rows.rows[0].get(0).unwrap();
    assert!(matches!(repeated, SendOutcome::Complete { seq, .. } if seq == expected), "{repeated:?}");
    let rows = actor.sql("SELECT COUNT(*) FROM entries", ()).await.unwrap();
    assert_eq!(rows.rows[0].get::<i64>(0).unwrap(), 2);
}

#[tokio::test]
async fn send_failure_survives_supervisor_reset() {
    let directory = tempfile::tempdir().unwrap();
    let node =
        Node::new(directory.path(), registry(vec![Arc::new(Counter::plain())]), Arc::new(DefaultEffects), Config::default()).await.unwrap();
    let id = node.spawn(&node.root(), &ChildSpec::new(H1, b"", ChildType::Worker)).await.unwrap();
    match node.send_with_outcome(&id, "bad", b"poison").await.unwrap() {
        SendOutcome::Failed { id: received, seq, cursor, cause } => {
            assert_eq!(received, id);
            assert_eq!(seq, 1);
            assert_eq!(cursor, 0);
            assert!(cause.contains("counter rejects poison"), "{cause}");
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
    let actor = node.open(&id).await.unwrap();
    let rows = actor.sql("SELECT seq FROM inbox WHERE key='bad'", ()).await.unwrap();
    assert!(rows.rows.is_empty(), "supervisor should have reset the failed incarnation");
}

#[tokio::test]
async fn send_to_stopped_actor_reports_pending() {
    let directory = tempfile::tempdir().unwrap();
    let node =
        Node::new(directory.path(), registry(vec![Arc::new(Counter::plain())]), Arc::new(DefaultEffects), Config::default()).await.unwrap();
    let id = node.spawn_root(H1, b"").await.unwrap();
    node.stop(&id, "test").await.unwrap();
    let outcome = node.send_with_outcome(&id, "pending", b"value").await.unwrap();
    assert!(matches!(outcome, SendOutcome::Pending { seq: 1, .. }), "{outcome:?}");
}
