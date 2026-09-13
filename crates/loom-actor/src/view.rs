//! A view materializes pure template results from the ordinary subscription inbox.
use crate::{Behavior, Cap, ChildSpec, Ctx, Registry, Trap, actor};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};
use turso::Connection;

pub const HASH: &str = "view-v1";

/// Implementations report the inferred row and must not perform effects while rendering.
#[async_trait]
pub trait Template: Send + Sync {
    fn hash(&self) -> &str;
    fn effects(&self) -> &[String];
    async fn render(&self, cx: &mut Ctx<'_>, row: Value) -> Result<Value, Trap>;
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewInit {
    pub source: Cap,
    pub table: String,
    pub template: String,
    #[serde(default)]
    pub order_by: Vec<String>,
}

impl ViewInit {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let init: Self = serde_json::from_slice(bytes).context("view-v1 init")?;
        ensure!(!init.table.is_empty(), "view-v1 init: table is empty");
        ensure!(!init.template.is_empty(), "view-v1 init: template is empty");
        ensure!(init.order_by.iter().all(|name| !name.is_empty()), "view-v1 init: order_by contains an empty column");
        Ok(init)
    }
}

pub struct View {
    template: Arc<dyn Template>,
}

impl View {
    pub fn new(template: Arc<dyn Template>) -> Result<Self> {
        if let Some(effect) = template.effects().first() {
            anyhow::bail!("view-v1 template {}: forbidden effect {effect}", template.hash());
        }
        Ok(Self { template })
    }

    async fn render_row(&self, cx: &mut Ctx<'_>, init: &ViewInit, key: &str, row: &Value) -> Result<(), Trap> {
        let tree = self.template.render(cx, row.clone()).await?;
        validate_tree(&tree).map_err(|error| fail(cx, format!("template {}: {error:#}", self.hash())))?;
        let mut sort = Vec::new();
        for name in &init.order_by {
            sort.push(row.get(name).cloned().ok_or_else(|| fail(cx, format!("order_by column {name:?} is absent")))?);
        }
        let sort = serde_json::to_vec(&sort).map_err(|error| cx.runtime(error))?;
        let tree = serde_json::to_vec(&tree).map_err(|error| cx.runtime(error))?;
        cx.sql(
            "INSERT INTO tree(key,sort,tree) VALUES (?,?,?) ON CONFLICT(key) DO UPDATE SET sort=excluded.sort,tree=excluded.tree",
            turso::params![key, sort, tree],
        )
        .await?;
        Ok(())
    }
}

/// Creation resolves the template before creating a file or recording the parent's spawn.
pub async fn from_spec(registry: &Arc<dyn Registry>, spec: &ChildSpec) -> Result<Option<Arc<dyn Behavior>>> {
    if spec.behavior_hash != HASH {
        return Ok(None);
    }
    let init = ViewInit::parse(&spec.init)?;
    Ok(Some(Arc::new(View::new(registry.template(&init.template).await?)?)))
}

pub(crate) async fn pin_spec(registry: &Arc<dyn Registry>, spec: &ChildSpec) -> Result<ChildSpec> {
    let mut pinned = spec.clone();
    if let Some(behavior) = from_spec(registry, spec).await? {
        let mut init = ViewInit::parse(&spec.init)?;
        init.template = behavior.hash().to_owned();
        pinned.init = serde_json::to_vec(&init)?;
        pinned.durability = crate::Durability::Ephemeral;
        pinned.restart = crate::RestartPolicy::Temporary;
    } else {
        pinned.behavior_hash = actor::behavior(registry, &spec.behavior_hash).await?.hash().to_owned();
    }
    Ok(pinned)
}

/// The marker is installed by schema before snapshot zero, including during replay.
pub(crate) async fn behavior_on(registry: &Arc<dyn Registry>, conn: &Connection, hash: &str) -> Result<Arc<dyn Behavior>> {
    let marker = actor::query(conn, "SELECT value FROM meta WHERE key='view_kind'", ()).await?;
    if let Some(row) = marker.rows.first() {
        let id = actor::meta(conn, "id").await?;
        let seq = actor::cursor(conn).await?;
        ensure!(row.get::<String>(0)? == HASH, "actor {id} seq {seq}: view_kind flag has an unsupported value");
        let template = registry.template(hash).await.with_context(|| format!("actor {id} seq {seq}: view template"))?;
        return Ok(Arc::new(View::new(template).with_context(|| format!("actor {id} seq {seq}: view template"))?));
    }
    actor::behavior(registry, hash).await
}

#[derive(Deserialize)]
struct ChangedRow {
    id: i64,
    #[serde(default)]
    change_type: Option<i64>,
    #[serde(default)]
    table: Option<String>,
    after: Option<Value>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Frame {
    Snapshot { source: String, rows: Vec<ChangedRow> },
    Delta { source: String, cause: Value, rows: Vec<ChangedRow> },
    Resnapshot { source: String },
}

fn fail(cx: &Ctx<'_>, message: impl std::fmt::Display) -> Trap {
    Trap::new(format!("actor {} seq {}: view-v1: {message}", cx.self_id(), cx.seq()))
}

async fn save(cx: &mut Ctx<'_>, key: &str, value: impl Serialize) -> Result<(), Trap> {
    let value = serde_json::to_string(&value).map_err(|error| cx.runtime(error))?;
    actor::set_meta(cx.conn, key, &value).await.map_err(|error| cx.runtime(error))
}

async fn load<T: serde::de::DeserializeOwned>(cx: &mut Ctx<'_>, key: &str) -> Result<T, Trap> {
    let text = actor::meta(cx.conn, key).await.map_err(|error| cx.runtime(error))?;
    serde_json::from_str(&text).map_err(|error| fail(cx, format!("meta {key}: {error}")))
}

#[async_trait]
impl Behavior for View {
    fn hash(&self) -> &str {
        self.template.hash()
    }

    fn description(&self) -> &str {
        "Pure keyed trees materialized from an actor table."
    }

    fn schema(&self) -> &str {
        // View metadata leaves with its ephemeral actor; rows leave on delete/snapshot.
        "CREATE TABLE IF NOT EXISTS tree(key TEXT PRIMARY KEY, sort BLOB, tree BLOB);
         INSERT OR IGNORE INTO meta(key,value) VALUES ('view_kind','view-v1');
         INSERT OR IGNORE INTO meta(key,value) VALUES ('view_rows','{}');
         INSERT OR IGNORE INTO meta(key,value) VALUES ('view_resnapshot','false');"
    }

    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        let initialized = cx.trusted_sql("SELECT value FROM meta WHERE key='view_init'", ()).await?;
        if initialized.rows.is_empty() {
            let mut init = ViewInit::parse(msg).map_err(|error| fail(cx, error))?;
            init.template = self.hash().to_owned();
            let subscription = cx.subscribe(&init.source, &init.table).await?;
            // terminate queues unsubscribe; the pump removes the target's subscriber row.
            save(cx, "view_subscription", subscription).await?;
            save(cx, "view_init", init).await?;
            return Ok(());
        }
        let init: ViewInit = load(cx, "view_init").await?;
        let frame: Frame = serde_json::from_slice(msg).map_err(|error| fail(cx, format!("frame: {error}")))?;
        if let Frame::Delta { cause, .. } = &frame
            && !cause.is_null()
            && !cause.is_string()
        {
            return Err(fail(cx, "delta cause must be a string or null"));
        }
        // record_origin sets our tree delta's cause to this input delta's key,
        // never to its cause: source -> view -> browser is the correlation bound.
        let source = match &frame {
            Frame::Snapshot { source, .. } | Frame::Delta { source, .. } | Frame::Resnapshot { source } => source,
        };
        if source != &init.source.target {
            return Err(fail(cx, format!("frame source {source:?} differs from {}", init.source.target)));
        }
        if cx.sender().as_deref() != Some(source.as_str()) {
            return Err(fail(cx, format!("frame sender {:?} is not source {source}", cx.sender())));
        }
        if matches!(&frame, Frame::Resnapshot { .. }) {
            // Only a full snapshot clears this barrier; deltas cannot cross a known gap.
            return save(cx, "view_resnapshot", true).await;
        }
        let snapshot = matches!(&frame, Frame::Snapshot { .. });
        if !snapshot && load::<bool>(cx, "view_resnapshot").await? {
            return Err(fail(cx, "delta received while view_resnapshot flag is set"));
        }
        let changes = match frame {
            Frame::Snapshot { rows, .. } | Frame::Delta { rows, .. } => rows,
            Frame::Resnapshot { .. } => unreachable!("resnapshot returned above"),
        };
        let previous: BTreeMap<String, Value> = load(cx, "view_rows").await?;
        let mut rows = if snapshot { BTreeMap::new() } else { previous.clone() };
        for change in changes {
            if !snapshot && change.table.is_none() {
                return Err(fail(cx, "delta row is missing table"));
            }
            if change.table.as_ref().is_some_and(|table| table != &init.table) {
                return Err(fail(cx, format!("frame table {:?} differs from {}", change.table, init.table)));
            }
            let key = change.id.to_string();
            let operation = if snapshot { 1 } else { change.change_type.ok_or_else(|| fail(cx, "delta missing change_type"))? };
            match operation {
                -1 => {
                    rows.remove(&key);
                    cx.sql("DELETE FROM tree WHERE key=?", [key]).await?;
                }
                0 | 1 => {
                    let row = change.after.filter(Value::is_object).ok_or_else(|| fail(cx, "row after must be an object"))?;
                    self.render_row(cx, &init, &key, &row).await?;
                    rows.insert(key, row);
                }
                other => return Err(fail(cx, format!("unsupported change_type flag {other}"))),
            }
        }
        if snapshot {
            for key in previous.keys().filter(|key| !rows.contains_key(*key)) {
                cx.sql("DELETE FROM tree WHERE key=?", [key.as_str()]).await?;
            }
            save(cx, "view_resnapshot", false).await?;
        }
        save(cx, "view_rows", rows).await
    }

    async fn upgrade(&self, cx: &mut Ctx<'_>, _from_hash: &str) -> Result<(), Trap> {
        let initialized = cx.trusted_sql("SELECT value FROM meta WHERE key='view_init'", ()).await?;
        if initialized.rows.is_empty() {
            return Ok(());
        }
        let mut init: ViewInit = load(cx, "view_init").await?;
        let rows: BTreeMap<String, Value> = load(cx, "view_rows").await?;
        for (key, row) in rows {
            self.render_row(cx, &init, &key, &row).await?;
        }
        init.template = self.hash().to_owned();
        save(cx, "view_init", init).await
    }

    async fn terminate(&self, cx: &mut Ctx<'_>, _reason: &str) -> Result<(), Trap> {
        let rows = cx.trusted_sql("SELECT value FROM meta WHERE key='view_subscription'", ()).await?;
        if let Some(row) = rows.rows.first() {
            let value: String = row.get(0).map_err(|error| cx.runtime(error))?;
            let subscription: String = serde_json::from_str(&value).map_err(|error| fail(cx, error))?;
            cx.unsubscribe(&subscription).await?;
            cx.trusted_sql("DELETE FROM meta WHERE key='view_subscription'", ()).await?;
        }
        Ok(())
    }
}

fn validate_tree(tree: &Value) -> Result<()> {
    let node = tree.as_object().context("tree node must be an object")?;
    ensure!(node.get("tag").and_then(Value::as_str).is_some_and(|tag| !tag.is_empty()), "tree tag is required");
    if let Some(attrs) = node.get("attrs") {
        ensure!(attrs.is_object(), "tree attrs must be an object");
    }
    if let Some(children) = node.get("children") {
        let children = children.as_array().context("tree children must be an array")?;
        let mut keys = std::collections::BTreeSet::new();
        for child in children {
            if child.is_string() {
                continue;
            }
            validate_tree(child)?;
            if child.get("tag").and_then(Value::as_str) == Some("li") {
                ensure!(child.get("key").and_then(Value::as_str).is_some(), "list item key is required");
            }
            if let Some(key) = child.get("key") {
                let key = key.as_str().context("tree key must be a string")?;
                ensure!(keys.insert(key), "duplicate child key {key:?}");
            }
        }
    }
    Ok(())
}
