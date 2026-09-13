mod cluster_support;
mod registry;

use cluster_support::*;
use loom_actor::{DefaultEffects, Durability, Placement, Rights, Status};
use std::{sync::{Arc, atomic::Ordering}, time::Duration};

#[tokio::test]
async fn two_nodes_pair_fifo() {
    let pair = Pair::new(Durability::Local).await;
    for index in 1..100 {
        pair.send(&format!("message-{index}")).await;
    }
    drain(&pair.n1.node).await;
    drain(&pair.n2.node).await;
    let sender = pair.n1.node.open(&pair.a).await.unwrap();
    let target = pair.n2.node.open(&pair.b).await.unwrap();
    let outgoing = sender.sql("SELECT seq,idx,delivered FROM outbox ORDER BY seq,idx", ()).await.unwrap();
    let incoming = target.sql("SELECT key FROM inbox WHERE sender=? ORDER BY seq", [pair.a.as_str()]).await.unwrap();
    assert_eq!(outgoing.rows.len(), 100);
    assert_eq!(incoming.rows.len(), 100);
    for (sent, received) in outgoing.rows.iter().zip(&incoming.rows) {
        let seq: i64 = sent.get(0).unwrap();
        let idx: i64 = sent.get(1).unwrap();
        assert_eq!(received.get::<String>(0).unwrap(), format!("{}:{seq}:{idx}", pair.a));
        assert_eq!(sent.get::<i64>(2).unwrap(), 1);
    }
    assert!(matches!(pair.n1.node.resolve(&pair.b).await.unwrap(), Placement::Remote { .. }));
}

#[tokio::test]
async fn cold_actor_boots_on_send() {
    let pair = Pair::new(Durability::Local).await;
    let before = lease(&pair.store(), &pair.b)["epoch"].as_u64().unwrap();
    pair.n2.node.close().await.unwrap();
    pair.n2.server.crash();
    drain(&pair.n1.node).await;
    assert!(matches!(pair.n1.node.resolve(&pair.b).await.unwrap(), Placement::Local));
    assert_eq!(forwarded(&pair.n1.node, &pair.b).await, 1);
    assert!(lease(&pair.store(), &pair.b)["epoch"].as_u64().unwrap() > before);
}

#[tokio::test]
async fn owner_death_moves_actor_within_ttl() {
    let pair = Pair::new(Durability::Local).await;
    for key in ["second", "third"] {
        pair.n2.node.send(&pair.b, key, b"committed").await.unwrap();
    }
    drain(&pair.n2.node).await;
    pair.n2.node.ship(&pair.b).await.unwrap();
    assert_eq!(pair.n2.node.open(&pair.b).await.unwrap().cursor().await.unwrap(), 3);
    pair.n2.server.crash();
    drop(pair.n2);
    pair.clock.expire_except(&pair.n1.node).await;
    drain(&pair.n1.node).await;
    let restored = pair.n1.node.open(&pair.b).await.unwrap();
    assert_eq!(restored.cursor().await.unwrap(), 4);
    assert_eq!(integer(&restored, "SELECT COUNT(*) FROM inbox").await, 4);
    assert_eq!(integer(&restored, "SELECT COUNT(*) FROM entries").await, 4);
}

#[tokio::test]
async fn output_gate_holds_unshipped_send() {
    for durability in [Durability::Local, Durability::Remote] {
        let pair = Pair::new(durability).await;
        pair.n1.node.pause_shipping();
        let runner = pair.n1.node.clone();
        let mut attempt = tokio::spawn(async move { runner.run_until_idle().await });
        if durability == Durability::Local {
            assert!(tokio::time::timeout(Duration::from_millis(100), &mut attempt).await.is_err());
            let sender = pair.n1.node.open(&pair.a).await.unwrap();
            assert_eq!(integer(&sender, "SELECT COUNT(*) FROM outbox WHERE delivered=0").await, 1);
            assert_eq!(forwarded(&pair.n2.node, &pair.b).await, 0);
            pair.n1.node.resume_shipping();
        }
        tokio::time::timeout(Duration::from_secs(10), &mut attempt).await.unwrap().unwrap().unwrap();
        assert_eq!(forwarded(&pair.n2.node, &pair.b).await, 1);
        pair.n1.node.resume_shipping();
    }
}

#[tokio::test]
async fn ingress_ack_waits_for_receiver_ship() {
    held_ack_does_not_block_another_pair().await;
    for crash in [false, true] {
        let pair = Pair::new(Durability::Local).await;
        pair.n2.node.ship(&pair.b).await.unwrap();
        pair.n2.node.pause_shipping();
        let runner = pair.n1.node.clone();
        let mut attempt = tokio::spawn(async move { runner.run_until_idle().await });
        wait_forwarded(&pair.n2.node, &pair.b, 1).await;
        assert!(tokio::time::timeout(Duration::from_millis(100), &mut attempt).await.is_err());
        let sender = pair.n1.node.open(&pair.a).await.unwrap();
        assert_eq!(integer(&sender, "SELECT COUNT(*) FROM outbox WHERE delivered=0").await, 1);
        if !crash {
            pair.n2.node.resume_shipping();
            tokio::time::timeout(Duration::from_secs(10), attempt).await.unwrap().unwrap().unwrap();
            assert_eq!(integer(&sender, "SELECT COUNT(*) FROM outbox WHERE delivered=0").await, 0);
        } else {
            pair.n2.server.crash();
            drop(pair.n2);
            // Cancellation leaves A's durable pending row for retry after takeover.
            attempt.abort();
            let _ = attempt.await;
            pair.clock.expire_except(&pair.n1.node).await;
            let restored = pair.n1.node.open(&pair.b).await.unwrap();
            assert_eq!(integer(&restored, "SELECT COUNT(*) FROM inbox WHERE key != 'init' AND sender != 'external'").await, 0);
            drain(&pair.n1.node).await;
            assert_eq!(forwarded(&pair.n1.node, &pair.b).await, 1);
            assert_eq!(integer(&sender, "SELECT COUNT(*) FROM outbox WHERE delivered=0").await, 0);
        }
    }
}

#[tokio::test]
async fn ingress_rejects_wrong_cluster_key() {
    let pair = Pair::new(Durability::Local).await;
    let client = reqwest::Client::new();
    let wrong = blake3::keyed_hash(&[0x91; 32], b"loom-ingress-v1").to_hex().to_string();
    let valid = blake3::keyed_hash(&KEY, b"loom-ingress-v1").to_hex().to_string();
    let ops = serde_json::json!({"ops":[loom_actor::DeliveryOp::Message {
        target: pair.b.clone(), key: "forbidden".into(), sender: pair.a.clone(), msg: b"no".to_vec(),
    }]});
    let ingress = format!("http://{}/v1/ingress", pair.n2.server.addr);
    let response = client.post(&ingress).bearer_auth(wrong).json(&ops).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    let response = client.post(&ingress).bearer_auth(USER_TOKEN).json(&ops).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
    let response = client.post(format!("http://{}/v1/command", pair.n2.server.addr)).bearer_auth(&valid)
        .json(&serde_json::json!({"verb":"actors"})).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
    assert_eq!(forwarded(&pair.n2.node, &pair.b).await, 0);
    let response = client.post(&ingress).bearer_auth(&valid).json(&ops).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(forwarded(&pair.n2.node, &pair.b).await, 1, "positive control must reach the real delivery dispatcher");
}

#[tokio::test]
async fn cap_survives_move() {
    let pair = Pair::new(Durability::Local).await;
    let cap = pair.cap.clone();
    let moved = pair.n1.node.move_actor(&pair.b, "n1").await.unwrap();
    assert_eq!(moved.node_id, "n1");
    assert!(matches!(pair.n1.node.resolve(&pair.b).await.unwrap(), Placement::Local));
    drain(&pair.n1.node).await;
    assert_eq!(forwarded(&pair.n1.node, &pair.b).await, 1);
    let mut forged = cap;
    let mut input = forged.target.as_bytes().to_vec();
    input.extend_from_slice(&forged.cap_id.to_le_bytes());
    input.extend_from_slice(&forged.epoch.to_le_bytes());
    input.extend_from_slice(&forged.rights.bits.to_le_bytes());
    forged.mac = *blake3::keyed_hash(&[0x91; 32], &input).as_bytes();
    let invalid = spawn(&pair.n1.node, "forwarder-v1", &serde_json::to_vec(&forged).unwrap(), Durability::Local).await;
    drain(&pair.n1.node).await;
    let actor = pair.n1.node.open(&invalid).await.unwrap();
    assert_eq!(actor.status().await.unwrap(), Status::Parked);
    let errors = actor.sql("SELECT error FROM dead_letters", ()).await.unwrap();
    assert_eq!(errors.rows.len(), 1);
    assert!(errors.rows[0].get::<String>(0).unwrap().contains("invalid authority"));
    assert_eq!(forwarded(&pair.n1.node, &pair.b).await, 1);
}

#[tokio::test]
async fn stale_owner_cannot_forward() {
    let workspace = tempfile::tempdir().unwrap();
    let store = workspace.path().join("store");
    let clock = Arc::new(ManualClock::default());
    let gate = Arc::new(GatedEffects::default());
    let n1 = node(&workspace.path().join("n1"), &store, "n1", clock.clone(), Arc::new(DefaultEffects)).await;
    let n2 = node(&workspace.path().join("n2"), &store, "n2", clock.clone(), gate.clone()).await;
    let target = spawn(&n1.node, "counter-v1", b"init", Durability::Local).await;
    drain(&n1.node).await;
    let target_cap = n1.node.cap_for(&target, Rights::SEND).await.unwrap();
    let b = spawn(&n2.node, "blocked-forwarder-v1", b"init", Durability::Local).await;
    drain(&n2.node).await;
    n2.node.ship(&b).await.unwrap();
    let old_epoch = lease(&store, &b)["epoch"].as_u64().unwrap();
    let stale = n2.node.open(&b).await.unwrap();
    n2.node.send(&b, "blocked", &serde_json::to_vec(&target_cap).unwrap()).await.unwrap();
    gate.armed.store(true, Ordering::SeqCst);
    let runner = n2.node.clone();
    let attempt = tokio::spawn(async move { runner.run_until_idle().await });
    tokio::time::timeout(Duration::from_secs(10), gate.entered.notified()).await.unwrap();
    clock.expire_except(&n1.node).await;
    let restored = n1.node.open(&b).await.unwrap();
    assert_eq!(restored.cursor().await.unwrap(), 1);
    gate.release.notify_one();
    assert!(tokio::time::timeout(Duration::from_secs(10), attempt).await.unwrap().unwrap().is_err());
    assert!(n2.node.pump(&b).await.is_err());
    assert_eq!(integer(&restored, "SELECT COUNT(*) FROM inbox").await, 1);
    assert_eq!(integer(&stale, "SELECT COUNT(*) FROM outbox").await, 0);
    assert_eq!(forwarded(&n1.node, &target).await, 0);
    assert!(workspace.path().join("n2").join(format!("{b}.stale.{old_epoch}.db")).is_file());
}
