mod actors;
pub use actors::ActorMcp;
use loom_api::{Access, Service, ServiceDirectory};
use loom_proto::CommandRequest;
use rmcp::service::RequestContext;
use rmcp::{ServerHandler, ServiceExt, model::*};
use std::sync::Arc;

#[derive(Clone)]
pub struct LoomMcp {
    directory: ServiceDirectory,
    session: String,
    default_access: Access,
}
impl LoomMcp {
    pub fn new(service: Arc<Service>, default_access: Access, node: loom_actor::Node) -> Self {
        Self::for_directory(
            Arc::new(service.as_ref().clone().with_actors(node)).into(),
            default_access,
        )
    }
    pub fn for_directory(directory: ServiceDirectory, default_access: Access) -> Self {
        Self {
            directory,
            default_access,
            session: uuid::Uuid::new_v4().to_string(),
        }
    }
    fn service_for(
        &self,
        context: &RequestContext<rmcp::RoleServer>,
    ) -> Result<Service, rmcp::ErrorData> {
        let access = context
            .extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<Access>())
            .cloned()
            .unwrap_or_else(|| self.default_access.clone());
        self.directory
            .scoped(access)
            .map_err(|error| rmcp::ErrorData::invalid_params(error.to_string(), None))
    }
}

impl ServerHandler for LoomMcp {
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<rmcp::RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        Ok(ListToolsResult {
            tools: loom_proto::verbs::VERBS
                .iter()
                .map(|verb| {
                    Tool::new(
                        verb.name,
                        command_description(verb.name),
                        verb.schema()
                            .as_object()
                            .expect("verb schema is an object")
                            .clone(),
                    )
                })
                .collect(),
            ..Default::default()
        })
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let response = self
            .service_for(&context)?
            .command(CommandRequest {
                session: Some(self.session.clone()),
                command: request.name.into_owned(),
                args: serde_json::Value::Object(request.arguments.unwrap_or_default()),
            })
            .await;
        let envelope = serde_json::to_value(response)
            .map_err(|error| rmcp::ErrorData::internal_error(error.to_string(), None))?;
        Ok(CallToolResult::structured(envelope))
    }
    fn get_info(&self) -> ServerInfo {
        ServerInfo{instructions:Some("Loom runs Rust core WebAssembly guests. All I/O goes through loom effects. Use add to check and build Rust source, view to read stored source, update to propagate changes atomically through callers, and run to execute names or hashes. history and diff compare item identities; find searches names and dependents follows pinned dependencies. export bundles definitions with their dependency closure into one CAS object and import rebuilds such a bundle from source on this node. The actor tools inspect and drive the native actor network; behaviors lists spawnable hashes and tree shows the root supervisor.".into()),capabilities:ServerCapabilities::builder().enable_tools().enable_resources().enable_prompts().build(),..Default::default()}
    }
    async fn list_prompts(
        &self,
        _: Option<PaginatedRequestParams>,
        _: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ListPromptsResult, rmcp::ErrorData> {
        Ok(ListPromptsResult {
            prompts: vec![Prompt::new(
                "intro_rust",
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
            "intro_rust" => {
                "Use add {source,name?} to store and build a definition, view {target} to inspect its CAS source, and run {target,args?} to run it. update {name,source,expected_hash?} rebuilds affected callers and publishes their names atomically; old hashes remain runnable. Read result.update.status: complete means published, needs_repair means live names are unchanged. Use update_view {id} to resume a durable session, update_repair {id,revision,changes} to submit a map of definition names to {source,deps?,allowed_effects?}, and update_abort {id,revision} to discard a pending session. Always pass the hash you read as expected_hash and the latest session revision when multiple agents collaborate. A conflict requires inspecting current state before retrying. update_rebase {id,revision} retries after unrelated namespace changes while preserving repairs; it refuses if an edited definition changed concurrently. history {name} lists changes, diff {old,new} compares item hashes, find {text} searches names and items, and dependents {hash} lists pinned dependents. Write ordinary Rust using the loom SDK. Guest Rust has no macros. Every crate-root pub fn is an entry. Declare an optional schema with pub const LOOM_SCHEMA: &str. There are no effect declarations. The compiler driver infers effect rows; add reports them per entry in entries.<name>.effects (labels and unknown). perform(name, args) suspends the guest. handle/handle_any install deep guest handlers; callbacks perform in the outer context. scope.spawn and job.join run borrowed closures; call(def,args) calls another definition without inheriting handlers. fs::list(machine,path) returns typed DirEntry values with name, size, and kind: EntryKind::File, Directory, Symlink, Other. machine is a machine ID; path is relative to its pinned root. fs::walk adds bounded recursion. fs::read returns String, fs::read_optional returns Option<String>, fs::write writes UTF-8 content. preview::writes runs under a guest handler that returns filesystem diff previews without writing those files. No std::fs/net/time/env/process; use Loom effects. Cargo diagnostics include file,line,col,code and hint; build.ms is actual elapsed time. Guest effect values cross typed DAG-CBOR; MCP envelopes use JSON."
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
            let service = self.service_for(&context)?;
            let node = service
                .actor_node()
                .ok_or_else(|| rmcp::ErrorData::invalid_params("actor node unavailable", None))?;
            return ActorMcp::new(node.clone()).resource(uri).await;
        }
        let response = if let Some(name) = uri.strip_prefix("loom://def/") {
            self.service_for(&context)?
                .command(CommandRequest {
                    session: None,
                    command: "view".into(),
                    args: serde_json::json!({"target":name}),
                })
                .await
        } else if let Some(hash) = uri.strip_prefix("loom://build/") {
            self.service_for(&context)?
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
            contents: vec![json_resource(response.result, uri)],
        })
    }
}

fn json_resource(value: serde_json::Value, uri: &str) -> ResourceContents {
    ResourceContents::TextResourceContents {
        uri: uri.into(),
        mime_type: Some("application/json".into()),
        text: value.to_string(),
        meta: None,
    }
}

pub async fn stdio(service: Arc<Service>, node: loom_actor::Node) -> anyhow::Result<()> {
    let access = Access::owner().for_tenant(service.tenant().clone());
    let running = LoomMcp::new(service, access, node)
        .serve(rmcp::transport::stdio())
        .await?;
    running.waiting().await?;
    Ok(())
}
pub fn router(directory: impl Into<ServiceDirectory>) -> axum::Router {
    use rmcp::transport::streamable_http_server::{
        StreamableHttpService, session::local::LocalSessionManager,
    };
    let directory = directory.into();
    let transport = StreamableHttpService::new(
        move || Ok(LoomMcp::for_directory(directory.clone(), Access::default())),
        Arc::new(LocalSessionManager::default()),
        Default::default(),
    );
    axum::Router::new().nest_service("/mcp", transport)
}

fn command_description(name: &str) -> String {
    match name {
        "update" => "Publish revised Rust source and automatically rebuild callers. Pass expected_hash from the source you edited and a unique request_id to recover this exact session after transport failure. Inspect result.update.status: complete publishes atomically; needs_repair leaves names unchanged and returns repair sources and diagnostics.",
        "update_view" => "Read a durable update session, its latest revision, affected definitions and compiler diagnostics.",
        "update_repair" => "Apply a batch of source repairs to an update session and retry propagation. Supply the latest revision; stale submissions are rejected. Inspect result.update.status before treating this as published.",
        "update_rebase" => "Replan a conflicted update against current names while retaining repairs. Refuses to overwrite concurrently edited definitions. Supply the latest session revision.",
        "update_abort" => "Abort a pending, repair or conflicted update session without changing live definitions. Supply the latest revision.",
        "export" => "Bundle named definitions with their dependency closure, sources, identities and vendored crate trees as one CARv1 file stored in the CAS. Download result.bundle.$ref from GET /v1/cas/{cid}. Compiled wasm is excluded; import rebuilds from source.",
        "import" => "Import a bundle already stored with POST /v1/cas (application/octet-stream): verify every block, rebuild each definition from source in dependency order, and publish all of them in one transaction. Refuses the whole bundle when a rebuilt hash differs from the recorded one or a bound name collides; into prefixes every imported name.",
        _ => return format!("Run the {name} command."),
    }.to_owned()
}
