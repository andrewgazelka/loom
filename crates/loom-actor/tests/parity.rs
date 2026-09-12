use crate::registry::Registry;
use async_trait::async_trait;
use loom_actor::{
    Actor, Behavior, ChildSpec, Config, Ctx, DefaultEffects, EffectError, EffectHandler, EffectKey, Node, RestartPolicy, Shutdown, Status,
    Trap,
};
use loom_actor::{Cap, Rights};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

struct Fixture {
    mode: &'static str,
}

#[async_trait]
impl Behavior for Fixture {
    fn hash(&self) -> &str {
        self.mode
    }
    fn schema(&self) -> &str {
        "CREATE TABLE events(body BLOB); CREATE TABLE saved(ref TEXT); CREATE TABLE terminated(reason TEXT)"
    }
    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        if self.mode == "defer-always" {
            return cx.defer();
        }
        let input: Value = serde_json::from_slice(msg).map_err(|e| Trap::new(e.to_string()))?;
        let text = |key: &str| input[key].as_str().ok_or_else(|| Trap::new(format!("missing {key}")));
        let cap = |key: &str| serde_json::from_value::<Cap>(input[key].clone()).map_err(|e| Trap::new(e.to_string()));
        match input["type"].as_str() {
            Some("init") => {
                cx.trap_exit(input["trap_exit"].as_bool().unwrap_or(false)).await?;
                return Ok(());
            }
            Some("watch") => {
                let reference = cx.monitor(&cap("target")?).await?;
                cx.sql("INSERT INTO saved(ref) VALUES (?)", [reference]).await?;
            }
            Some("batch") => {
                for number in 0..10 {
                    cx.send(&cap("target")?, &encoded(json!({"type":"number","n":number}))).await?;
                    if number == 4 {
                        cx.request("pump_barrier", &[]).await?;
                    }
                }
                cx.exit("boom").await?;
            }
            Some("x") if self.mode == "selective" => {
                let rows = cx.sql("SELECT body FROM events", ()).await?;
                let seen_z = rows.rows.iter().any(|row| {
                    let body: Vec<u8> = row.get(0).unwrap();
                    serde_json::from_slice::<Value>(&body).unwrap()["type"] == "z"
                });
                if !seen_z {
                    return cx.defer();
                }
            }
            Some("make_call") => {
                let timeout = input["timeout"].as_u64().ok_or_else(|| Trap::new("missing timeout"))?;
                let reference = cx.call(&cap("target")?, b"request", timeout).await?;
                cx.sql("INSERT INTO saved(ref) VALUES (?)", [reference]).await?;
            }
            Some("call") => {
                if self.mode == "reply" {
                    cx.reply(&cap("reply_cap")?, text("ref")?, b"answer").await?;
                }
                if self.mode == "die" {
                    cx.exit("boom").await?;
                }
            }
            Some("ask") => {
                let mut request = input["request"].clone();
                request["reply_cap"] = serde_json::to_value(cx.self_cap().await?).map_err(|e| Trap::new(e.to_string()))?;
                cx.send(&cap("target")?, &encoded(request)).await?;
            }
            _ => {}
        }
        if self.mode == "selective" && input.get("target").is_some() {
            cx.send(&cap("target")?, msg).await?;
        }
        cx.sql("INSERT INTO events(body) VALUES (?)", [msg]).await?;
        Ok(())
    }
    async fn terminate(&self, cx: &mut Ctx<'_>, reason: &str) -> Result<(), Trap> {
        cx.sql("INSERT INTO terminated(reason) VALUES (?)", [reason]).await?;
        Ok(())
    }
}

fn encoded(value: Value) -> Vec<u8> {
    serde_json::to_vec(&value).unwrap()
}

async fn node(dir: &std::path::Path) -> Node {
    node_with_effects(dir, Arc::new(DefaultEffects)).await
}

async fn node_with_effects(dir: &std::path::Path, effects: Arc<dyn EffectHandler>) -> Node {
    let mut registry = Registry::new();
    for mode in ["ordinary", "selective", "defer-always", "reply", "silent", "die"] {
        registry.insert(mode.to_owned(), Arc::new(Fixture { mode }) as Arc<dyn Behavior>);
    }
    Node::new(dir, Arc::new(registry), effects, Config::default()).await.unwrap()
}

struct PumpBarrier {
    first_call: AtomicBool,
    entered: tokio::sync::Notify,
}

#[async_trait]
impl EffectHandler for PumpBarrier {
    async fn call(&self, key: &EffectKey, kind: &str, req: &[u8]) -> Result<Vec<u8>, EffectError> {
        if kind != "pump_barrier" {
            return DefaultEffects.call(key, kind, req).await;
        }
        if self.first_call.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(encoded(json!({"type":"barrier_result"})))
    }
}

async fn drain(node: &Node) {
    tokio::time::timeout(Duration::from_secs(60), node.run_until_idle())
        .await
        .expect("parity test did not become idle in 60 seconds")
        .unwrap();
}

async fn spawn(node: &Node, hash: &str) -> String {
    node.spawn_root(hash, &encoded(json!({"type":"init"}))).await.unwrap()
}

async fn inbox(actor: &Actor) -> Vec<Value> {
    actor
        .sql("SELECT msg FROM inbox ORDER BY seq", ())
        .await
        .unwrap()
        .rows
        .iter()
        .map(|row| serde_json::from_slice(&row.get::<Vec<u8>>(0).unwrap()).unwrap())
        .collect()
}

async fn count(actor: &Actor, table: &str) -> i64 {
    actor.sql(&format!("SELECT COUNT(*) FROM {table}"), ()).await.unwrap().rows[0].get(0).unwrap()
}

async fn reference(actor: &Actor) -> String {
    actor.sql("SELECT ref FROM saved ORDER BY rowid DESC LIMIT 1", ()).await.unwrap().rows[0].get(0).unwrap()
}

#[tokio::test]
async fn pair_fifo_and_down_after_messages() {
    let dir = tempfile::tempdir().unwrap();
    let barrier = Arc::new(PumpBarrier { first_call: AtomicBool::new(true), entered: tokio::sync::Notify::new() });
    let runtime = node_with_effects(dir.path(), barrier.clone()).await;
    let a = spawn(&runtime, "ordinary").await;
    let b = spawn(&runtime, "ordinary").await;
    runtime.send(&b, "watch", &encoded(json!({"type":"watch","target":runtime.cap_for(&a, Rights::ALL).await.unwrap()}))).await.unwrap();
    drain(&runtime).await;
    runtime.send(&a, "batch", &encoded(json!({"type":"batch","target":runtime.cap_for(&b, Rights::ALL).await.unwrap()}))).await.unwrap();
    let running_node = runtime.clone();
    let running = tokio::spawn(async move { running_node.run_until_idle().await });
    tokio::time::timeout(Duration::from_secs(30), barrier.entered.notified()).await.expect("pump never reached its midpoint");
    let receiver = runtime.open(&b).await.unwrap();
    let midpoint: Vec<Value> = inbox(&receiver).await.into_iter().filter(|msg| msg["type"] == "number" || msg["type"] == "down").collect();
    assert_eq!(midpoint.len(), 5);
    for (number, message) in midpoint.iter().enumerate() {
        assert_eq!(message["n"], number);
    }
    running.abort();
    assert!(running.await.unwrap_err().is_cancelled());
    drop(receiver);
    drop(runtime);

    let runtime = node_with_effects(dir.path(), barrier.clone()).await;
    drain(&runtime).await;
    let receiver = runtime.open(&b).await.unwrap();
    let before: Vec<Value> = inbox(&receiver).await.into_iter().filter(|msg| msg["type"] == "number" || msg["type"] == "down").collect();
    assert_eq!(before.len(), 11);
    for (number, message) in before.iter().take(10).enumerate() {
        assert_eq!(message["n"], number);
    }
    assert_eq!(before[10]["type"], "down");
    assert_eq!(before[10]["from"], a);
    assert_eq!(before[10]["reason"], "boom");
    assert_eq!(before[10]["ref"], reference(&receiver).await);

    // Simulate lost delivery acknowledgments for the latter half before reopening.
    let sender = runtime.open(&a).await.unwrap();
    sender.sql("UPDATE outbox SET delivered=0 WHERE idx>=5 OR idx<0", ()).await.unwrap();
    drop(sender);
    drop(receiver);
    drop(runtime);
    let runtime = node_with_effects(dir.path(), barrier).await;
    drain(&runtime).await;
    let after: Vec<Value> =
        inbox(&runtime.open(&b).await.unwrap()).await.into_iter().filter(|msg| msg["type"] == "number" || msg["type"] == "down").collect();
    assert_eq!(after, before);
}

#[tokio::test]
async fn defer_is_selective_receive() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = node(dir.path()).await;
    let b = spawn(&runtime, "selective").await;
    let receiver = spawn(&runtime, "ordinary").await;
    for kind in ["x", "y", "z"] {
        runtime
            .send(&b, kind, &encoded(json!({"type":kind,"target":runtime.cap_for(&receiver, Rights::ALL).await.unwrap()})))
            .await
            .unwrap();
    }
    drain(&runtime).await;
    let actor = runtime.open(&b).await.unwrap();
    let rows = actor.sql("SELECT body FROM events ORDER BY rowid", ()).await.unwrap();
    let committed: Vec<Value> =
        rows.rows.iter().map(|row| serde_json::from_slice::<Value>(&row.get::<Vec<u8>>(0).unwrap()).unwrap()["type"].clone()).collect();
    assert_eq!(committed, vec![json!("y"), json!("z"), json!("x")]);
    assert_eq!(actor.cursor().await.unwrap(), 4);
    let outbox = actor.sql("SELECT msg FROM outbox WHERE target=? ORDER BY seq,idx", [receiver.as_str()]).await.unwrap();
    let delivered: Vec<Value> =
        outbox.rows.iter().map(|row| serde_json::from_slice::<Value>(&row.get::<Vec<u8>>(0).unwrap()).unwrap()["type"].clone()).collect();
    assert_eq!(delivered, committed);

    let looping = spawn(&runtime, "defer-always").await;
    drain(&runtime).await;
    let looping = runtime.open(&looping).await.unwrap();
    assert_eq!(looping.status().await.unwrap(), Status::Parked);
    let errors = looping.sql("SELECT seq,error FROM dead_letters", ()).await.unwrap();
    assert_eq!(errors.rows.len(), 1);
    assert_eq!(errors.rows[0].get::<i64>(0).unwrap(), 1);
    assert!(errors.rows[0].get::<String>(1).unwrap().contains("seq 1"));
}

#[tokio::test]
async fn call_reply_timeout_and_death() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = node(dir.path()).await;
    let caller = spawn(&runtime, "ordinary").await;
    for mode in ["reply", "silent", "die"] {
        let callee = spawn(&runtime, mode).await;
        let timeout = if mode == "silent" { 50 } else { 5_000 };
        runtime
            .send(
                &caller,
                mode,
                &encoded(json!({"type":"make_call","target":runtime.cap_for(&callee, Rights::ALL).await.unwrap(),"timeout":timeout})),
            )
            .await
            .unwrap();
        drain(&runtime).await;
        let actor = runtime.open(&caller).await.unwrap();
        let reference = reference(&actor).await;
        let responses: Vec<Value> = inbox(&actor)
            .await
            .into_iter()
            .filter(|msg| msg["ref"] == reference && matches!(msg["type"].as_str(), Some("reply" | "down" | "call_timeout")))
            .collect();
        assert_eq!(responses.len(), 1, "call to {mode}: {responses:?}");
        let expected = match mode {
            "reply" => "reply",
            "silent" => "call_timeout",
            "die" => "down",
            _ => unreachable!(),
        };
        assert_eq!(responses[0]["type"], expected);
        if mode == "reply" {
            assert_eq!(responses[0]["msg"], json!(b"answer".to_vec()));
        }
        if mode == "die" {
            assert_eq!(responses[0]["reason"], "boom");
        }
        assert_eq!(count(&actor, "calls").await, 0);
    }
}

#[tokio::test]
async fn kill_terminate_and_shutdown_timeout() {
    let default_supervisor: ChildSpec = serde_json::from_value(json!({
        "behavior_hash":"custom-supervisor", "init":[], "type":"supervisor"
    }))
    .unwrap();
    assert_eq!(default_supervisor.shutdown, Shutdown::Infinity);
    let explicit_supervisor: ChildSpec = serde_json::from_value(json!({
        "behavior_hash":"custom-supervisor", "init":[], "type":"supervisor", "shutdown":"brutal"
    }))
    .unwrap();
    assert_eq!(explicit_supervisor.shutdown, Shutdown::Brutal);
    let dir = tempfile::tempdir().unwrap();
    let runtime = node(dir.path()).await;
    let graceful = spawn(&runtime, "ordinary").await;
    let killed = spawn(&runtime, "ordinary").await;
    drain(&runtime).await;
    runtime.stop(&graceful, "shutdown").await.unwrap();
    runtime.stop(&killed, "kill").await.unwrap();
    drain(&runtime).await;
    let graceful = runtime.open(&graceful).await.unwrap();
    let killed = runtime.open(&killed).await.unwrap();
    let rows = graceful.sql("SELECT reason FROM terminated", ()).await.unwrap();
    assert_eq!(rows.rows.len(), 1);
    assert_eq!(rows.rows[0].get::<String>(0).unwrap(), "shutdown");
    assert_eq!(count(&killed, "terminated").await, 0);
    assert_eq!(runtime.info(killed.id()).await.unwrap().reason, "killed");

    let supervisor = runtime.spawn_root("supervisor-v1", &encoded(json!({"type":"configure","strategy":"one_for_one"}))).await.unwrap();
    let mut spec = ChildSpec::new(
        "ordinary",
        &encoded(json!({"type":"init","trap_exit":true})),
        runtime.behavior("ordinary").await.unwrap().child_type(),
    );
    spec.restart = RestartPolicy::Temporary;
    spec.shutdown = Shutdown::TimeoutMs(30);
    runtime.send(&supervisor, "start", &encoded(json!({"type":"start_child","spec":spec}))).await.unwrap();
    drain(&runtime).await;
    let supervisor_actor = runtime.open(&supervisor).await.unwrap();
    let child: String = supervisor_actor.sql("SELECT id FROM children", ()).await.unwrap().rows[0].get(0).unwrap();
    let watcher = spawn(&runtime, "ordinary").await;
    runtime
        .send(&watcher, "watch", &encoded(json!({"type":"watch","target":runtime.cap_for(&child, Rights::ALL).await.unwrap()})))
        .await
        .unwrap();
    drain(&runtime).await;
    runtime.send(&supervisor, "terminate", &encoded(json!({"type":"terminate_child","id":child}))).await.unwrap();
    drain(&runtime).await;
    let info = runtime.info(&child).await.unwrap();
    assert_eq!(info.status, Status::Stopped);
    assert_eq!(info.reason, "killed");
    assert_eq!(count(&runtime.open(&child).await.unwrap(), "terminated").await, 0);
    let downs: Vec<Value> = inbox(&runtime.open(&watcher).await.unwrap()).await.into_iter().filter(|msg| msg["type"] == "down").collect();
    assert_eq!(downs.len(), 1);
    assert_eq!(downs[0]["reason"], "killed");
}

#[tokio::test]
async fn dynamic_supervisor_and_registry() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = node(dir.path()).await;
    let mut template =
        ChildSpec::new("ordinary", &encoded(json!({"type":"init"})), runtime.behavior("ordinary").await.unwrap().child_type());
    template.restart = RestartPolicy::Temporary;
    let supervisor =
        runtime.spawn_root("supervisor-v1", &encoded(json!({"type":"configure","strategy":"dynamic","template":template}))).await.unwrap();
    for number in 0..3 {
        runtime
            .send(
                &supervisor,
                &format!("child-{number}"),
                &encoded(json!({"type":"start_child","init":encoded(json!({"type":"init","number":number}))})),
            )
            .await
            .unwrap();
    }
    drain(&runtime).await;
    let supervisor_actor = runtime.open(&supervisor).await.unwrap();
    let children: Vec<String> = supervisor_actor
        .sql("SELECT id FROM children ORDER BY rowid", ())
        .await
        .unwrap()
        .rows
        .iter()
        .map(|row| row.get(0).unwrap())
        .collect();
    assert_eq!(children.len(), 3);
    let requester = spawn(&runtime, "ordinary").await;
    runtime
        .send(
            &requester,
            "count",
            &encoded(
                json!({"type":"ask","target":runtime.cap_for(&supervisor, Rights::ALL).await.unwrap(),"request":{"type":"count_children"}}),
            ),
        )
        .await
        .unwrap();
    drain(&runtime).await;
    let replies: Vec<Value> =
        inbox(&runtime.open(&requester).await.unwrap()).await.into_iter().filter(|msg| msg["type"] == "count_children").collect();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["count"], 3);
    runtime.register("child", &children[1]).await.unwrap();
    for child in &children {
        runtime.join("workers", child).await.unwrap();
    }
    assert_eq!(runtime.whereis("child").await.unwrap(), Some(children[1].clone()));
    assert_eq!(runtime.members("workers").await.unwrap().len(), 3);
    runtime.send(&supervisor, "terminate", &encoded(json!({"type":"terminate_child","id":children[1]}))).await.unwrap();
    drain(&runtime).await;
    assert_eq!(runtime.whereis("child").await.unwrap(), None);
    let members = runtime.members("workers").await.unwrap();
    assert_eq!(members.len(), 2);
    assert!(!members.contains(&children[1]));
}
