mod actors;
pub use actors::ActorMcp;
use loom_api::{Access, Service};
use loom_proto::CommandRequest;
use rmcp::service::RequestContext;
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{tool::ToolRouter, wrapper::Parameters},
    model::*,
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone)]
pub struct LoomMcp {
    service: Arc<Service>,
    actors: ActorMcp,
    session: String,
    default_access: Access,
    tool_router: ToolRouter<Self>,
}
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct AddArgs {
    source: String,
    name: Option<String>,
    allowed_effects: Option<Vec<String>>,
    #[serde(default)]
    deps: std::collections::BTreeMap<String, String>,
}
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct UpdateArgs {
    name: String,
    source: String,
    #[serde(
        default,
        deserialize_with = "present_effects",
        skip_serializing_if = "Option::is_none"
    )]
    allowed_effects: Option<Option<Vec<String>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    deps: Option<std::collections::BTreeMap<String, String>>,
}
fn present_effects<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Option<Vec<String>>>, D::Error> {
    Option::<Vec<String>>::deserialize(deserializer).map(Some)
}
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct TargetArgs {
    target: String,
}
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct RunArgs {
    target: String,
    #[serde(default = "empty_args")]
    args: serde_json::Value,
}
fn empty_args() -> serde_json::Value {
    serde_json::json!([])
}
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct NameArgs {
    name: String,
}
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct DiffArgs {
    old: String,
    new: String,
}
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct FindArgs {
    text: String,
}
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct HashArgs {
    hash: String,
}
#[derive(Deserialize, JsonSchema)]
pub struct CommandArgs {
    session: Option<String>,
    command: String,
    #[serde(default)]
    args: serde_json::Value,
}
#[tool_router]
impl LoomMcp {
    pub fn new(service: Arc<Service>, default_access: Access, node: loom_actor::Node) -> Self {
        Self {
            service: Arc::new(service.as_ref().clone().with_actors(node.clone())),
            actors: ActorMcp::new(node),
            default_access,
            session: uuid::Uuid::new_v4().to_string(),
            tool_router: Self::tool_router() + Self::actor_tool_router(),
        }
    }
    async fn definition_command(
        &self,
        context: &RequestContext<rmcp::RoleServer>,
        command: &str,
        args: impl Serialize,
    ) -> String {
        let response = self
            .service_for(context)
            .command(CommandRequest {
                session: Some(self.session.clone()),
                command: command.into(),
                args: serde_json::to_value(args).expect("MCP arguments serialize as JSON"),
            })
            .await;
        serde_json::to_string(&response).expect("API response serializes as JSON")
    }
    #[tool(
        description = "Add Rust source and return definition, entry item, wasm hashes and the item table."
    )]
    async fn loom_add(
        &self,
        Parameters(args): Parameters<AddArgs>,
        context: RequestContext<rmcp::RoleServer>,
    ) -> String {
        self.definition_command(&context, "add", args).await
    }
    #[tool(
        description = "Read definition source from the CAS by name or hash, with its item table."
    )]
    async fn loom_view(
        &self,
        Parameters(args): Parameters<TargetArgs>,
        context: RequestContext<rmcp::RoleServer>,
    ) -> String {
        self.definition_command(&context, "view", args).await
    }
    #[tool(description = "Add new Rust source and move a name while keeping old hashes runnable.")]
    async fn loom_update(
        &self,
        Parameters(args): Parameters<UpdateArgs>,
        context: RequestContext<rmcp::RoleServer>,
    ) -> String {
        self.definition_command(&context, "update", args).await
    }
    #[tool(description = "List a name’s definition history with timestamps and changed items.")]
    async fn loom_history(
        &self,
        Parameters(args): Parameters<NameArgs>,
        context: RequestContext<rmcp::RoleServer>,
    ) -> String {
        self.definition_command(&context, "history", args).await
    }
    #[tool(
        description = "Compare item hashes between two definitions: added, removed and changed."
    )]
    async fn loom_diff(
        &self,
        Parameters(args): Parameters<DiffArgs>,
        context: RequestContext<rmcp::RoleServer>,
    ) -> String {
        self.definition_command(&context, "diff", args).await
    }
    #[tool(
        description = "Run a definition by name or hash and return its output and performed effects."
    )]
    async fn loom_run(
        &self,
        Parameters(args): Parameters<RunArgs>,
        context: RequestContext<rmcp::RoleServer>,
    ) -> String {
        self.definition_command(&context, "run", args).await
    }
    #[tool(description = "Find definition names and item names containing text.")]
    async fn loom_find(
        &self,
        Parameters(args): Parameters<FindArgs>,
        context: RequestContext<rmcp::RoleServer>,
    ) -> String {
        self.definition_command(&context, "find", args).await
    }
    #[tool(description = "List definitions whose pinned dependencies include this hash.")]
    async fn loom_dependents(
        &self,
        Parameters(args): Parameters<HashArgs>,
        context: RequestContext<rmcp::RoleServer>,
    ) -> String {
        self.definition_command(&context, "dependents", args).await
    }
    fn service_for(&self, context: &RequestContext<rmcp::RoleServer>) -> Service {
        let access = context
            .extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<Access>())
            .cloned()
            .unwrap_or_else(|| self.default_access.clone());
        self.service.scoped(access)
    }
    #[tool(
        description = "Run a command with its JSON args object: machine.create {root:string} returns machine with id; run {target:string,args:Value[]} invokes a definition; view {target:string} reads source and item hashes; defs {} lists definitions; events {after?:number,limit?:number} lists definition events, deps {hash:string}; cas.list {limit?,after?,kind?,q?}, cas.inspect {hash:string}. For machine filesystem work define guest code using fs.list, then call it with machine id. Read loom_intro_rust for effect signatures."
    )]
    async fn loom_command(
        &self,
        Parameters(args): Parameters<CommandArgs>,
        context: RequestContext<rmcp::RoleServer>,
    ) -> String {
        let direct = loom_api::command_returns_direct(&args.command);
        let response = self
            .service_for(&context)
            .command(CommandRequest {
                session: Some(args.session.unwrap_or_else(|| self.session.clone())),
                command: args.command,
                args: args.args,
            })
            .await;
        serde_json::to_string(&if direct {
            response
        } else {
            self.service.inline(response)
        })
        .unwrap()
    }
}
#[tool_handler]
impl ServerHandler for LoomMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo{instructions:Some("Loom runs Rust core WebAssembly guests. All I/O goes through loom effects. Use loom_add to check and build Rust source, loom_view to read stored source, loom_update to move names, and loom_run to execute names or hashes. loom_history and loom_diff compare item identities; loom_find searches names and loom_dependents follows pinned dependencies. The actor_* tools inspect and drive the native actor network; actor_behaviors lists spawnable hashes and actor_tree shows the root supervisor.".into()),capabilities:ServerCapabilities::builder().enable_tools().enable_resources().enable_prompts().build(),..Default::default()}
    }
    async fn list_prompts(
        &self,
        _: Option<PaginatedRequestParams>,
        _: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ListPromptsResult, rmcp::ErrorData> {
        Ok(ListPromptsResult {
            prompts: vec![Prompt::new(
                "loom_intro_rust",
                Some("Rust guest effects and compilation"),
                None,
            )],
            ..Default::default()
        })
    }
    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<GetPromptResult, rmcp::ErrorData> {
        let text = match request.name.as_str() {
            "loom_intro_rust" => {
                "Use loom_add {source,name?} to store and build a definition, loom_view {target} to inspect its CAS source, and loom_run {target,args?} to run it. loom_update {name,source} moves a name; old hashes remain runnable. loom_history {name} lists changes, loom_diff {old,new} compares item hashes, loom_find {text} searches names and items, and loom_dependents {hash} lists pinned dependents. Write ordinary Rust using the loom SDK. Guest Rust has no macros. Every crate-root pub fn is an entry. Declare an optional schema with pub const LOOM_SCHEMA: &str. There are no effect declarations. The compiler driver infers effect rows; loom_add reports them per entry in entries.<name>.effects (labels and unknown). perform(name, args) suspends the guest. handle/handle_any install deep guest handlers; callbacks perform in the outer context. scope.spawn and job.join run borrowed closures; call(def,args) calls another definition without inheriting handlers. fs::list(machine,path) returns typed DirEntry values with name, size, and kind: EntryKind::File, Directory, Symlink, Other. machine is a machine ID; path is relative to its pinned root. fs::walk adds bounded recursion. fs::read returns String, fs::read_optional returns Option<String>, fs::write writes UTF-8 content. preview::writes runs under a guest handler that returns filesystem diff previews without writing those files. No std::fs/net/time/env/process; use Loom effects. Cargo diagnostics include file,line,col,code and hint; build.ms is actual elapsed time. Guest effect values cross typed DAG-CBOR; MCP envelopes use JSON."
            }
            _ => return Err(rmcp::ErrorData::invalid_params("unknown prompt", None)),
        };
        Ok(GetPromptResult {
            description: None,
            messages: vec![PromptMessage::new_text(PromptMessageRole::User, text)],
        })
    }
    async fn list_resources(
        &self,
        _: Option<PaginatedRequestParams>,
        context: RequestContext<rmcp::RoleServer>,
    ) -> Result<ListResourcesResult, rmcp::ErrorData> {
        self.actor_access(&context, loom_api::Scope::Read)?;
        Ok(ListResourcesResult { resources: vec![serde_json::from_value(serde_json::json!({"uri":"actor://tree","name":"Actor supervision tree","mimeType":"application/json"})).map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?], ..Default::default() })
    }
    async fn list_resource_templates(
        &self,
        _: Option<PaginatedRequestParams>,
        _: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ListResourceTemplatesResult, rmcp::ErrorData> {
        let mut templates = Vec::new();
        for uri in [
            "actor://{id}/inbox",
            "actor://{id}/effects",
            "actor://{id}/outbox",
            "actor://{id}/lineage",
            "loom://def/{name}",
            "loom://build/{component_hash}",
        ] {
            templates.push(
                serde_json::from_value(
                    serde_json::json!({"uriTemplate":uri,"name":uri,"mimeType":"application/json"}),
                )
                .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?,
            );
        }
        Ok(ListResourceTemplatesResult {
            resource_templates: templates,
            ..Default::default()
        })
    }
    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ReadResourceResult, rmcp::ErrorData> {
        let uri = &request.uri;
        if uri.starts_with("actor://") {
            self.actor_access(&context, loom_api::Scope::Read)?;
            return self.actors.resource(uri).await;
        }
        let response = if let Some(name) = uri.strip_prefix("loom://def/") {
            self.service_for(&context)
                .command(CommandRequest {
                    session: None,
                    command: "view".into(),
                    args: serde_json::json!({"target":name}),
                })
                .await
        } else if let Some(hash) = uri.strip_prefix("loom://build/") {
            self.service_for(&context)
                .command(CommandRequest {
                    session: None,
                    command: "build".into(),
                    args: serde_json::json!({"hash":hash}),
                })
                .await
        } else {
            return Err(rmcp::ErrorData::invalid_params(
                "unknown resource URI",
                None,
            ));
        };
        if !response.ok {
            return Err(rmcp::ErrorData::invalid_params(
                response.result.to_string(),
                None,
            ));
        }
        Ok(ReadResourceResult {
            contents: vec![ResourceContents::text(response.result.to_string(), uri)],
        })
    }
}

pub async fn stdio(service: Arc<Service>, node: loom_actor::Node) -> anyhow::Result<()> {
    let running = LoomMcp::new(service, Access::owner(), node)
        .serve(rmcp::transport::stdio())
        .await?;
    running.waiting().await?;
    Ok(())
}
pub fn router(service: Arc<Service>, node: loom_actor::Node) -> axum::Router {
    use rmcp::transport::streamable_http_server::{
        StreamableHttpService, session::local::LocalSessionManager,
    };
    let transport = StreamableHttpService::new(
        move || {
            Ok(LoomMcp::new(
                service.clone(),
                Access::default(),
                node.clone(),
            ))
        },
        Arc::new(LocalSessionManager::default()),
        Default::default(),
    );
    axum::Router::new().nest_service("/mcp", transport)
}

#[cfg(test)]
mod definition_args_tests {
    use super::UpdateArgs;
    use serde_json::json;

    #[test]
    fn update_preserves_omitted_and_explicit_policy() {
        for input in [
            json!({"name":"f", "source":"source"}),
            json!({"name":"f", "source":"source", "allowed_effects":null}),
            json!({"name":"f", "source":"source", "allowed_effects":[]}),
            json!({"name":"f", "source":"source", "deps":{}}),
        ] {
            let args: UpdateArgs = serde_json::from_value(input.clone()).unwrap();
            assert_eq!(serde_json::to_value(args).unwrap(), input);
        }
        let schema = serde_json::to_value(schemars::schema_for!(UpdateArgs)).unwrap();
        let required = schema["required"].as_array().unwrap();
        assert!(!required.contains(&json!("allowed_effects")));
        assert!(!required.contains(&json!("deps")));
    }
}
