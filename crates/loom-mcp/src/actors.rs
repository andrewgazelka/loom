use crate::LoomMcp;
use loom_actor::Node;
use loom_api::Scope;
use rmcp::{
    ErrorData, RoleServer, handler::server::wrapper::Parameters, model::*, service::RequestContext,
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[derive(Clone)]
pub struct ActorMcp {
    service: loom_api::actors::ActorService,
}
fn error(error: impl std::fmt::Display) -> ErrorData {
    ErrorData::invalid_params(format!("{error:#}"), None)
}
fn json_text(value: impl serde::Serialize) -> Result<String, ErrorData> {
    serde_json::to_string(&value).map_err(error)
}
impl ActorMcp {
    pub fn new(node: Node) -> Self {
        Self {
            service: loom_api::actors::ActorService::new(node),
        }
    }
    pub(crate) async fn resource(&self, uri: &str) -> Result<ReadResourceResult, ErrorData> {
        let value = self.service.resource(uri).await.map_err(error)?;
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
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct IdArgs {
    id: String,
}
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct TreeArgs {
    root: Option<String>,
}
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct SendArgs {
    id: String,
    key: Option<String>,
    msg: Value,
}
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct SpawnArgs {
    behavior_hash: String,
    /// JSON initialization message; null creates an empty inbox.
    init: Value,
    parent: Option<String>,
    /// Optional restart, shutdown, link, monitor, and type fields from ChildSpec.
    spec: Option<Value>,
}
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct StopArgs {
    id: String,
    reason: String,
}
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct RestartArgs {
    id: String,
    /// One of resume, skip, reset.
    verb: String,
}
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct PromoteArgs {
    id: String,
    behavior_hash: String,
    author: String,
    rationale: String,
}
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct PromoteWhereArgs {
    old_hash: String,
    new_hash: String,
    author: String,
    rationale: String,
}
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct ForkArgs {
    id: String,
    at_seq: i64,
}
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct ValidateArgs {
    id: String,
    candidate_hash: String,
    k: i64,
    /// Read-only SQL evaluated on the candidate; one nonzero numeric scalar passes.
    assertions: Option<Vec<String>>,
}
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct SqlArgs {
    id: String,
    query: String,
    params: Option<Vec<Value>>,
}
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct NameArgs {
    name: String,
}
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct RegisterArgs {
    name: String,
    id: String,
}
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct GroupArgs {
    group: String,
}
#[tool_router(router = actor_tool_router, vis = "pub(crate)")]
impl LoomMcp {
    #[tool(name = "actor_list", description = "List every actor in this node.")]
    async fn actor_list(&self, context: RequestContext<RoleServer>) -> Result<String, ErrorData> {
        self.actor_access(&context, Scope::Read)?;
        let value = self
            .service_for(&context)
            .actor_command("actor_list", json!({}))
            .await
            .map_err(error)?;
        json_text(value)
    }
    #[tool(name = "actor_tree", description = "Read the nested supervision tree.")]
    async fn actor_tree(
        &self,
        Parameters(args): Parameters<TreeArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        self.actor_access(&context, Scope::Read)?;
        let value = self
            .service_for(&context)
            .actor_command("actor_tree", serde_json::to_value(args).map_err(error)?)
            .await
            .map_err(error)?;
        json_text(value)
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
        let value = self
            .service_for(&context)
            .actor_command("actor_info", serde_json::to_value(args).map_err(error)?)
            .await
            .map_err(error)?;
        json_text(value)
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
        let value = self
            .service_for(&context)
            .actor_command("actor_send", serde_json::to_value(args).map_err(error)?)
            .await
            .map_err(error)?;
        json_text(value)
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
        let value = self
            .service_for(&context)
            .actor_command("actor_spawn", serde_json::to_value(args).map_err(error)?)
            .await
            .map_err(error)?;
        json_text(value)
    }
    #[tool(name = "actor_stop", description = "Stop an actor with a reason.")]
    async fn actor_stop(
        &self,
        Parameters(args): Parameters<StopArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        self.actor_access(&context, Scope::Execute)?;
        let value = self
            .service_for(&context)
            .actor_command("actor_stop", serde_json::to_value(args).map_err(error)?)
            .await
            .map_err(error)?;
        json_text(value)
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
        let value = self
            .service_for(&context)
            .actor_command("actor_restart", serde_json::to_value(args).map_err(error)?)
            .await
            .map_err(error)?;
        json_text(value)
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
        let value = self
            .service_for(&context)
            .actor_command("actor_promote", serde_json::to_value(args).map_err(error)?)
            .await
            .map_err(error)?;
        json_text(value)
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
        let value = self
            .service_for(&context)
            .actor_command(
                "actor_promote_where",
                serde_json::to_value(args).map_err(error)?,
            )
            .await
            .map_err(error)?;
        json_text(value)
    }
    #[tool(name = "actor_lineage", description = "Inspect actor history rows.")]
    async fn actor_lineage(
        &self,
        Parameters(args): Parameters<IdArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        self.actor_access(&context, Scope::Read)?;
        let value = self
            .service_for(&context)
            .actor_command("actor_lineage", serde_json::to_value(args).map_err(error)?)
            .await
            .map_err(error)?;
        json_text(value)
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
        let value = self
            .service_for(&context)
            .actor_command(
                "actor_dead_letters",
                serde_json::to_value(args).map_err(error)?,
            )
            .await
            .map_err(error)?;
        json_text(value)
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
        let value = self
            .service_for(&context)
            .actor_command("actor_fork", serde_json::to_value(args).map_err(error)?)
            .await
            .map_err(error)?;
        json_text(value)
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
        let value = self
            .service_for(&context)
            .actor_command("actor_validate", serde_json::to_value(args).map_err(error)?)
            .await
            .map_err(error)?;
        json_text(value)
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
        let value = self
            .service_for(&context)
            .actor_command("actor_sql", serde_json::to_value(args).map_err(error)?)
            .await
            .map_err(error)?;
        json_text(value)
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
        let value = self
            .service_for(&context)
            .actor_command("actor_whereis", serde_json::to_value(args).map_err(error)?)
            .await
            .map_err(error)?;
        json_text(value)
    }
    #[tool(name = "actor_register", description = "Register a unique actor name.")]
    async fn actor_register(
        &self,
        Parameters(args): Parameters<RegisterArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        self.actor_access(&context, Scope::Execute)?;
        let value = self
            .service_for(&context)
            .actor_command("actor_register", serde_json::to_value(args).map_err(error)?)
            .await
            .map_err(error)?;
        json_text(value)
    }
    #[tool(name = "actor_members", description = "List actors in a group.")]
    async fn actor_members(
        &self,
        Parameters(args): Parameters<GroupArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        self.actor_access(&context, Scope::Read)?;
        let value = self
            .service_for(&context)
            .actor_command("actor_members", serde_json::to_value(args).map_err(error)?)
            .await
            .map_err(error)?;
        json_text(value)
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
        let value = self
            .service_for(&context)
            .actor_command("actor_behaviors", json!({}))
            .await
            .map_err(error)?;
        json_text(value)
    }
    #[tool(
        name = "actor_run",
        description = "Run the actor network until idle and count processed messages."
    )]
    async fn actor_run(&self, context: RequestContext<RoleServer>) -> Result<String, ErrorData> {
        self.actor_access(&context, Scope::Execute)?;
        let value = self
            .service_for(&context)
            .actor_command("actor_run", json!({}))
            .await
            .map_err(error)?;
        json_text(value)
    }
}
