use crate::registry::Registry;
use loom_actor::{Cap, Rights};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use loom_actor::{Actor, Behavior, ChildSpec, Config, Ctx, DefaultEffects, Node, Status, Trap};
use serde_json::{Value, json};

struct Probe;

#[async_trait]
impl Behavior for Probe {
    fn hash(&self) -> &str {
        "probe-v1"
    }
    fn schema(&self) -> &str {
        "CREATE TABLE events(seq INTEGER, msg BLOB); CREATE TABLE references_saved(ref TEXT)"
    }

    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        let command: Value = serde_json::from_slice(msg).map_err(|error| Trap::new(error.to_string()))?;
        match command["type"].as_str() {
            Some("fail") => return Err(Trap::new("probe poison")),
            Some("init") => {
                cx.trap_exit(command["trap_exit"].as_bool().unwrap_or(false)).await?;
                if let Some(spec) = command.get("spec") {
                    let spec: ChildSpec = serde_json::from_value(spec.clone()).map_err(|error| Trap::new(error.to_string()))?;
                    let child = cx.spawn(&spec).await?;
                    if command["monitor"].as_bool() == Some(true) {
                        let reference = cx.monitor(&child).await?;
                        cx.sql("INSERT INTO references_saved(ref) VALUES (?)", [reference]).await?;
                    }
                }
            }
            Some("watch") => {
                let target: Cap = serde_json::from_value(command["target"].clone()).map_err(|e| Trap::new(e.to_string()))?;
                let reference = cx.monitor(&target).await?;
                cx.sql("INSERT INTO references_saved(ref) VALUES (?)", [reference]).await?;
            }
            Some("terminate") => cx.exit("normal").await?,
            Some("ask") => {
                let target: Cap = serde_json::from_value(command["target"].clone()).map_err(|e| Trap::new(e.to_string()))?;
                let reply_cap = cx.self_cap().await?;
                cx.send(&target, &bytes(json!({"type":"which_children", "reply_cap":reply_cap}))).await?;
            }
            _ => {}
        }
        let seq = cx.seq();
        cx.sql("INSERT INTO events(seq,msg) VALUES (?,?)", turso::params![seq, msg]).await?;
        Ok(())
    }
}

struct FixedProbe;
#[async_trait]
impl Behavior for FixedProbe {
    fn hash(&self) -> &str {
        "probe-fixed"
    }
    fn schema(&self) -> &str {
        ""
    }
    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        let command: Value = serde_json::from_slice(msg).map_err(|error| Trap::new(error.to_string()))?;
        if command["type"] == "fail" {
            let seq = cx.seq();
            cx.sql("INSERT INTO events(seq,msg) VALUES (?,?)", turso::params![seq, msg]).await?;
            Ok(())
        } else {
            Probe.handle(cx, msg).await
        }
    }
}

struct AlwaysFails;

#[async_trait]
impl Behavior for AlwaysFails {
    fn hash(&self) -> &str {
        "always-fails-v1"
    }
    fn schema(&self) -> &str {
        ""
    }
    async fn handle(&self, _cx: &mut Ctx<'_>, _msg: &[u8]) -> Result<(), Trap> {
        Err(Trap::new("unconditional poison"))
    }
}

async fn node(dir: &std::path::Path) -> Node {
    let mut registry = Registry::new();
    let probe: Arc<dyn Behavior> = Arc::new(Probe);
    let failure: Arc<dyn Behavior> = Arc::new(AlwaysFails);
    registry.insert(probe.hash().into(), probe);
    registry.insert(failure.hash().into(), failure);
    registry.insert("probe-fixed".into(), Arc::new(FixedProbe));
    Node::new(dir, Arc::new(registry), Arc::new(DefaultEffects), Config::default()).await.unwrap()
}

fn bytes(value: Value) -> Vec<u8> {
    serde_json::to_vec(&value).unwrap()
}

async fn drain(node: &Node) {
    tokio::time::timeout(Duration::from_secs(60), node.run_until_idle())
        .await
        .expect("supervision did not become idle in 60 seconds")
        .unwrap();
}

async fn children(actor: &Actor) -> Vec<String> {
    actor
        .sql("SELECT id FROM children ORDER BY spawned_seq, id", ())
        .await
        .unwrap()
        .rows
        .iter()
        .map(|row| row.get::<String>(0).unwrap())
        .collect()
}

async fn messages(actor: &Actor, kind: &str) -> Vec<Value> {
    let rows = actor.sql("SELECT msg FROM inbox ORDER BY seq", ()).await.unwrap();
    rows.rows
        .iter()
        .map(|row| serde_json::from_slice::<Value>(&row.get::<Vec<u8>>(0).unwrap()).unwrap())
        .filter(|message| message["type"].as_str() == Some(kind))
        .collect()
}

async fn meta(actor: &Actor, key: &str) -> String {
    let rows = actor.sql("SELECT value FROM meta WHERE key = ?", [key]).await.unwrap();
    assert_eq!(rows.rows.len(), 1, "missing meta key {key}");
    rows.rows[0].get::<String>(0).unwrap()
}

#[tokio::test]
async fn monitor_delivers_down_once() {
    let dir = tempfile::tempdir().unwrap();
    let node = node(dir.path()).await;
    let b = node.spawn_root("probe-v1", &bytes(json!({"type":"init"}))).await.unwrap();
    let a = node.spawn_root("probe-v1", &bytes(json!({"type":"init"}))).await.unwrap();
    node.send(&a, "watch", &bytes(json!({"type":"watch","target":node.cap_for(&b, Rights::ALL).await.unwrap()}))).await.unwrap();
    drain(&node).await;
    let watcher = node.open(&a).await.unwrap();
    let saved = watcher.sql("SELECT ref FROM references_saved", ()).await.unwrap();
    assert_eq!(saved.rows.len(), 1);
    let reference: String = saved.rows[0].get(0).unwrap();

    node.send(&b, "terminate", &bytes(json!({"type":"terminate"}))).await.unwrap();
    drain(&node).await;
    let downs = messages(&watcher, "down").await;
    assert_eq!(downs.len(), 1);
    assert_eq!(downs[0]["ref"], reference);
    assert_eq!(downs[0]["from"], b);
    assert_eq!(downs[0]["reason"], "normal");
    assert_eq!(node.open(&b).await.unwrap().status().await.unwrap(), Status::Stopped);

    // Re-deliver the committed self-stop, exercising idempotent fanout as well as an idle pump.
    let target = node.open(&b).await.unwrap();
    target.sql("UPDATE outbox SET delivered = 0 WHERE target LIKE 'stop:%'", ()).await.unwrap();
    node.pump(&b).await.unwrap();
    node.pump(&b).await.unwrap();
    drain(&node).await;
    assert_eq!(messages(&watcher, "down").await, downs);
}

#[tokio::test]
async fn monitor_after_stop_delivers_down() {
    let dir = tempfile::tempdir().unwrap();
    let node = node(dir.path()).await;
    let b = node.spawn_root("probe-v1", &bytes(json!({"type":"init"}))).await.unwrap();
    let a = node.spawn_root("probe-v1", &bytes(json!({"type":"init"}))).await.unwrap();
    node.send(&b, "terminate", &bytes(json!({"type":"terminate"}))).await.unwrap();
    drain(&node).await;
    assert_eq!(node.open(&b).await.unwrap().status().await.unwrap(), Status::Stopped);
    // Watching a target that is already stopped queues the DOWN in the target's outbox;
    // only the target's own pump delivers it, so the target must be woken.
    node.send(&a, "watch", &bytes(json!({"type":"watch","target":node.cap_for(&b, Rights::ALL).await.unwrap()}))).await.unwrap();
    drain(&node).await;
    let watcher = node.open(&a).await.unwrap();
    let downs = messages(&watcher, "down").await;
    assert_eq!(downs.len(), 1);
    assert_eq!(downs[0]["from"], b);
    assert_eq!(downs[0]["reason"], "normal");
}

#[tokio::test]
async fn link_cascades_stop() {

    for trap_exits in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let node = node(dir.path()).await;
        let c_spec = ChildSpec::new("probe-v1", &bytes(json!({"type":"init"})), node.behavior("probe-v1").await.unwrap().child_type());
        let b_spec =
            ChildSpec::new("probe-v1", &bytes(json!({"type":"init","spec":c_spec})), node.behavior("probe-v1").await.unwrap().child_type());
        let a = node.spawn_root("probe-v1", &bytes(json!({"type":"init","trap_exit":trap_exits,"spec":b_spec}))).await.unwrap();
        drain(&node).await;
        let parent = node.open(&a).await.unwrap();
        let b_ids = children(&parent).await;
        assert_eq!(b_ids.len(), 1);
        let b = node.open(&b_ids[0]).await.unwrap();
        let c_ids = children(&b).await;
        assert_eq!(c_ids.len(), 1);
        let c = node.open(&c_ids[0]).await.unwrap();
        b.sql("UPDATE meta SET value = 'stop' WHERE key = 'strategy'", ()).await.unwrap();
        node.send(&b_ids[0], "fail", &bytes(json!({"type":"fail"}))).await.unwrap();
        drain(&node).await;

        assert_eq!(b.status().await.unwrap(), Status::Stopped);
        assert_eq!(c.status().await.unwrap(), Status::Stopped);
        assert_eq!(meta(&b, "reason").await, "poison");
        assert_eq!(meta(&c, "reason").await, "poison");
        if trap_exits {
            assert_eq!(parent.status().await.unwrap(), Status::Running);
            let exits = messages(&parent, "exit").await;
            assert_eq!(exits.len(), 1);
            assert_eq!(exits[0]["from"], b_ids[0]);
            assert_eq!(exits[0]["reason"], "poison");
        } else {
            assert_eq!(parent.status().await.unwrap(), Status::Stopped);
            assert_eq!(meta(&parent, "reason").await, "poison");
        }
    }
}

#[tokio::test]
async fn one_for_one_reset_then_intensity() {
    let dir = tempfile::tempdir().unwrap();
    let node = node(dir.path()).await;
    let config = bytes(json!({"type":"configure","strategy":"one_for_one","max_restarts":2,"max_seconds":60}));
    let supervisor_spec = ChildSpec::new("supervisor-v1", &config, node.behavior("supervisor-v1").await.unwrap().child_type());
    let parent_id =
        node.spawn_root("probe-v1", &bytes(json!({"type":"init","trap_exit":true,"monitor":true,"spec":supervisor_spec}))).await.unwrap();
    drain(&node).await;
    let parent = node.open(&parent_id).await.unwrap();
    let supervisor_ids = children(&parent).await;
    assert_eq!(supervisor_ids.len(), 1);
    let supervisor_id = &supervisor_ids[0];
    let spec = ChildSpec::new("always-fails-v1", b"{}", node.behavior("always-fails-v1").await.unwrap().child_type());
    node.send(supervisor_id, "start", &bytes(json!({"type":"start_child","spec":spec}))).await.unwrap();
    drain(&node).await;

    let supervisor = node.open(supervisor_id).await.unwrap();
    let child_ids = children(&supervisor).await;
    assert_eq!(child_ids.len(), 1);
    let child = &child_ids[0];
    assert!(dir.path().join(format!("{child}.reset.1.db")).is_file());
    assert!(dir.path().join(format!("{child}.reset.2.db")).is_file());
    assert!(!dir.path().join(format!("{child}.reset.3.db")).exists());
    assert_eq!(supervisor.status().await.unwrap(), Status::Stopped);
    assert_eq!(meta(&supervisor, "reason").await, "shutdown");
    let downs = messages(&parent, "down").await;
    assert_eq!(downs.len(), 1);
    assert_eq!(downs[0]["from"], *supervisor_id);
    assert_eq!(downs[0]["reason"], "shutdown");
    let saved = parent.sql("SELECT ref FROM references_saved", ()).await.unwrap();
    assert_eq!(downs[0]["ref"], saved.rows[0].get::<String>(0).unwrap());
    assert_eq!(parent.status().await.unwrap(), Status::Running);
}

#[tokio::test]
async fn rest_for_one_order() {
    let dir = tempfile::tempdir().unwrap();
    let node = node(dir.path()).await;
    let supervisor_id = node
        .spawn_root("supervisor-v1", &bytes(json!({"type":"configure","strategy":"rest_for_one","max_restarts":3,"max_seconds":60})))
        .await
        .unwrap();
    for name in ["A", "B", "C"] {
        let spec =
            ChildSpec::new("probe-v1", &bytes(json!({"type":"init","name":name})), node.behavior("probe-v1").await.unwrap().child_type());
        node.send(&supervisor_id, name, &bytes(json!({"type":"start_child","spec":spec}))).await.unwrap();
    }
    drain(&node).await;
    let supervisor = node.open(&supervisor_id).await.unwrap();
    let ids = children(&supervisor).await;
    assert_eq!(ids.len(), 3);
    node.register("middle-child", &ids[1]).await.unwrap();
    assert_eq!(node.whereis("middle-child").await.unwrap(), Some(ids[1].clone()));
    node.send(&ids[0], "a-state", &bytes(json!({"type":"tick","value":"keep"}))).await.unwrap();
    node.send(&ids[2], "c-state", &bytes(json!({"type":"tick","value":"discard"}))).await.unwrap();
    drain(&node).await;
    let a = node.open(&ids[0]).await.unwrap();
    let a_cursor = a.cursor().await.unwrap();
    let a_events = a.sql("SELECT seq,msg FROM events ORDER BY seq", ()).await.unwrap().rows;
    node.send(&ids[1], "fail", &bytes(json!({"type":"fail"}))).await.unwrap();
    drain(&node).await;

    assert_eq!(a.cursor().await.unwrap(), a_cursor);
    assert_eq!(a.sql("SELECT seq,msg FROM events ORDER BY seq", ()).await.unwrap().rows, a_events);
    assert!(!dir.path().join(format!("{}.reset.1.db", ids[0])).exists());
    for child in &ids[1..] {
        assert!(dir.path().join(format!("{child}.reset.1.db")).is_file());
        assert_eq!(node.open(child).await.unwrap().status().await.unwrap(), Status::Running);
    }
    let c = node.open(&ids[2]).await.unwrap();
    assert_eq!(c.cursor().await.unwrap(), 1);
    assert_eq!(c.sql("SELECT msg FROM events", ()).await.unwrap().rows.len(), 1);
    assert_eq!(node.whereis("middle-child").await.unwrap(), None);
    let tree = node.tree(&supervisor_id).await.unwrap();
    assert_eq!(tree.len(), 4);
    assert_eq!(tree[0].depth, 0);
    assert_eq!(tree[0].id, supervisor_id);
    assert_eq!(tree[0].behavior_hash, "supervisor-v1");
    assert_eq!(tree[0].status, Status::Running);
    for index in 0..ids.len() {
        assert_eq!(tree[index + 1].depth, 1);
        assert_eq!(tree[index + 1].id, ids[index]);
        assert_eq!(tree[index + 1].behavior_hash, "probe-v1");
        assert_eq!(tree[index + 1].status, Status::Running);
    }

    let requester = node.spawn_root("probe-v1", &bytes(json!({"type":"init"}))).await.unwrap();
    node.send(&requester, "ask", &bytes(json!({"type":"ask","target":node.cap_for(&supervisor_id, Rights::ALL).await.unwrap()})))
        .await
        .unwrap();
    drain(&node).await;
    let replies = messages(&node.open(&requester).await.unwrap(), "children").await;
    assert_eq!(replies.len(), 1);
    let listed = replies[0]["children"].as_array().unwrap();
    assert_eq!(listed.len(), 3);
    for index in 0..ids.len() {
        assert_eq!(listed[index]["id"], ids[index]);
        assert_eq!(listed[index]["status"], "running");
    }
}

#[tokio::test]
async fn resume_keeps_tree_intact() {
    let dir = tempfile::tempdir().unwrap();
    let node = node(dir.path()).await;
    let supervisor_id = node.spawn_root("supervisor-v1", &bytes(json!({"type":"configure","max_restarts":0}))).await.unwrap();
    let grandchild_spec = ChildSpec::new("probe-v1", &bytes(json!({"type":"init"})), node.behavior("probe-v1").await.unwrap().child_type());
    let child_spec = ChildSpec::new(
        "probe-v1",
        &bytes(json!({"type":"init","spec":grandchild_spec,"monitor":true})),
        node.behavior("probe-v1").await.unwrap().child_type(),
    );
    node.send(&supervisor_id, "start", &bytes(json!({"type":"start_child","spec":child_spec}))).await.unwrap();
    drain(&node).await;
    let supervisor = node.open(&supervisor_id).await.unwrap();
    let child_id = children(&supervisor).await.remove(0);
    let child = node.open(&child_id).await.unwrap();
    let grandchild_id = children(&child).await.remove(0);
    let grandchild = node.open(&grandchild_id).await.unwrap();
    let monitor_id = node.spawn_root("probe-v1", &bytes(json!({"type":"init"}))).await.unwrap();
    node.send(&monitor_id, "watch", &bytes(json!({"type":"watch","target":node.cap_for(&child_id, Rights::ALL).await.unwrap()})))
        .await
        .unwrap();
    drain(&node).await;
    let monitor = node.open(&monitor_id).await.unwrap();
    let links = child.sql("SELECT * FROM links ORDER BY peer", ()).await.unwrap().rows;
    let monitors = child.sql("SELECT * FROM monitors ORDER BY ref", ()).await.unwrap().rows;
    let monitored_by = child.sql("SELECT * FROM monitored_by ORDER BY ref", ()).await.unwrap().rows;
    let grandchild_cursor = grandchild.cursor().await.unwrap();
    let restarts = supervisor.sql("SELECT * FROM restarts", ()).await.unwrap().rows;

    // Hold the supervisor at an explicit fixture barrier while the real child traps.
    supervisor.sql("UPDATE meta SET value='parked' WHERE key='status'", ()).await.unwrap();
    node.send(&child_id, "poison", &bytes(json!({"type":"fail"}))).await.unwrap();
    let poison_seq: i64 = child.sql("SELECT seq FROM inbox WHERE key='poison'", ()).await.unwrap().rows[0].get(0).unwrap();
    drain(&node).await;
    assert_eq!(child.status().await.unwrap(), Status::Parked);
    assert_eq!(messages(&supervisor, "poison").await.len(), 1);
    assert!(messages(&monitor, "down").await.is_empty());
    node.promote(&child_id, "probe-fixed", "test", "repair poison before supervision").await.unwrap();
    supervisor.sql("UPDATE meta SET value='running' WHERE key='status'", ()).await.unwrap();
    drain(&node).await;

    let commands = supervisor.sql("SELECT msg FROM outbox WHERE target='spawn'", ()).await.unwrap();
    let resumes: Vec<Value> = commands
        .rows
        .iter()
        .map(|row| serde_json::from_slice(&row.get::<Vec<u8>>(0).unwrap()).unwrap())
        .filter(|command: &Value| command["verb"] == "resume" && command["id"] == child_id)
        .collect();
    assert_eq!(resumes.len(), 1, "the supervisor must actually emit Resume");
    assert_eq!(child.status().await.unwrap(), Status::Running);
    assert_eq!(child.cursor().await.unwrap(), poison_seq);
    let processed = child.sql("SELECT COUNT(*) FROM events WHERE seq=?", [poison_seq]).await.unwrap();
    assert_eq!(processed.rows[0].get::<i64>(0).unwrap(), 1);
    assert_eq!(grandchild.status().await.unwrap(), Status::Running);
    assert_eq!(grandchild.cursor().await.unwrap(), grandchild_cursor);
    assert!(messages(&monitor, "down").await.is_empty());
    assert_eq!(child.sql("SELECT * FROM links ORDER BY peer", ()).await.unwrap().rows, links);
    assert_eq!(child.sql("SELECT * FROM monitors ORDER BY ref", ()).await.unwrap().rows, monitors);
    assert_eq!(child.sql("SELECT * FROM monitored_by ORDER BY ref", ()).await.unwrap().rows, monitored_by);
    assert_eq!(supervisor.sql("SELECT * FROM restarts", ()).await.unwrap().rows, restarts);
    assert_eq!(supervisor.status().await.unwrap(), Status::Running);
    assert!(!dir.path().join(format!("{child_id}.reset.1.db")).exists());
}

struct AlternateSupervisor;

#[async_trait]
impl Behavior for AlternateSupervisor {
    fn hash(&self) -> &str {
        "alternate-supervisor"
    }

    fn child_type(&self) -> loom_actor::ChildType {
        loom_actor::ChildType::Supervisor
    }

    fn schema(&self) -> &str {
        loom_actor::Supervisor.schema()
    }

    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        let command: Value = serde_json::from_slice(msg).map_err(|error| Trap::new(error.to_string()))?;
        if command["type"] == "exit" {
            // A committed write proves the exit handler finished before shutdown.
            cx.sql("INSERT INTO graceful_exit(msg) VALUES (?)", [msg]).await?;
        }
        loom_actor::Supervisor.handle(cx, msg).await
    }
}

#[tokio::test]
async fn supervisor_typed_child_gets_infinite_shutdown() {
    use loom_actor::{ChildType, Shutdown};

    let dir = tempfile::tempdir().unwrap();
    let mut registry = Registry::new();
    registry.insert(AlternateSupervisor.hash().into(), Arc::new(AlternateSupervisor));
    let node = Node::new(dir.path(), Arc::new(registry), Arc::new(DefaultEffects), Config::default()).await.unwrap();
    let parent = node.root();
    let child = node.spawn_root(AlternateSupervisor.hash(), &bytes(json!({"type":"configure"}))).await.unwrap();
    let spec_rows = node.open(&parent).await.unwrap().sql("SELECT msg FROM outbox WHERE target='spawn'", ()).await.unwrap();
    let spawn: Value = serde_json::from_slice(&spec_rows.rows[0].get::<Vec<u8>>(0).unwrap()).unwrap();
    let spec: ChildSpec = serde_json::from_value(spawn["spec"].clone()).unwrap();
    assert_eq!(spec.child_type, ChildType::Supervisor);
    assert_eq!(spec.shutdown, Shutdown::Infinity);
    let actor = node.open(&child).await.unwrap();
    actor.sql("CREATE TABLE graceful_exit(msg BLOB)", ()).await.unwrap();
    drain(&node).await;

    node.stop(&parent, "shutdown").await.unwrap();
    drain(&node).await;

    let exits = actor.sql("SELECT msg FROM graceful_exit", ()).await.unwrap();
    assert_eq!(exits.rows.len(), 1);
    let exit: Value = serde_json::from_slice(&exits.rows[0].get::<Vec<u8>>(0).unwrap()).unwrap();
    assert_eq!(exit["type"], "exit");
    assert_eq!(exit["reason"], "shutdown");
    assert_eq!(actor.status().await.unwrap(), Status::Stopped);
    assert_eq!(meta(&actor, "reason").await, "shutdown");
    let error = node.spawn_root("unknown-supervisor", b"").await.unwrap_err();
    assert!(format!("{error:#}").contains("unknown test behavior unknown-supervisor"));
}
