mod args;
mod inspection;
mod operations;
use crate::{Access, Scope};
use args::*;
use loom_actor::{Cap, ChildSpec, Node, Rights, Rows};
use serde_json::{Value, json};
#[derive(Clone)]
pub struct ActorService {
    node: Node,
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
        self.authority(root, Rights::INSPECT, "actor_tree")
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let entries = self.node.tree(root).await?;
        let mut nodes = std::collections::HashMap::new();
        for entry in entries.iter().rev() {
            let info = self.node.info(&entry.id).await?;
            let children: Vec<Value> = info
                .children
                .iter()
                .filter_map(|id| nodes.remove(id))
                .collect();
            nodes.insert(entry.id.clone(), json!({"id":entry.id,"status":entry.status,"behavior_hash":entry.behavior_hash,"cursor":info.cursor,"children":children}));
        }
        nodes
            .remove(root)
            .ok_or_else(|| anyhow::anyhow!("actor {root} seq -1: missing tree root"))
    }
    async fn rows(
        &self,
        operation: &str,
        id: &str,
        query: &str,
        params: Vec<Value>,
    ) -> Result<Value, anyhow::Error> {
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
    pub async fn command(
        &self,
        access: &Access,
        command: &str,
        args: Value,
    ) -> anyhow::Result<Value> {
        match command {
            "actor_list" => {
                access.require(Scope::Read)?;
                self.actor_list().await
            }
            "actor_tree" => {
                access.require(Scope::Read)?;
                self.actor_tree(serde_json::from_value(args)?).await
            }
            "actor_info" => {
                access.require(Scope::Read)?;
                self.actor_info(serde_json::from_value(args)?).await
            }
            "actor_send" => {
                access.require(Scope::Execute)?;
                self.actor_send(serde_json::from_value(args)?).await
            }
            "actor_spawn" => {
                access.require(Scope::Execute)?;
                self.actor_spawn(serde_json::from_value(args)?).await
            }
            "actor_stop" => {
                access.require(Scope::Execute)?;
                self.actor_stop(serde_json::from_value(args)?).await
            }
            "actor_restart" => {
                access.require(Scope::Execute)?;
                self.actor_restart(serde_json::from_value(args)?).await
            }
            "actor_promote" => {
                access.require(Scope::Define)?;
                self.actor_promote(serde_json::from_value(args)?).await
            }
            "actor_promote_where" => {
                access.require(Scope::Define)?;
                self.actor_promote_where(serde_json::from_value(args)?)
                    .await
            }
            "actor_lineage" => {
                access.require(Scope::Read)?;
                self.actor_lineage(serde_json::from_value(args)?).await
            }
            "actor_dead_letters" => {
                access.require(Scope::Read)?;
                self.actor_dead_letters(serde_json::from_value(args)?).await
            }
            "actor_fork" => {
                access.require(Scope::Execute)?;
                self.actor_fork(serde_json::from_value(args)?).await
            }
            "actor_validate" => {
                access.require(Scope::Execute)?;
                self.actor_validate(serde_json::from_value(args)?).await
            }
            "actor_sql" => {
                access.require(Scope::Read)?;
                self.actor_sql(serde_json::from_value(args)?).await
            }
            "actor_whereis" => {
                access.require(Scope::Read)?;
                self.actor_whereis(serde_json::from_value(args)?).await
            }
            "actor_register" => {
                access.require(Scope::Execute)?;
                self.actor_register(serde_json::from_value(args)?).await
            }
            "actor_members" => {
                access.require(Scope::Read)?;
                self.actor_members(serde_json::from_value(args)?).await
            }
            "actor_behaviors" => {
                access.require(Scope::Read)?;
                self.actor_behaviors().await
            }
            "actor_run" => {
                access.require(Scope::Execute)?;
                self.actor_run().await
            }
            _ => anyhow::bail!("unknown actor operation {command}"),
        }
    }
}

impl crate::Service {
    /// Resolve definition references once for every actor transport.
    pub async fn actor_command(&self, command: &str, mut args: Value) -> anyhow::Result<Value> {
        use anyhow::Context;
        self.access.require(crate::auth::command_scope(command))?;
        let actors = self
            .actors
            .as_ref()
            .context("actor node is not configured")?;
        let references: &[&str] = match command {
            "actor_spawn" | "actor_promote" => &["behavior_hash"],
            "actor_validate" => &["candidate_hash"],
            "actor_promote_where" => &["old_hash", "new_hash"],
            _ => &[],
        };
        for field in references {
            let target = crate::field(&args, field)?;
            let hash = match self.store.resolve(target)? {
                Some(definition) => definition.hash,
                None => target.to_owned(),
            };
            actors.node.behavior(&hash).with_context(|| {
                format!("definition reference {target:?} resolves to unregistered behavior {hash}")
            })?;
            args[*field] = json!(hash);
        }
        actors.command(&self.access, command, args).await
    }
}

#[cfg(test)]
mod tests;
