use async_trait::async_trait;
use loom_actor::{Actor, Behavior, ChildSpec, ChildType, Config, Ctx, DefaultEffects, Durability, Node, Trap, view::Template};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path, sync::Arc, time::Duration};

pub struct Registry;
#[async_trait]
impl loom_actor::Registry for Registry {
    async fn behaviors(&self) -> anyhow::Result<Vec<loom_actor::builtin::BehaviorInfo>> {
        Ok(Vec::new())
    }
    async fn resolve(&self, hash: &str) -> anyhow::Result<Arc<dyn Behavior>> {
        match hash {
            "counter-v1" => Ok(Arc::new(loom_actor::builtin::Counter::plain())),
            "subscriber-test" => Ok(Arc::new(Subscriber)),
            _ => anyhow::bail!("unknown test behavior {hash}"),
        }
    }
    async fn template(&self, hash: &str) -> anyhow::Result<Arc<dyn Template>> {
        match hash {
            "template-a" | "template-b" | "template-sleep" => Ok(Arc::new(Render {
                hash: hash.to_owned(),
                effects: if hash == "template-sleep" { vec!["sleep".into()] } else { Vec::new() },
            })),
            _ => anyhow::bail!("unknown test template {hash}"),
        }
    }
}
struct Render {
    hash: String,
    effects: Vec<String>,
}
#[async_trait]
impl Template for Render {
    fn hash(&self) -> &str {
        &self.hash
    }
    fn effects(&self) -> &[String] {
        &self.effects
    }
    async fn render(&self, _cx: &mut Ctx<'_>, row: Value) -> Result<Value, Trap> {
        Ok(json!({"tag":"li","key":format!("row-{}",row["seq"]),
            "attrs":{"class":self.hash},"children":[row["seq"].to_string()]}))
    }
}
struct Subscriber;
#[async_trait]
impl Behavior for Subscriber {
    fn hash(&self) -> &str {
        "subscriber-test"
    }
    fn schema(&self) -> &str {
        "CREATE TABLE frames(body BLOB); CREATE TABLE saved(id TEXT)"
    }
    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        let value: Value = serde_json::from_slice(msg).map_err(|error| Trap::new(error.to_string()))?;
        if let Some(cap) = value.get("subscribe") {
            let cap = serde_json::from_value(cap.clone()).map_err(|error| Trap::new(error.to_string()))?;
            let id = cx.subscribe(&cap, "entries").await?;
            // The unsubscribe command removes both the runtime row and this fixture's saved identity.
            cx.sql("INSERT INTO saved VALUES (?)", [id]).await?;
        } else if value.get("unsubscribe").is_some() {
            let rows = cx.sql("SELECT id FROM saved", ()).await?;
            for row in rows.rows {
                let id: String = row.get(0).map_err(|error| Trap::new(error.to_string()))?;
                cx.unsubscribe(&id).await?;
            }
            cx.sql("DELETE FROM saved", ()).await?;
        } else if value.get("type").is_some() {
            cx.sql("INSERT INTO frames VALUES (?)", [msg]).await?;
        }
        Ok(())
    }
}
pub async fn node(path: &Path, config: Config) -> Node {
    Node::new(path, Arc::new(Registry), Arc::new(DefaultEffects), config).await.unwrap()
}
pub async fn drain(node: &Node) {
    tokio::time::timeout(Duration::from_secs(60), node.run_until_idle()).await.unwrap().unwrap();
}
pub async fn spawn(node: &Node, hash: &str, init: &[u8], durability: Durability) -> String {
    let mut spec = ChildSpec::new(hash, init, ChildType::Worker);
    spec.durability = durability;
    spec.link = false;
    // Fixtures are temporary children: a trap parks them and stays parked, so tests can
    // read the dead letter instead of racing the root supervisor's restart of a permanent child.
    spec.restart = loom_actor::RestartPolicy::Temporary;
    node.spawn(&node.root(), &spec).await.unwrap()
}
pub async fn command(node: &Node, id: &str, key: &str, body: &[u8]) {
    node.send(id, key, body).await.unwrap();
    drain(node).await;
}
pub async fn integer(actor: &Actor, sql: &str) -> i64 {
    actor.sql(sql, ()).await.unwrap().rows[0].get(0).unwrap()
}
pub async fn frames(node: &Node, id: &str) -> Vec<Value> {
    node.open(id)
        .await
        .unwrap()
        .sql("SELECT body FROM frames ORDER BY rowid", ())
        .await
        .unwrap()
        .rows
        .iter()
        .map(|row| serde_json::from_slice(&row.get::<Vec<u8>>(0).unwrap()).unwrap())
        .collect()
}
pub async fn next(stream: &mut loom_actor::HostStream) -> Value {
    let bytes = tokio::time::timeout(Duration::from_secs(10), stream.receiver.recv()).await.unwrap().unwrap();
    serde_json::from_slice(&bytes).unwrap()
}
pub async fn connection(path: &Path) -> turso::Connection {
    turso::Builder::new_local(path.to_str().unwrap()).build().await.unwrap().connect().unwrap()
}
pub async fn entries_hash(conn: &turso::Connection) -> String {
    let mut rows = conn.query("SELECT * FROM entries ORDER BY rowid", ()).await.unwrap();
    let mut hash = blake3::Hasher::new();
    while let Some(row) = rows.next().await.unwrap() {
        for column in 0..row.column_count() {
            let value = format!("{:?}", row.get_value(column).unwrap());
            hash.update(&(value.len() as u64).to_le_bytes());
            hash.update(value.as_bytes());
        }
    }
    hash.finalize().to_hex().to_string()
}
pub fn listing(path: &Path) -> BTreeSet<std::path::PathBuf> {
    std::fs::read_dir(path).unwrap().map(|entry| entry.unwrap().path()).collect()
}
pub async fn view_init(node: &Node, source: &str, template: &str) -> Vec<u8> {
    let cap = node.cap_for(source, loom_actor::Rights::INSPECT).await.unwrap();
    serde_json::to_vec(&json!({"source":cap,"table":"entries","template":template,"order_by":["seq"]})).unwrap()
}
