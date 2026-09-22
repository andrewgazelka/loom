mod args;
mod cluster;
mod inspection;
mod journal;
mod operations;
use crate::{Access, Scope};
use args::*;
use loom_actor::{Cap, ChildSpec, Node, Rights, Rows};
use serde_json::{Value, json};
#[derive(Clone)]
pub struct ActorService {
    pub(crate) node: Node,
}
fn error(error: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("{error:#}")
}
fn json_value(value: impl serde::Serialize) -> anyhow::Result<Value> {
    Ok(serde_json::to_value(value)?)
}
fn rows_json(rows: Rows) -> anyhow::Result<Value> {
    let mut result = Vec::new();
    for row in rows.rows {
        let mut object = serde_json::Map::new();
        for (index, name) in rows.columns.iter().enumerate() {
            let value = match row.get_value(index)? {
                loom_actor::Value::Null => Value::Null,
                loom_actor::Value::Integer(value) => json!(value),
                loom_actor::Value::Real(value) => json!(value),
                loom_actor::Value::Text(value) => json!(value),
                loom_actor::Value::Blob(value) => json!(value),
            };
            object.insert(name.clone(), value);
        }
        result.push(Value::Object(object));
    }
    Ok(json!(result))
}
fn sql_params(params: Vec<Value>) -> anyhow::Result<Vec<loom_actor::Value>> {
    params
        .into_iter()
        .map(|value| {
            Ok(match value {
                Value::Null => loom_actor::Value::Null,
                Value::Bool(value) => loom_actor::Value::Integer(i64::from(value)),
                Value::Number(value) if value.is_i64() => {
                    loom_actor::Value::Integer(value.as_i64().unwrap())
                }
                Value::Number(value) => loom_actor::Value::Real(
                    value
                        .as_f64()
                        .ok_or_else(|| anyhow::anyhow!("invalid SQL number"))?,
                ),
                Value::String(value) => loom_actor::Value::Text(value),
                _ => anyhow::bail!("SQL parameters must be null, boolean, number, or string"),
            })
        })
        .collect()
}
impl ActorService {
    pub fn new(node: Node) -> Self {
        Self { node }
    }
    async fn authority(
        &self,
        id: &str,
        rights: Rights,
        operation: &str,
    ) -> Result<Cap, anyhow::Error> {
        let cap = self.node.cap_for(id, rights).await.map_err(error)?;
        self.node
            .check_cap(&cap, rights, operation)
            .await
            .map_err(error)?;
        Ok(cap)
    }
    async fn tree(&self, root: &str) -> anyhow::Result<Value> {
        if let Some(value) = self.forward_command("tree", &json!({"root":root})).await? {
            return Ok(value);
        }
        self.authority(root, Rights::INSPECT, "tree")
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let info = self.node.info(root).await?;
        let mut children = Vec::new();
        for child in &info.children {
            children.push(Box::pin(self.tree(child)).await?);
        }
        Ok(
            json!({"id":root,"status":info.status,"behavior_hash":info.behavior_hash,"cursor":info.cursor,"children":children}),
        )
    }
    async fn rows(
        &self,
        operation: &str,
        id: &str,
        query: &str,
        params: Vec<Value>,
    ) -> Result<Value, anyhow::Error> {
        if let Some(value) = self
            .forward_command("sql", &json!({"id":id,"query":query,"params":params}))
            .await?
        {
            return Ok(value);
        }
        let cap = self.authority(id, Rights::INSPECT, operation).await?;
        let actor = self.node.open(&cap.target).await.map_err(error)?;
        let params = sql_params(params).map_err(|e| error(format!("actor {id} seq -1: {e}")))?;
        rows_json(actor.inspect_sql(query, params).await.map_err(error)?).map_err(error)
    }
    pub async fn resource(&self, uri: &str) -> anyhow::Result<Value> {
        let value = if uri == "actor://tree" {
            self.tree(&self.node.root()).await.map_err(error)?
        } else {
            let path = uri
                .strip_prefix("actor://")
                .ok_or_else(|| error("invalid actor URI"))?;
            let (id, kind) = path
                .split_once('/')
                .ok_or_else(|| error("invalid actor URI"))?;
            let query = match kind {
                "inbox" => "SELECT * FROM inbox ORDER BY seq",
                "effects" => "SELECT * FROM effects ORDER BY seq,idx",
                "outbox" => "SELECT * FROM outbox ORDER BY seq,idx",
                "lineage" => "SELECT * FROM code_changes ORDER BY seq",
                _ => return Err(error(format!("actor {id} seq -1: unknown resource {kind}"))),
            };
            self.rows(uri, id, query, vec![]).await?
        };
        Ok(value)
    }
}
impl ActorService {
    async fn command(&self, access: &Access, command: &str, args: Value) -> anyhow::Result<Value> {
        access.require(crate::auth::command_scope(command))?;
        if let Some(result) = self.forward_command(command, &args).await? {
            return Ok(result);
        }
        match command {
            "view" => {
                access.require(Scope::Execute)?;
                self.actor_view(serde_json::from_value(args)?).await
            }
            "subscriptions" => {
                access.require(Scope::Read)?;
                let args: IdArgs = serde_json::from_value(args)?;
                self.authority(&args.id, Rights::INSPECT, "subscriptions")
                    .await?;
                json_value(self.node.subscriptions(&args.id).await.map_err(error)?)
            }
            "nodes" => json_value(self.node.nodes().await.map_err(error)?),
            "move" => json_value(
                self.node
                    .move_actor(crate::field(&args, "id")?, crate::field(&args, "node_id")?)
                    .await
                    .map_err(error)?,
            ),
            "actors" => {
                access.require(Scope::Read)?;
                self.cluster_actor_list(args["cluster"].as_bool().unwrap_or(false))
                    .await
            }
            "tree" => {
                access.require(Scope::Read)?;
                self.actor_tree(serde_json::from_value(args)?).await
            }
            "info" => {
                access.require(Scope::Read)?;
                self.actor_info(serde_json::from_value(args)?).await
            }
            "send" => {
                access.require(Scope::Execute)?;
                self.actor_send(serde_json::from_value(args)?).await
            }
            "spawn" => {
                access.require(Scope::Execute)?;
                self.actor_spawn(serde_json::from_value(args)?).await
            }
            "stop" => {
                access.require(Scope::Execute)?;
                self.actor_stop(serde_json::from_value(args)?).await
            }
            "restart" => {
                access.require(Scope::Execute)?;
                self.actor_restart(serde_json::from_value(args)?).await
            }
            "promote" => {
                access.require(Scope::Define)?;
                self.actor_promote(serde_json::from_value(args)?).await
            }
            "promote_where" => {
                access.require(Scope::Define)?;
                self.actor_promote_where(serde_json::from_value(args)?)
                    .await
            }
            "lineage" => {
                access.require(Scope::Read)?;
                self.actor_lineage(serde_json::from_value(args)?).await
            }
            "dead_letters" => {
                access.require(Scope::Read)?;
                self.actor_dead_letters(serde_json::from_value(args)?).await
            }
            "fork" => {
                access.require(Scope::Execute)?;
                self.actor_fork(serde_json::from_value(args)?).await
            }
            "validate" => {
                access.require(Scope::Execute)?;
                self.actor_validate(serde_json::from_value(args)?).await
            }
            "sql" => {
                access.require(Scope::Read)?;
                self.actor_sql(serde_json::from_value(args)?).await
            }
            "whereis" => {
                access.require(Scope::Read)?;
                self.actor_whereis(serde_json::from_value(args)?).await
            }
            "register" => {
                access.require(Scope::Execute)?;
                self.actor_register(serde_json::from_value(args)?).await
            }
            "members" => {
                access.require(Scope::Read)?;
                self.actor_members(serde_json::from_value(args)?).await
            }
            "behaviors" => {
                access.require(Scope::Read)?;
                self.actor_behaviors().await
            }
            "drain" => {
                access.require(Scope::Execute)?;
                self.actor_run().await
            }
            _ => anyhow::bail!("unknown actor operation {command}"),
        }
    }
}

impl crate::Service {
    /// Resolve definition references once for every actor transport.
    pub(super) async fn actor_command(
        &self,
        command: &str,
        mut args: Value,
    ) -> anyhow::Result<Value> {
        use anyhow::Context;
        self.access
            .require(crate::auth::request_scope(command, &args))?;
        let actors = self
            .actors
            .as_ref()
            .context("actor node is not configured")?;
        let references: &[&str] = match command {
            "spawn" => &["def"],
            "view" => &["template"],
            "promote" => &["hash"],
            "validate" => &["candidate"],
            "promote_where" => &["old", "new"],
            _ => &[],
        };
        // The reference a spawn was addressed by, before it becomes a hash:
        // the journal keeps the name when the caller used one.
        let spawn_reference = if command == "spawn" {
            args["def"].as_str().map(str::to_owned)
        } else {
            None
        };
        for field in references {
            let target = crate::field(&args, field)?;
            if command == "spawn" && target == "view-v1" {
                continue;
            }
            if let Some(definition) = self.store.resolve(target)? {
                args[*field] = json!(definition.hash);
                continue;
            }
            let behavior = actors
                .node
                .behavior(target)
                .await
                .with_context(|| format!("actor definition reference {target:?}"))?;
            let hash = behavior.hash();
            args[*field] = json!(hash);
        }
        let result = actors.command(&self.access, command, args.clone()).await?;
        let name = spawn_reference.as_deref().filter(|reference| {
            !reference.starts_with('#') && Some(*reference) != args["def"].as_str()
        });
        journal::record_verb(&self.store, actors, command, &args, &result, name).await?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests;
