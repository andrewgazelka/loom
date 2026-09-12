mod actors;
pub use actors::ActorMcp;
use loom_api::{Access, Service};
use loom_proto::{CommandRequest, DefineRequest, EvalRequest};
use rmcp::service::RequestContext;
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{tool::ToolRouter, wrapper::Parameters},
    model::*,
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Clone)]
pub struct LoomMcp {
    service: Arc<Service>,
    actors: ActorMcp,
    session: String,
    default_access: Access,
    tool_router: ToolRouter<Self>,
}
#[derive(Deserialize, JsonSchema)]
pub struct DefineArgs {
    #[serde(default)]
    allowed_effects: Option<Vec<String>>,
    #[serde(default)]
    lang: Option<String>,
    name: String,
    source: String,
    #[serde(default)]
    deps: std::collections::BTreeMap<String, String>,
}
#[derive(Deserialize, JsonSchema)]
pub struct EvalArgs {
    #[serde(default)]
    deps: std::collections::BTreeMap<String, String>,
    session: Option<String>,
    source: String,
}
#[derive(Deserialize, JsonSchema)]
pub struct CommandArgs {
    session: Option<String>,
    command: String,
    #[serde(default)]
    args: serde_json::Value,
}
#[derive(Deserialize, JsonSchema)]
pub struct ResolveArgs {
    hash: String,
}
#[derive(Deserialize, JsonSchema)]
pub struct CrateAddArgs {
    name: String,
    version: String,
}
#[derive(Deserialize, JsonSchema)]
pub struct UpgradeArgs {
    old: String,
    new: String,
}
#[tool_router]
impl LoomMcp {
    pub fn new(service: Arc<Service>, default_access: Access, node: loom_actor::Node) -> Self {
        Self {
            service,
            actors: ActorMcp::new(node),
            default_access,
            session: uuid::Uuid::new_v4().to_string(),
            tool_router: Self::tool_router() + Self::actor_tool_router(),
        }
    }
    #[tool(
        name = "crate_add",
        description = "Fetch and checksum-verify a crates.io release into the content-addressed source store."
    )]
    async fn crate_add(
        &self,
        Parameters(args): Parameters<CrateAddArgs>,
        context: RequestContext<rmcp::RoleServer>,
    ) -> String {
        let response = self
            .service_for(&context)
            .command(CommandRequest {
                session: Some(self.session.clone()),
                command: "crate.add".into(),
                args: serde_json::json!({"name":args.name,"version":args.version}),
            })
            .await;
        serde_json::to_string(&response).unwrap()
    }
    #[tool(
        name = "loom_upgrade",
        description = "Explicitly replace a definition or crate hash in named dependents and report their new identities."
    )]
    async fn loom_upgrade(
        &self,
        Parameters(args): Parameters<UpgradeArgs>,
        context: RequestContext<rmcp::RoleServer>,
    ) -> String {
        let response = self
            .service_for(&context)
            .command(CommandRequest {
                session: Some(self.session.clone()),
                command: "upgrade".into(),
                args: serde_json::json!({"old":args.old,"new":args.new}),
            })
            .await;
        serde_json::to_string(&response).unwrap()
    }
    #[tool(
        description = "Define checked Rust compiled to core WebAssembly. Builds return cargo diagnostics and duration."
    )]
    async fn loom_define(
        &self,
        Parameters(args): Parameters<DefineArgs>,
        context: RequestContext<rmcp::RoleServer>,
    ) -> String {
        let lang = match args.lang.as_deref() {
            None | Some("rust") => loom_proto::Lang::Rust,
            Some(_) => {
                return serde_json::to_string(
                    &self
                        .service
                        .response(Err(anyhow::anyhow!("invalid language"))),
                )
                .unwrap();
            }
        };
        serde_json::to_string(
            &self.service.inline(
                self.service_for(&context)
                    .define(DefineRequest {
                        allowed_effects: args.allowed_effects,
                        lang,
                        name: args.name,
                        source: args.source,
                        deps: args.deps,
                    })
                    .await,
            ),
        )
        .unwrap()
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
    #[tool(description = "Evaluate a Rust expression in this connection's session.")]
    async fn loom_eval(
        &self,
        Parameters(args): Parameters<EvalArgs>,
        context: RequestContext<rmcp::RoleServer>,
    ) -> String {
        serde_json::to_string(
            &self.service.inline(
                self.service_for(&context)
                    .eval(EvalRequest {
                        session: Some(args.session.unwrap_or_else(|| self.session.clone())),
                        source: args.source,
                        deps: args.deps,
                    })
                    .await,
            ),
        )
        .unwrap()
    }
    #[tool(
        description = "Run a command with its JSON args object: machine.create {root:string} returns machine with id; call {hash:string,args:Value[]} invokes a definition; resolve {hash:string} accepts name/hash/CID; defs {} lists definitions; events {after?:number,limit?:number} lists definition events, deps {hash:string}; cas.list {limit?,after?,kind?,q?}, cas.inspect {hash:string}. For machine filesystem work define guest code using fs.list, then call it with machine id. Read loom_intro_rust for effect signatures."
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

    #[tool(description = "Resolve a definition name, hash, or JSON CAS reference.")]
    async fn loom_resolve(
        &self,
        Parameters(args): Parameters<ResolveArgs>,
        context: RequestContext<rmcp::RoleServer>,
    ) -> String {
        serde_json::to_string(
            &self
                .service_for(&context)
                .command(CommandRequest {
                    session: Some(self.session.clone()),
                    command: "resolve".into(),
                    args: serde_json::json!({"hash":args.hash}),
                })
                .await,
        )
        .unwrap()
    }
}
#[tool_handler]
impl ServerHandler for LoomMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo{instructions:Some("Loom runs Rust core WebAssembly guests. All I/O goes through loom effects. Define checks and builds; calls use definition hashes. The actor_* tools inspect and drive the native actor network; actor_behaviors lists spawnable hashes and actor_tree shows the root supervisor.".into()),capabilities:ServerCapabilities::builder().enable_tools().enable_resources().enable_prompts().build(),..Default::default()}
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
                "Write ordinary Rust using the loom SDK. Every pub fn at the crate root is an entry. Effect rows are inferred through the resolved call graph and enforced at runtime. perform labels must be string literals or consts; dynamic labels are rejected. perform(name, args) suspends the guest. handle/handle_any install deep guest handlers; callbacks perform in the outer context. scope.spawn and job.join run borrowed closures; call(def,args) calls another definition without inheriting handlers. fs::list(machine,path) returns typed DirEntry values with name, size, and kind: EntryKind::File, Directory, Symlink, Other. machine is a machine ID; path is relative to its pinned root. fs::walk adds bounded recursion. fs::read returns String, fs::read_optional returns Option<String>, fs::write writes UTF-8 content. preview::writes runs under a guest handler that returns filesystem diff previews without writing those files. No std::fs/net/time/env/process; use Loom effects. Cargo diagnostics include file,line,col,code and hint; build.ms is actual elapsed time. Guest effect values cross typed DAG-CBOR; MCP envelopes use JSON."
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
                    command: "resolve".into(),
                    args: serde_json::json!({"hash":name}),
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
