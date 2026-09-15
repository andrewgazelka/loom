pub mod actors;
mod commands;
mod definitions;
mod evolution;
mod http;
mod javascript;
mod message_failure;
mod source;
#[cfg(test)]
mod tests;
mod unison;
pub use http::{protect, protect_public, router};
use source::*;
mod auth;
mod tenant;
pub use tenant::{ServiceDirectory, TenantId};
mod actor_websocket;
pub use actor_websocket::WebSocketHub;
mod build_progress;
mod cas_browser;
use anyhow::{Context, Result, bail, ensure};
pub use auth::{Access, Authorizer, Scope, TokenConfig};
use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Path, Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response as HttpResponse},
    routing::{get, post},
};
use loom_proto::{CommandRequest, Def, DefineRequest, Lang, Response, Value};
use loom_store::Store;
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct Service {
    access: Access,
    tenant: TenantId,
    websockets: Option<WebSocketHub>,
    actors: Option<actors::ActorService>,
    pub store: Store,
    pub runtime: loom_rt::Runtime,
    checker: Arc<loom_check::Checker>,
    builder: Arc<loom_build::Builder>,
    v8_engine: Option<Arc<loom_v8::V8Engine>>,
    script_compiler: Arc<loom_imports::Compiler>,
    native_registry: Option<Arc<dyn loom_actor::Registry>>,
    process_presets: BTreeMap<String, String>,
    languages: Vec<Lang>,
    backup_directory: PathBuf,
    definitions_gate: Arc<tokio::sync::Mutex<()>>,
    build_progress: build_progress::BuildProgress,
    last_reply_storage_nanos: Arc<AtomicU64>,
}
impl Service {
    pub fn new(store: Store, root: PathBuf, languages: Vec<Lang>) -> Result<Self> {
        Self::new_with_engine(store, root, languages, None)
    }
    pub fn new_with_engine(
        store: Store,
        root: PathBuf,
        languages: Vec<Lang>,
        engine: Option<Arc<loom_v8::V8Engine>>,
    ) -> Result<Self> {
        let backup_directory = root.join("backups");
        let checker = Arc::new(loom_check::Checker::new());
        let builder = Arc::new(loom_build::Builder::new(root, store.clone()));
        let resolver = Arc::new(BuildResolver {
            store: store.clone(),
            gate: tokio::sync::Mutex::new(()),
        });
        let runtime = loom_rt::Runtime::with_resolver_and_v8(store.clone(), resolver, engine)?;
        let v8_engine =
            if languages.contains(&Lang::JavaScript) || languages.contains(&Lang::TypeScript) {
                Some(runtime.v8_engine()?)
            } else {
                None
            };
        Ok(Self {
            access: Access::owner(),
            tenant: TenantId::default(),
            websockets: None,
            actors: None,
            runtime,
            store,
            checker,
            builder,
            v8_engine,
            script_compiler: Arc::new(loom_imports::Compiler::default()),
            native_registry: None,
            process_presets: BTreeMap::new(),
            languages,
            backup_directory,
            definitions_gate: Arc::new(tokio::sync::Mutex::new(())),
            build_progress: build_progress::BuildProgress::default(),
            last_reply_storage_nanos: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn with_driver_path(mut self, path: PathBuf) -> Self {
        self.builder = Arc::new(
            self.builder
                .for_store(self.store.clone())
                .with_driver_path(path),
        );
        self
    }

    pub fn with_process_presets(mut self, presets: BTreeMap<String, String>) -> Result<Self> {
        ensure!(
            self.native_registry.is_none(),
            "configure process presets before native drivers"
        );
        self.process_presets = presets;
        Ok(self)
    }
    pub fn with_native_drivers(
        mut self,
        behaviors: Vec<Arc<dyn loom_actor::Behavior>>,
        drivers: Vec<Arc<dyn loom_actor::Driver>>,
    ) -> Result<Self> {
        ensure!(
            self.native_registry.is_none(),
            "native drivers already configured"
        );
        self.native_registry = Some(Arc::new(
            loom_actor::CompositeRegistry::new(self.actor_registry(), behaviors, drivers)?
                .with_processes(self.process_presets.clone())?,
        ));
        Ok(self)
    }
    pub fn actor_registry(&self) -> Arc<dyn loom_actor::Registry> {
        if let Some(registry) = &self.native_registry {
            return registry.clone();
        }
        Arc::new(loom_behavior::StoreRegistry::with_runtime(
            self.store.clone(),
            self.runtime.clone(),
        ))
    }

    pub fn with_actors(mut self, node: loom_actor::Node) -> Self {
        self.actors = Some(actors::ActorService::new(node));
        self
    }
    pub fn tenant(&self) -> &TenantId {
        &self.tenant
    }
    pub fn with_tenant(mut self, tenant: TenantId) -> Self {
        if tenant != TenantId::default() {
            let cache = self
                .builder
                .cache_directory()
                .join("tenants")
                .join(tenant.as_str());
            self.builder = Arc::new(self.builder.for_cache_directory(cache));
            self.runtime = self.runtime.without_host_authority();
        }
        self.access = self.access.for_tenant(tenant.clone());
        self.tenant = tenant;
        self
    }
    /// Import policy is host configuration, never guest-controlled authority.
    pub fn with_script_compiler(mut self, compiler: loom_imports::Compiler) -> Self {
        self.script_compiler = Arc::new(compiler);
        self
    }
    pub fn with_build_directory(mut self, directory: PathBuf) -> Self {
        self.builder = Arc::new(self.builder.for_cache_directory(directory));
        self
    }
    pub fn build_directory(&self) -> &std::path::Path {
        self.builder.cache_directory()
    }
    pub fn with_websockets(mut self, hub: WebSocketHub) -> Self {
        if let Some(engine) = &self.v8_engine {
            hub.limit_actor_messages(engine.max_message_bytes());
        }
        self.websockets = Some(hub);
        self
    }
    pub fn actor_node(&self) -> Option<&loom_actor::Node> {
        self.actors.as_ref().map(|actors| &actors.node)
    }
    pub fn scoped(&self, access: Access) -> Result<Self> {
        ensure!(access.tenant() == &self.tenant, "tenant access mismatch");
        let mut service = self.clone();
        if !access.allows(Scope::Host) {
            service.runtime = service.runtime.without_host_authority();
        }
        service.access = access;
        Ok(service)
    }
    pub fn with_backup_directory(mut self, directory: PathBuf) -> Self {
        self.backup_directory = directory;
        self
    }
    pub fn response(&self, result: Result<Value>) -> Response {
        let result = result.and_then(|value| {
            loom_proto::encode(&value).map_err(anyhow::Error::msg)?;
            Ok(value)
        });
        let storage_start = Instant::now();
        let sequence = self.store.flush().and_then(|()| self.store.latest_seq());
        self.last_reply_storage_nanos.store(
            u64::try_from(storage_start.elapsed().as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        let seq = match sequence {
            Ok(seq) => seq,
            Err(error) => {
                return Response {
                    ok: false,
                    seq: 0,
                    result: json!({"error":format!("definition sequence unavailable: {error:#}"),"code":"store_unavailable"}),
                    diagnostics: vec![],
                };
            }
        };
        match result {
            Ok(result) => Response {
                ok: true,
                seq,
                result,
                diagnostics: vec![],
            },
            Err(error) if error.is::<message_failure::ActorMessageFailure>() => {
                let failure = error
                    .downcast_ref::<message_failure::ActorMessageFailure>()
                    .expect("checked error type");
                Response {
                    ok: false,
                    seq,
                    result: json!({"code":failure.code(),"error":failure.to_string(),"id":failure.id,"seq":failure.seq,"cause":failure.cause}),
                    diagnostics: vec![],
                }
            }
            Err(error) => Response {
                ok: false,
                seq,
                result: json!({"error":format!("{error:#}"),"code":if error.is::<auth::ScopeDenied>(){"forbidden"}else{"operation_failed"}}),
                diagnostics: vec![],
            },
        }
    }
    pub fn build_record(&self, hash: &str) -> Result<Value> {
        let mut after = 0;
        loop {
            let events = self.store.definition_events(after, 1000)?;
            if events.is_empty() {
                bail!("build not found")
            };
            for event in events {
                after = event.seq;
                if event.event["type"] == "component_built" && event.event["component_hash"] == hash
                {
                    let mut result = event.event;
                    let reference = result["logs_ref"]
                        .as_str()
                        .context("build logs reference missing")?;
                    result["logs"] = Value::String(String::from_utf8(
                        self.store.get(reference)?.context("build logs missing")?,
                    )?);
                    return Ok(result);
                }
            }
        }
    }
    pub fn inline(&self, mut response: Response) -> Response {
        if let Ok(bytes) = serde_json::to_vec(&response.result)
            && bytes.len() > 8192
        {
            let storage_start = Instant::now();
            let reference = self
                .store
                .put_value("result", &response.result)
                .and_then(|hash| self.store.reference(&hash, loom_proto::DAG_CBOR_CODEC));
            self.last_reply_storage_nanos.fetch_add(
                u64::try_from(storage_start.elapsed().as_nanos()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
            match reference {
                Ok(reference) => response.result = reference,
                Err(error) => return self.response(Err(error)),
            }
        }
        let storage_start = Instant::now();
        let flushed = self.store.flush();
        self.last_reply_storage_nanos.fetch_add(
            u64::try_from(storage_start.elapsed().as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        if let Err(error) = flushed {
            return self.response(Err(error));
        }
        response
    }
}
fn field<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    args[name]
        .as_str()
        .with_context(|| format!("missing string argument {name}"))
}
/// Definition commands return their requested data directly; CAS browsing stays read-only.
fn command_returns_direct(command: &str) -> bool {
    matches!(
        command,
        "add"
            | "view"
            | "update"
            | "update_view"
            | "update_repair"
            | "update_abort"
            | "update_rebase"
            | "history"
            | "diff"
            | "run"
            | "find"
            | "dependents"
            | "resolve"
            | "cas.list"
            | "cas.inspect"
    )
}
