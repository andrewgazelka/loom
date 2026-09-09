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
    session: String,
    default_access: Access,
    tool_router: ToolRouter<Self>,
}
#[derive(Deserialize, JsonSchema)]
pub struct DefineArgs {
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
#[tool_router]
impl LoomMcp {
    pub fn new(service: Arc<Service>, default_access: Access) -> Self {
        Self {
            service,
            default_access,
            session: uuid::Uuid::new_v4().to_string(),
            tool_router: Self::tool_router(),
        }
    }
    #[tool(
        description = "Define a checked TS or Rust component. Rust builds return cargo diagnostics and duration."
    )]
    async fn loom_define(
        &self,
        Parameters(args): Parameters<DefineArgs>,
        context: RequestContext<rmcp::RoleServer>,
    ) -> String {
        let lang = match args.lang.as_deref() {
            None | Some("ts") => loom_proto::Lang::Ts,
            Some("rust") => loom_proto::Lang::Rust,
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
    #[tool(description = "Evaluate a TypeScript expression in this connection's session.")]
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
        description = "Run a loom command: defs, actors, events, state, spawn, send, fork, upgrade, call, resolve, deps."
    )]
    async fn loom_command(
        &self,
        Parameters(args): Parameters<CommandArgs>,
        context: RequestContext<rmcp::RoleServer>,
    ) -> String {
        let resolve = args.command == "resolve";
        let response = self
            .service_for(&context)
            .command(CommandRequest {
                session: Some(args.session.unwrap_or_else(|| self.session.clone())),
                command: args.command,
                args: args.args,
            })
            .await;
        serde_json::to_string(&if resolve {
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
        ServerInfo{instructions:Some("Loom runs pure TS and Rust WASM guests. All I/O goes through loom abilities. Define checks and builds; call/spawn use definition hashes.".into()),capabilities:ServerCapabilities::builder().enable_tools().enable_resources().enable_prompts().build(),..Default::default()}
    }
    async fn list_prompts(
        &self,
        _: Option<PaginatedRequestParams>,
        _: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ListPromptsResult, rmcp::ErrorData> {
        Ok(ListPromptsResult {
            prompts: vec![
                Prompt::new(
                    "loom_intro_ts",
                    Some("TypeScript guest abilities and checking"),
                    None,
                ),
                Prompt::new(
                    "loom_intro_rust",
                    Some("Rust guest abilities and compilation"),
                    None,
                ),
            ],
            ..Default::default()
        })
    }
    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<GetPromptResult, rmcp::ErrorData> {
        let text = match request.name.as_str() {
            "loom_intro_ts" => {
                "Write synchronous pure TypeScript. Import abilities from 'loom' and named definition identities from 'loom:defs'; supply deps as an alias-to-hash map to define or eval. Abilities: perform(desc), all(descs), fork(def,args), join(fibers), sleep(ms), now(), random(), exec(args), fs. No fetch, Date, Math.random, timers, eval or Function. Define returns strict diagnostics; correct source and retry. Functions export main; actors export run(state,msg) returning events and fold(state,event) returning state. New components must compile before execution; build.ms reports elapsed time."
            }
            "loom_intro_rust" => {
                "Write pure Rust using loom_guest_rs. Export free functions with #[loom::def] or implement Actor and #[loom::actor]. Abilities are synchronous host imports: perform(desc), all(descs), fork(def,args), join(fibers), abilities::exec, abilities::fs. No std::fs/net/time/env/process; use loom abilities. Cargo diagnostics include file,line,col,code and hint. New dependencies require a cold build; warm builds reuse cache. build.ms reports actual elapsed time. State, event, message, and call values use serde JSON-compatible shapes."
            }
            _ => return Err(rmcp::ErrorData::invalid_params("unknown prompt", None)),
        };
        Ok(GetPromptResult {
            description: None,
            messages: vec![PromptMessage::new_text(PromptMessageRole::User, text)],
        })
    }
    async fn list_resource_templates(
        &self,
        _: Option<PaginatedRequestParams>,
        _: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ListResourceTemplatesResult, rmcp::ErrorData> {
        let mut templates = Vec::new();
        for uri in [
            "loom://def/{name}",
            "loom://actor/{id}/state",
            "loom://actor/{id}/log",
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
        let response = if let Some(name) = uri.strip_prefix("loom://def/") {
            self.service_for(&context)
                .command(CommandRequest {
                    session: None,
                    command: "resolve".into(),
                    args: serde_json::json!({"hash":name}),
                })
                .await
        } else if let Some(path) = uri.strip_prefix("loom://actor/") {
            let Some((id, kind)) = path.rsplit_once('/') else {
                return Err(rmcp::ErrorData::invalid_params("invalid actor URI", None));
            };
            let command = match kind {
                "state" => "state",
                "log" => "events",
                _ => {
                    return Err(rmcp::ErrorData::invalid_params(
                        "unknown actor resource",
                        None,
                    ));
                }
            };
            self.service_for(&context)
                .command(CommandRequest {
                    session: None,
                    command: command.into(),
                    args: serde_json::json!({"actor":id}),
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

pub async fn stdio(service: Arc<Service>) -> anyhow::Result<()> {
    let running = LoomMcp::new(service, Access::owner())
        .serve(rmcp::transport::stdio())
        .await?;
    running.waiting().await?;
    Ok(())
}
pub fn router(service: Arc<Service>) -> axum::Router {
    use rmcp::transport::streamable_http_server::{
        StreamableHttpService, session::local::LocalSessionManager,
    };
    let transport = StreamableHttpService::new(
        move || Ok(LoomMcp::new(service.clone(), Access::default())),
        Arc::new(LocalSessionManager::default()),
        Default::default(),
    );
    axum::Router::new().nest_service("/mcp", transport)
}
