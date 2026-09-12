use crate::LoomMcp;
use loom_actor::{Cap, ChildSpec, Node, Rights, Rows};
use loom_api::Scope;
use rmcp::{
    ErrorData, RoleServer, handler::server::wrapper::Parameters, model::*, service::RequestContext,
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Clone)]
pub struct ActorMcp {
    node: Node,
}
fn error(error: impl std::fmt::Display) -> ErrorData {
    ErrorData::invalid_params(format!("{error:#}"), None)
}
fn json_text(value: impl serde::Serialize) -> Result<String, ErrorData> {
    serde_json::to_string(&value).map_err(error)
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
impl ActorMcp {
    pub fn new(node: Node) -> Self {
        Self { node }
    }
    async fn authority(&self, id: &str, rights: Rights, operation: &str) -> Result<Cap, ErrorData> {
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
    ) -> Result<Value, ErrorData> {
        let cap = self.authority(id, Rights::INSPECT, operation).await?;
        let actor = self.node.open(&cap.target).await.map_err(error)?;
        let params = sql_params(params).map_err(|e| error(format!("actor {id} seq -1: {e}")))?;
        rows_json(actor.inspect_sql(query, params).await.map_err(error)?).map_err(error)
    }
    pub(crate) async fn resource(&self, uri: &str) -> Result<ReadResourceResult, ErrorData> {
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
        Ok(ReadResourceResult {
            contents: vec![ResourceContents::text(value.to_string(), uri)],
        })
    }
}
impl LoomMcp {
    pub(crate) fn actor_access(
        &self,
        context: &RequestContext<RoleServer>,
        scope: Scope,
    ) -> Result<(), ErrorData> {
        context
            .extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<loom_api::Access>())
            .unwrap_or(&self.default_access)
            .require(scope)
            .map_err(error)
    }
}
#[derive(Deserialize, JsonSchema)]
pub struct IdArgs {
    id: String,
}
#[derive(Deserialize, JsonSchema)]
pub struct TreeArgs {
    root: Option<String>,
}
#[derive(Deserialize, JsonSchema)]
pub struct SendArgs {
    id: String,
    key: Option<String>,
    msg: Value,
}
#[derive(Deserialize, JsonSchema)]
pub struct SpawnArgs {
    behavior_hash: String,
    /// JSON initialization message; null creates an empty inbox.
    init: Value,
    parent: Option<String>,
    /// Optional restart, shutdown, link, monitor, and type fields from ChildSpec.
    spec: Option<Value>,
}
#[derive(Deserialize, JsonSchema)]
pub struct StopArgs {
    id: String,
    reason: String,
}
#[derive(Deserialize, JsonSchema)]
pub struct RestartArgs {
    id: String,
    /// One of resume, skip, reset.
    verb: String,
}
#[derive(Deserialize, JsonSchema)]
pub struct PromoteArgs {
    id: String,
    behavior_hash: String,
    author: String,
    rationale: String,
}
#[derive(Deserialize, JsonSchema)]
pub struct PromoteWhereArgs {
    old_hash: String,
    new_hash: String,
    author: String,
    rationale: String,
}
#[derive(Deserialize, JsonSchema)]
pub struct ForkArgs {
    id: String,
    at_seq: i64,
}
#[derive(Deserialize, JsonSchema)]
pub struct ValidateArgs {
    id: String,
    candidate_hash: String,
    k: i64,
    /// Read-only SQL evaluated on the candidate; one nonzero numeric scalar passes.
    assertions: Option<Vec<String>>,
}
#[derive(Deserialize, JsonSchema)]
pub struct SqlArgs {
    id: String,
    query: String,
    params: Option<Vec<Value>>,
}
#[derive(Deserialize, JsonSchema)]
pub struct NameArgs {
    name: String,
}
#[derive(Deserialize, JsonSchema)]
pub struct RegisterArgs {
    name: String,
    id: String,
}
#[derive(Deserialize, JsonSchema)]
pub struct GroupArgs {
    group: String,
}
#[tool_router(router = actor_tool_router, vis = "pub(crate)")]
impl LoomMcp {
    #[tool(name = "actor_list", description = "List every actor in this node.")]
    async fn actor_list(&self, context: RequestContext<RoleServer>) -> Result<String, ErrorData> {
        self.actor_access(&context, Scope::Read)?;
        let mut list = Vec::new();
        for id in self.actors.node.actor_ids().map_err(error)? {
            self.actors
                .authority(&id, Rights::INSPECT, "actor_list")
                .await?;
            let info = self.actors.node.info(&id).await.map_err(error)?;
            list.push(json!({"id":id,"status":info.status,"behavior_hash":info.behavior_hash,"cursor":info.cursor,"inbox_len":info.inbox_len,"parent":info.parent}));
        }
        json_text(list)
    }
    #[tool(name = "actor_tree", description = "Read the nested supervision tree.")]
    async fn actor_tree(
        &self,
        Parameters(args): Parameters<TreeArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        self.actor_access(&context, Scope::Read)?;
        json_text(
            self.actors
                .tree(&args.root.unwrap_or_else(|| self.actors.node.root()))
                .await
                .map_err(error)?,
        )
    }
    #[tool(
        name = "actor_info",
        description = "Inspect actor lifecycle, mailbox and relationships."
    )]
    async fn actor_info(
        &self,
        Parameters(args): Parameters<IdArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        self.actor_access(&context, Scope::Read)?;
        self.actors
            .authority(&args.id, Rights::INSPECT, "actor_info")
            .await?;
        json_text(self.actors.node.info(&args.id).await.map_err(error)?)
    }
    #[tool(
        name = "actor_send",
        description = "Inject a keyed JSON message and run the node until idle."
    )]
    async fn actor_send(
        &self,
        Parameters(args): Parameters<SendArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        self.actor_access(&context, Scope::Execute)?;
        self.actors
            .authority(&args.id, Rights::SEND, "actor_send")
            .await?;
        let key = args
            .key
            .unwrap_or_else(|| format!("mcp:{}", uuid::Uuid::new_v4()));
        self.actors
            .node
            .send(
                &args.id,
                &key,
                &serde_json::to_vec(&args.msg).map_err(error)?,
            )
            .await
            .map_err(error)?;
        self.actors.node.run_until_idle().await.map_err(error)?;
        json_text(json!({"cursor":self.actors.node.info(&args.id).await.map_err(error)?.cursor}))
    }
    #[tool(
        name = "actor_spawn",
        description = "Spawn a registered behavior under a supervisor."
    )]
    async fn actor_spawn(
        &self,
        Parameters(args): Parameters<SpawnArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        self.actor_access(&context, Scope::Execute)?;
        let parent = args.parent.unwrap_or_else(|| self.actors.node.root());
        self.actors
            .authority(&parent, Rights::SPAWN, "actor_spawn")
            .await?;
        let init = if args.init.is_null() {
            Vec::new()
        } else {
            serde_json::to_vec(&args.init).map_err(error)?
        };
        let mut spec = serde_json::to_value(ChildSpec::new(
            &args.behavior_hash,
            &init,
            self.actors
                .node
                .behavior(&args.behavior_hash)
                .map_err(error)?
                .child_type(),
        ))
        .map_err(error)?;
        if let Some(options) = args.spec {
            let options = options
                .as_object()
                .ok_or_else(|| error(format!("actor {parent} seq -1: spec must be an object")))?;
            if options.contains_key("type") && !options.contains_key("shutdown") {
                spec.as_object_mut()
                    .expect("serialized child spec")
                    .remove("shutdown");
            }
            for (key, value) in options {
                if matches!(key.as_str(), "behavior_hash" | "init") {
                    return Err(error(format!(
                        "actor {parent} seq -1: spec cannot override behavior_hash or init"
                    )));
                }
                if !matches!(
                    key.as_str(),
                    "restart" | "shutdown" | "link" | "monitor" | "type"
                ) {
                    return Err(error(format!(
                        "actor {parent} seq -1: unknown spec field {key}"
                    )));
                }
                spec[key] = value.clone();
            }
        }
        let spec: ChildSpec = serde_json::from_value(spec).map_err(error)?;
        let id = self
            .actors
            .node
            .spawn(&parent, &spec)
            .await
            .map_err(error)?;
        json_text(json!({"id":id}))
    }
    #[tool(name = "actor_stop", description = "Stop an actor with a reason.")]
    async fn actor_stop(
        &self,
        Parameters(args): Parameters<StopArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        self.actor_access(&context, Scope::Execute)?;
        self.actors
            .authority(&args.id, Rights::STOP, "actor_stop")
            .await?;
        self.actors
            .node
            .stop(&args.id, &args.reason)
            .await
            .map_err(error)?;
        json_text(json!({"id":args.id}))
    }
    #[tool(
        name = "actor_restart",
        description = "Restart an actor using resume, skip, or reset."
    )]
    async fn actor_restart(
        &self,
        Parameters(args): Parameters<RestartArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        self.actor_access(&context, Scope::Execute)?;
        self.actors
            .authority(&args.id, Rights::SPAWN, "actor_restart")
            .await?;
        let verb = serde_json::from_value(json!(args.verb))
            .map_err(|e| error(format!("actor {} seq -1: {e}", args.id)))?;
        self.actors
            .node
            .restart(&args.id, verb)
            .await
            .map_err(error)?;
        json_text(json!({"id":args.id}))
    }
    #[tool(
        name = "actor_promote",
        description = "Promote an actor and return its new lineage row."
    )]
    async fn actor_promote(
        &self,
        Parameters(args): Parameters<PromoteArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        self.actor_access(&context, Scope::Define)?;
        self.actors
            .authority(&args.id, Rights::PROMOTE, "actor_promote")
            .await?;
        self.actors
            .node
            .promote(&args.id, &args.behavior_hash, &args.author, &args.rationale)
            .await
            .map_err(error)?;
        let rows = self
            .actors
            .rows(
                "actor_promote",
                &args.id,
                "SELECT * FROM code_changes ORDER BY seq DESC LIMIT 1",
                vec![],
            )
            .await?;
        json_text(&rows[0])
    }
    #[tool(
        name = "actor_promote_where",
        description = "Promote all live actors using a behavior hash."
    )]
    async fn actor_promote_where(
        &self,
        Parameters(args): Parameters<PromoteWhereArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        self.actor_access(&context, Scope::Define)?;
        self.actors.node.behavior(&args.new_hash).map_err(error)?;
        for id in self.actors.node.actor_ids().map_err(error)? {
            let info = self.actors.node.info(&id).await.map_err(error)?;
            if info.behavior_hash == args.old_hash && info.status != loom_actor::Status::Stopped {
                self.actors
                    .authority(&id, Rights::PROMOTE, "actor_promote_where")
                    .await?;
            }
        }
        json_text(
            self.actors
                .node
                .promote_where(
                    &args.old_hash,
                    &args.new_hash,
                    &args.author,
                    &args.rationale,
                )
                .await
                .map_err(error)?,
        )
    }
    #[tool(name = "actor_lineage", description = "Inspect actor history rows.")]
    async fn actor_lineage(
        &self,
        Parameters(args): Parameters<IdArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        self.actor_access(&context, Scope::Read)?;
        json_text(
            self.actors
                .rows(
                    "actor_lineage",
                    &args.id,
                    "SELECT * FROM code_changes ORDER BY seq",
                    vec![],
                )
                .await?,
        )
    }
    #[tool(
        name = "actor_dead_letters",
        description = "Inspect actor history rows."
    )]
    async fn actor_dead_letters(
        &self,
        Parameters(args): Parameters<IdArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        self.actor_access(&context, Scope::Read)?;
        json_text(
            self.actors
                .rows(
                    "actor_dead_letters",
                    &args.id,
                    "SELECT * FROM dead_letters ORDER BY seq",
                    vec![],
                )
                .await?,
        )
    }
    #[tool(
        name = "actor_fork",
        description = "Fork an actor at a historical sequence."
    )]
    async fn actor_fork(
        &self,
        Parameters(args): Parameters<ForkArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        self.actor_access(&context, Scope::Execute)?;
        self.actors
            .authority(&args.id, Rights::INSPECT, "actor_fork")
            .await?;
        json_text(json!({"id":self.actors.node.fork(&args.id,args.at_seq).await.map_err(error)?}))
    }
    #[tool(
        name = "actor_validate",
        description = "Replay a candidate and evaluate read-only SQL assertions on it."
    )]
    async fn actor_validate(
        &self,
        Parameters(args): Parameters<ValidateArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        self.actor_access(&context, Scope::Execute)?;
        self.actors
            .authority(&args.id, Rights::INSPECT, "actor_validate")
            .await?;
        json_text(
            self.actors
                .node
                .validate_assertions(
                    &args.id,
                    &args.candidate_hash,
                    args.k,
                    &args.assertions.unwrap_or_default(),
                )
                .await
                .map_err(error)?,
        )
    }
    #[tool(
        name = "actor_sql",
        description = "Run a single read-only SQL inspection; writes are refused."
    )]
    async fn actor_sql(
        &self,
        Parameters(args): Parameters<SqlArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        self.actor_access(&context, Scope::Read)?;
        json_text(
            self.actors
                .rows(
                    "actor_sql",
                    &args.id,
                    &args.query,
                    args.params.unwrap_or_default(),
                )
                .await?,
        )
    }
    #[tool(
        name = "actor_whereis",
        description = "Resolve a registered actor name."
    )]
    async fn actor_whereis(
        &self,
        Parameters(args): Parameters<NameArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        self.actor_access(&context, Scope::Read)?;
        json_text(self.actors.node.whereis(&args.name).await.map_err(error)?)
    }
    #[tool(name = "actor_register", description = "Register a unique actor name.")]
    async fn actor_register(
        &self,
        Parameters(args): Parameters<RegisterArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        self.actor_access(&context, Scope::Execute)?;
        self.actors
            .authority(&args.id, Rights::INSPECT, "actor_register")
            .await?;
        self.actors
            .node
            .register(&args.name, &args.id)
            .await
            .map_err(error)?;
        json_text(json!({"id":args.id,"name":args.name}))
    }
    #[tool(name = "actor_members", description = "List actors in a group.")]
    async fn actor_members(
        &self,
        Parameters(args): Parameters<GroupArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        self.actor_access(&context, Scope::Read)?;
        json_text(self.actors.node.members(&args.group).await.map_err(error)?)
    }
    #[tool(
        name = "actor_behaviors",
        description = "List registered behavior hashes and descriptions."
    )]
    async fn actor_behaviors(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        self.actor_access(&context, Scope::Read)?;
        json_text(self.actors.node.behaviors())
    }
    #[tool(
        name = "actor_run",
        description = "Run the actor network until idle and count processed messages."
    )]
    async fn actor_run(&self, context: RequestContext<RoleServer>) -> Result<String, ErrorData> {
        self.actor_access(&context, Scope::Execute)?;
        json_text(json!({"processed":self.actors.node.run_until_idle().await.map_err(error)?}))
    }
}
