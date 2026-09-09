mod auth;
use anyhow::{Context, Result, bail, ensure};
pub use auth::{Access, Authorizer, Scope, TokenConfig};
use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Path, Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response as HttpResponse},
    routing::{get, post},
};
use loom_proto::{CommandRequest, Def, DefineRequest, EvalRequest, Lang, Response, Value};
use loom_store::Store;
use serde::Deserialize;
use serde_json::json;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

#[derive(Clone)]
pub struct Service {
    access: Access,
    pub store: Store,
    pub runtime: loom_rt::Runtime,
    checker: Arc<loom_check::Checker>,
    builder: Arc<loom_build::Builder>,
    languages: Vec<Lang>,
    backup_directory: PathBuf,
    definitions_gate: Arc<tokio::sync::Mutex<()>>,
}
impl Service {
    pub fn new(store: Store, root: PathBuf, languages: Vec<Lang>) -> Result<Self> {
        let backup_directory = root.join("backups");
        let checker = Arc::new(loom_check::Checker::new(root.clone()));
        let builder = Arc::new(loom_build::Builder::new(root));
        let resolver = Arc::new(BuildResolver {
            store: store.clone(),
            builder: builder.clone(),
            gate: tokio::sync::Mutex::new(()),
        });
        Ok(Self {
            access: Access::owner(),
            runtime: loom_rt::Runtime::with_resolver(store.clone(), resolver)?,
            store,
            checker,
            builder,
            languages,
            backup_directory,
            definitions_gate: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    pub fn scoped(&self, access: Access) -> Self {
        let mut service = self.clone();
        service.access = access;
        service
    }
    pub fn with_backup_directory(mut self, directory: PathBuf) -> Self {
        self.backup_directory = directory;
        self
    }
    pub fn response(&self, result: Result<Value>) -> Response {
        let seq = match self.store.latest_seq() {
            Ok(seq) => seq,
            Err(error) => {
                return Response {
                    ok: false,
                    seq: 0,
                    result: json!({"error":format!("log sequence unavailable: {error:#}"),"code":"store_unavailable"}),
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
            Err(error) => Response {
                ok: false,
                seq,
                result: json!({"error":format!("{error:#}"),"code":if error.is::<auth::ScopeDenied>(){"forbidden"}else{"operation_failed"}}),
                diagnostics: vec![],
            },
        }
    }
    pub async fn define(&self, request: DefineRequest) -> Response {
        if let Err(error) = self.access.require(Scope::Define) {
            return self.response(Err(error));
        }
        self.define_authorized(request).await
    }
    async fn define_authorized(&self, request: DefineRequest) -> Response {
        let _guard = self.definitions_gate.lock().await;
        match self.define_inner(request).await {
            Ok(response) => response,
            Err(error) => self.response(Err(error)),
        }
    }
    async fn define_inner(&self, mut request: DefineRequest) -> Result<Response> {
        ensure!(
            self.languages.contains(&request.lang),
            "language {} is disabled",
            request.lang.as_str()
        );
        ensure!(
            request.source.len()
                <= if request.lang == Lang::Rust {
                    16 * 1024 * 1024
                } else {
                    1024 * 1024
                },
            "source too large"
        );
        if request.lang == Lang::Rust
            && let Some(reference) = source_reference(&request.source)
        {
            let bundle = self
                .store
                .get(reference)?
                .context("Rust source bundle not found")?;
            request.source = decode_source_bundle(&bundle)?;
        }
        for hash in request.deps.values_mut() {
            *hash = self
                .store
                .resolve(hash)?
                .context("dependency not found")?
                .hash;
        }
        let previous = self.store.resolve(&request.name)?;
        let checked = self.check_definition(&request).await?;
        if !checked.diagnostics.is_empty() {
            return Ok(Response {
                ok: false,
                seq: self.store.latest_seq()?,
                result: Value::Null,
                diagnostics: checked.diagnostics,
            });
        }
        if checked.lang == Lang::Ts {
            let def = Def {
                hash: checked.hash.clone(),
                lang: checked.lang,
                component_hash: self
                    .store
                    .definition(&checked.hash)?
                    .and_then(|def| def.component_hash),
                sig: checked.sig,
            };
            self.store
                .define(&def, Some(&request.name), &checked.source, &checked.deps)?;
            let updates = self.rehash_dependents(previous.as_ref(), &def).await?;
            return Ok(self.response(Ok(json!({"def":def,"build":{"status":if def.component_hash.is_some(){"cached"}else{"pending"}},"rehashed":updates.rehashed,"stale_actors":updates.stale_actors}))));
        }
        let dependencies = dependency_closure(&self.store, &checked.deps)?;
        let built = self
            .builder
            .build_with_dependencies(&checked, &dependencies)
            .await?;
        if !built.diagnostics.is_empty() {
            return Ok(Response {
                ok: false,
                seq: self.store.latest_seq()?,
                result: json!({"build":{"ms":built.ms,"logs":built.logs}}),
                diagnostics: built.diagnostics,
            });
        }
        ensure!(
            !built.component.is_empty(),
            "builder returned an empty component"
        );
        let component_hash = self.store.put("component", &built.component)?;
        let logs_ref = self.store.put("blob", built.logs.as_bytes())?;
        let def = Def {
            hash: checked.hash,
            lang: checked.lang,
            component_hash: Some(component_hash.clone()),
            sig: checked.sig,
        };
        self.store
            .define(&def, Some(&request.name), &checked.source, &checked.deps)?;
        self.store.append("system",&json!({"type":"component_built","component_hash":component_hash,"logs_ref":logs_ref,"ms":built.ms,"size":built.component.len()}),0)?;
        let updates = self.rehash_dependents(previous.as_ref(), &def).await?;
        Ok(self.response(Ok(json!({"def":def,"build":{"ms":built.ms,"component_hash":component_hash,"size":built.component.len(),"logs_ref":logs_ref},"rehashed":updates.rehashed,"stale_actors":updates.stale_actors}))))
    }
    async fn check_definition(&self, request: &DefineRequest) -> Result<loom_check::CheckedDef> {
        let checked = self
            .checker
            .check_with_signatures(request, &dependency_signatures(&self.store, &request.deps)?)
            .await?;
        if checked.lang != Lang::Rust || !checked.diagnostics.is_empty() {
            return Ok(checked);
        }
        let dependencies = dependency_closure(&self.store, &checked.deps)?;
        let source = self
            .builder
            .prepare_rust_source(&checked, &dependencies)
            .await?;
        let prepared = DefineRequest {
            lang: Lang::Rust,
            name: request.name.clone(),
            source,
            deps: checked.deps,
        };
        Ok(self
            .checker
            .check_with_signatures(
                &prepared,
                &dependency_signatures(&self.store, &prepared.deps)?,
            )
            .await?)
    }
    async fn rehash_dependents(
        &self,
        previous: Option<&Def>,
        current: &Def,
    ) -> Result<Redefinitions> {
        let Some(previous) = previous.filter(|def| def.hash != current.hash) else {
            return Ok(Redefinitions::default());
        };
        let mut replacements = BTreeMap::new();
        replacements.insert(previous.hash.clone(), current.hash.clone());
        let mut pending = vec![previous.hash.clone()];
        let mut candidates = BTreeMap::new();
        while let Some(hash) = pending.pop() {
            for dependent in self.store.dependents(&hash)? {
                if candidates.contains_key(&dependent) {
                    continue;
                }
                let Some(name) = self.store.definition_name(&dependent)? else {
                    continue;
                };
                if self
                    .store
                    .resolve(&name)?
                    .is_none_or(|def| def.hash != dependent)
                {
                    continue;
                }
                ensure!(
                    candidates.len() < 1024,
                    "dependent closure exceeds 1024 definitions"
                );
                candidates.insert(
                    dependent.clone(),
                    stored_definition(&self.store, &dependent)?,
                );
                pending.push(dependent);
            }
        }
        let mut updates = Redefinitions::default();
        while !candidates.is_empty() {
            let hash = candidates
                .iter()
                .find(|entry| {
                    entry
                        .1
                        .deps
                        .values()
                        .all(|dependency| !candidates.contains_key(dependency))
                })
                .map(|entry| entry.0.clone())
                .context("cyclic definition dependencies")?;
            let mut candidate = candidates.remove(&hash).context("dependent disappeared")?;
            for dependency in candidate.deps.values_mut() {
                if let Some(replacement) = replacements.get(dependency) {
                    *dependency = replacement.clone();
                }
            }
            let request = DefineRequest {
                lang: candidate.lang,
                name: candidate.name.clone(),
                source: candidate.source,
                deps: candidate.deps,
            };
            let checked = self.check_definition(&request).await?;
            if !checked.diagnostics.is_empty() {
                updates.rehashed.push(
                    json!({"name":request.name,"previous":hash,"diagnostics":checked.diagnostics}),
                );
                continue;
            }
            let def = Def {
                hash: checked.hash,
                lang: checked.lang,
                component_hash: None,
                sig: checked.sig,
            };
            self.store
                .define(&def, Some(&request.name), &checked.source, &checked.deps)?;
            replacements.insert(hash.clone(), def.hash.clone());
            updates
                .rehashed
                .push(json!({"name":request.name,"previous":hash,"def":def}));
        }
        updates.stale_actors = self
            .store
            .actors()?
            .into_iter()
            .filter(|actor| replacements.contains_key(&actor.behavior_hash))
            .map(|actor| actor.id)
            .collect();
        Ok(updates)
    }
    pub async fn eval(&self, request: EvalRequest) -> Response {
        if let Err(error) = self.access.require(Scope::Execute) {
            return self.response(Err(error));
        }
        match self.eval_inner(request).await {
            Ok(response) => response,
            Err(error) => self.response(Err(error)),
        }
    }
    async fn eval_inner(&self, request: EvalRequest) -> Result<Response> {
        let session = request
            .session
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        for alias in request.deps.keys() {
            ensure!(valid_alias(alias), "dependency alias must be an identifier");
        }
        let dependency_import = if request.deps.is_empty() {
            String::new()
        } else {
            format!(
                "import {{{}}} from \"loom:defs\";",
                request.deps.keys().cloned().collect::<Vec<_>>().join(",")
            )
        };
        let source = format!(
            "{dependency_import} import {{perform,all,race,fork,join,call,send,spawn,exec,llm,fs,cas,sleep,now,random}} from \"loom\"; export function main(): unknown {{ return ({}); }}",
            request.source
        );
        let response = self
            .define_authorized(DefineRequest {
                lang: Lang::Ts,
                name: format!("session/{session}/eval"),
                source,
                deps: request.deps,
            })
            .await;
        if !response.ok {
            return Ok(response);
        }
        let hash = response.result["def"]["hash"]
            .as_str()
            .context("definition hash missing")?;
        let actor = match self.store.session(&session)? {
            Some(actor) => actor,
            None => {
                let definition=self.define(DefineRequest{lang:Lang::Ts,name:"loom/session".into(),source:"export function run(state: unknown, msg: unknown): unknown[] { return [msg]; } export function fold(state: unknown, event: unknown): unknown { return event; }".into(),deps:BTreeMap::new()}).await;
                if !definition.ok {
                    return Ok(definition);
                }
                let actor = self
                    .runtime
                    .spawn(
                        definition.result["def"]["hash"]
                            .as_str()
                            .context("session behavior hash missing")?,
                        Value::Null,
                    )
                    .await?;
                self.store.create_session(&session, &actor.id, "owner")?;
                actor.id
            }
        };
        let result = self.runtime.call_def(hash, Value::Null).await?;
        self.runtime
            .send(
                &actor,
                json!({"type":"evaluated","source":request.source,"def":hash,"result":result}),
            )
            .await?;
        Ok(self.response(Ok(json!({"session":session,"actor":actor,"value":result}))))
    }
    pub async fn command(&self, request: CommandRequest) -> Response {
        if let Err(error) = self.access.require(auth::command_scope(&request.command)) {
            return self.response(Err(error));
        }
        self.response(self.command_inner(request).await)
    }
    async fn command_inner(&self, request: CommandRequest) -> Result<Value> {
        let args = &request.args;
        match request.command.as_str() {
            "model.state" => Ok(serde_json::to_value(self.runtime.model().state()?)?),
            "model.list" => Ok(serde_json::to_value(self.runtime.model().list()?)?),
            "process.start" => Ok(serde_json::to_value(
                self.runtime.start_process(request.args.clone()).await?,
            )?),
            "process.list" => Ok(serde_json::to_value(self.runtime.processes().list()?)?),
            "process.status" => Ok(serde_json::to_value(
                self.runtime.processes().status(field(args, "id")?)?,
            )?),
            "process.wait" => Ok(serde_json::to_value(
                self.runtime.processes().wait(field(args, "id")?).await?,
            )?),
            "process.cancel" => Ok(serde_json::to_value(
                self.runtime.processes().cancel(field(args, "id")?).await?,
            )?),
            "backup" => {
                let name = field(args, "name")?;
                ensure!(
                    !name.is_empty()
                        && name.len() <= 128
                        && name.bytes().all(|byte| byte.is_ascii_alphanumeric()
                            || matches!(byte, b'-' | b'_' | b'.'))
                        && name != "."
                        && name != "..",
                    "backup name must be a filename"
                );
                tokio::fs::create_dir_all(&self.backup_directory).await?;
                let destination = self.backup_directory.join(name);
                let store = self.store.clone();
                Ok(serde_json::to_value(
                    tokio::task::spawn_blocking(move || {
                        loom_maintenance::backup(&store, &destination)
                    })
                    .await??,
                )?)
            }
            "compact" => {
                let store = self.store.clone();
                let through = args["through_seq"]
                    .as_i64()
                    .context("through_seq required")?;
                let limit = args["limit"]
                    .as_u64()
                    .context("limit required")?
                    .try_into()?;
                Ok(serde_json::to_value(
                    tokio::task::spawn_blocking(move || store.compact_log(through, limit))
                        .await??,
                )?)
            }
            "machine.create" => Ok(serde_json::to_value(
                self.runtime
                    .create_machine(std::path::Path::new(field(args, "root")?))?,
            )?),
            "stats" => Ok(serde_json::to_value(loom_maintenance::stats(&self.store)?)?),
            "gc" => Ok(serde_json::to_value(
                loom_maintenance::collect_effect_index(
                    &self.store,
                    args["limit"]
                        .as_u64()
                        .context("limit required")?
                        .try_into()?,
                )?,
            )?),
            "cache_evict" => Ok(serde_json::to_value(
                loom_maintenance::evict_build_cache(
                    &self.builder,
                    &loom_maintenance::CachePolicy {
                        max_bytes: args["max_bytes"].as_u64().context("max_bytes required")?,
                        max_age: Duration::from_secs(
                            args["max_age_secs"]
                                .as_u64()
                                .context("max_age_secs required")?,
                        ),
                        max_entries: args["max_entries"]
                            .as_u64()
                            .context("max_entries required")?
                            .try_into()?,
                    },
                )
                .await?,
            )?),
            "defs" => {
                let mut defs = Vec::new();
                for def in self.store.definitions()? {
                    let size = def
                        .component_hash
                        .as_deref()
                        .map(|h| self.store.get(h))
                        .transpose()?
                        .flatten()
                        .map(|b| b.len());
                    let name = self.store.definition_name(&def.hash)?;
                    let mut value = serde_json::to_value(def)?;
                    value["name"] = json!(name);
                    value["component_size"] = json!(size);
                    defs.push(value);
                }
                Ok(json!(defs))
            }
            "build" => self.build_record(field(args, "hash")?),
            "actors" => Ok(serde_json::to_value(self.store.actors()?)?),
            "events" => Ok(serde_json::to_value(self.store.events(
                args["actor"].as_str(),
                args["after"].as_i64().unwrap_or(0),
                args["limit"].as_u64().unwrap_or(1000).min(1000) as usize,
            )?)?),
            "state" => self.runtime.state(&self.command_actor(&request)?).await,
            "spawn" => Ok(serde_json::to_value(
                self.runtime
                    .spawn(field(args, "hash")?, args["initial"].clone())
                    .await?,
            )?),
            "send" => {
                self.runtime
                    .send(&self.command_actor(&request)?, args["msg"].clone())
                    .await
            }
            "fork" => {
                let actor = self.command_actor(&request)?;
                let fork = self.runtime.fork_actor(&actor).await?;
                if args["session"].is_string()
                    || (!args["actor"].is_string() && request.session.is_some())
                {
                    let session = uuid::Uuid::new_v4().to_string();
                    self.store.create_session(&session, &fork.id, "owner")?;
                    Ok(json!({"session":session,"actor":fork}))
                } else {
                    Ok(serde_json::to_value(fork)?)
                }
            }
            "upgrade" => {
                self.runtime
                    .upgrade(&self.command_actor(&request)?, field(args, "hash")?)
                    .await
            }
            "call" => {
                self.runtime
                    .call_def(field(args, "hash")?, args["args"].clone())
                    .await
            }
            "resolve" => {
                let hash = field(args, "hash")?;
                if let Some(def) = self.store.resolve(hash)? {
                    Ok(serde_json::to_value(def)?)
                } else {
                    self.store
                        .get_json(hash)?
                        .context("CAS value not found or not JSON")
                }
            }
            "deps" => Ok(serde_json::to_value(
                self.store.dependencies(field(args, "hash")?)?,
            )?),
            _ => bail!("unknown command {}", request.command),
        }
    }
    fn command_actor(&self, request: &CommandRequest) -> Result<String> {
        if let Some(actor) = request.args["actor"].as_str() {
            return Ok(actor.to_owned());
        }
        let session = request.args["session"]
            .as_str()
            .or(request.session.as_deref())
            .context("actor or session required")?;
        self.store.session(session)?.context("session not found")
    }
    pub fn build_record(&self, hash: &str) -> Result<Value> {
        let mut after = 0;
        loop {
            let events = self.store.events(Some("system"), after, 1000)?;
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
            match self.store.put("result", &bytes) {
                Ok(hash) => response.result = json!({"$ref":hash,"size":bytes.len()}),
                Err(error) => return self.response(Err(error)),
            }
        }
        response
    }
}
fn field<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    args[name]
        .as_str()
        .with_context(|| format!("missing string argument {name}"))
}
#[derive(Clone)]
struct ApiState {
    service: Arc<Service>,
    authorizer: Authorizer,
}
pub fn router(service: Arc<Service>, authorizer: Authorizer) -> Router {
    let state = ApiState {
        service,
        authorizer,
    };
    Router::new()
        .route(
            "/v1/define",
            post(define).layer(DefaultBodyLimit::max(16 * 1024 * 1024)),
        )
        .route("/v1/eval", post(eval))
        .route("/v1/command", post(command))
        .route("/v1/cas/{hash}", get(cas))
        .route("/v1/events", get(events))
        .route("/v1/defs/{name}", get(definition))
        .route("/v1/graph/deps/{hash}", get(deps))
        .route("/v1/builds/{hash}", get(build))
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .route_layer(middleware::from_fn_with_state(
            state.authorizer.clone(),
            authorize_token,
        ))
        .route("/v1/stream", get(stream))
        .route("/health", get(|| async { Json(json!({"ok":true})) }))
        .with_state(state)
}
pub fn protect(router: Router, authorizer: Authorizer) -> Router {
    router.layer(middleware::from_fn_with_state(authorizer, authorize_token))
}
async fn authorize_token(
    State(authorizer): State<Authorizer>,
    mut request: Request<axum::body::Body>,
    next: Next,
) -> HttpResponse {
    let access = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .and_then(|token| authorizer.authenticate(token));
    let Some(access) = access else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if request.method() == axum::http::Method::GET && !access.allows(Scope::Read) {
        return StatusCode::FORBIDDEN.into_response();
    }
    request.extensions_mut().insert(access);
    next.run(request).await
}
fn operation_response(service: &Service, response: Response) -> HttpResponse {
    let forbidden = response.result["code"] == "forbidden";
    let mut response = Json(service.inline(response)).into_response();
    if forbidden {
        *response.status_mut() = StatusCode::FORBIDDEN;
    }
    response
}
async fn define(State(s): State<ApiState>, request: Request<axum::body::Body>) -> HttpResponse {
    let service = s.service.scoped(
        request
            .extensions()
            .get::<Access>()
            .cloned()
            .unwrap_or_default(),
    );
    let bytes = match axum::body::to_bytes(request.into_body(), 16 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(error) => {
            let mut response = Json(service.response(Err(error.into()))).into_response();
            *response.status_mut() = StatusCode::PAYLOAD_TOO_LARGE;
            return response;
        }
    };
    let request: DefineRequest = match serde_json::from_slice(&bytes) {
        Ok(request) => request,
        Err(error) => {
            let mut response = Json(service.response(Err(error.into()))).into_response();
            *response.status_mut() = StatusCode::BAD_REQUEST;
            return response;
        }
    };
    if request.lang == Lang::Ts && bytes.len() > 1024 * 1024 {
        let mut response =
            Json(service.response(Err(anyhow::anyhow!("TypeScript request exceeds 1 MiB"))))
                .into_response();
        *response.status_mut() = StatusCode::PAYLOAD_TOO_LARGE;
        return response;
    }
    operation_response(&service, service.define(request).await)
}
async fn eval(
    State(s): State<ApiState>,
    axum::Extension(access): axum::Extension<Access>,
    request: Result<Json<EvalRequest>, axum::extract::rejection::JsonRejection>,
) -> HttpResponse {
    let service = s.service.scoped(access);
    match request {
        Ok(Json(request)) => operation_response(&service, service.eval(request).await),
        Err(error) => json_rejection(&service, error),
    }
}
async fn command(
    State(s): State<ApiState>,
    axum::Extension(access): axum::Extension<Access>,
    request: Result<Json<CommandRequest>, axum::extract::rejection::JsonRejection>,
) -> HttpResponse {
    let service = s.service.scoped(access);
    match request {
        Ok(Json(request)) => operation_response(&service, service.command(request).await),
        Err(error) => json_rejection(&service, error),
    }
}
fn json_rejection(
    service: &Service,
    error: axum::extract::rejection::JsonRejection,
) -> HttpResponse {
    let status = error.status();
    let mut response =
        Json(service.response(Err(anyhow::anyhow!(error.body_text())))).into_response();
    *response.status_mut() = status;
    response
}
async fn cas(State(s): State<ApiState>, Path(hash): Path<String>) -> HttpResponse {
    match s.service.store.get(&hash) {
        Ok(Some(bytes)) => bytes.into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => Json(s.service.response(Err(e))).into_response(),
    }
}
#[derive(Deserialize)]
struct EventQuery {
    actor: Option<String>,
    #[serde(default)]
    after: i64,
    limit: Option<usize>,
}
async fn events(State(s): State<ApiState>, Query(q): Query<EventQuery>) -> Json<Response> {
    Json(
        s.service.response(
            s.service
                .store
                .events(
                    q.actor.as_deref(),
                    q.after,
                    q.limit.unwrap_or(1000).min(1000),
                )
                .and_then(|v| Ok(serde_json::to_value(v)?)),
        ),
    )
}
async fn definition(State(s): State<ApiState>, Path(name): Path<String>) -> Json<Response> {
    Json(
        s.service.response(
            s.service
                .store
                .resolve(&name)
                .and_then(|v| Ok(serde_json::to_value(v.context("definition not found")?)?)),
        ),
    )
}
async fn deps(State(s): State<ApiState>, Path(hash): Path<String>) -> Json<Response> {
    Json(
        s.service.response(
            s.service
                .store
                .dependencies(&hash)
                .and_then(|v| Ok(serde_json::to_value(v)?)),
        ),
    )
}
async fn build(State(s): State<ApiState>, Path(hash): Path<String>) -> Json<Response> {
    Json(s.service.response(s.service.build_record(&hash)))
}
async fn stream(State(s): State<ApiState>, ws: WebSocketUpgrade) -> HttpResponse {
    ws.max_message_size(4096)
        .on_upgrade(move |socket| stream_events(s, socket))
}
#[derive(Deserialize)]
struct Subscription {
    token: String,
    #[serde(default)]
    after: i64,
    actor: Option<String>,
}
async fn stream_events(s: ApiState, mut socket: WebSocket) {
    let request = tokio::time::timeout(Duration::from_secs(5), socket.recv()).await;
    let Ok(Some(Ok(Message::Text(text)))) = request else {
        return;
    };
    let Ok(mut subscription) = serde_json::from_str::<Subscription>(&text) else {
        return;
    };
    if s.authorizer
        .authenticate(&subscription.token)
        .is_none_or(|access| !access.allows(Scope::Read))
    {
        return;
    }
    if socket
        .send(Message::Text("{\"ok\":true}".into()))
        .await
        .is_err()
    {
        return;
    }
    let mut interval = tokio::time::interval(Duration::from_millis(100));
    loop {
        tokio::select! {_ = interval.tick()=>{let Ok(events)=s.service.store.events(subscription.actor.as_deref(),subscription.after,1000) else{return};for event in events{subscription.after=event.seq;let Ok(text)=serde_json::to_string(&event)else{return};if socket.send(Message::Text(text.into())).await.is_err(){return}}},message=socket.recv()=>match message{Some(Ok(Message::Ping(bytes)))=>{if socket.send(Message::Pong(bytes)).await.is_err(){return}},Some(Ok(Message::Close(_)))|None|Some(Err(_))=>return,_=>{}}}
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;
    fn app() -> Router {
        router(
            Arc::new(
                Service::new(
                    Store::memory().unwrap(),
                    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
                    vec![Lang::Ts, Lang::Rust],
                )
                .unwrap(),
            ),
            Authorizer::single("test-secret".into()).unwrap(),
        )
    }
    #[tokio::test]
    async fn ts_define_is_lazy_and_eval_rejections_remain_failures() {
        let service = Service::new(
            Store::memory().unwrap(),
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
            vec![Lang::Ts],
        )
        .unwrap();
        let rejected = service
            .eval(EvalRequest {
                session: None,
                source: "fetch('https://example.com')".into(),
                deps: BTreeMap::new(),
            })
            .await;
        assert!(!rejected.ok);
        assert!(!rejected.diagnostics.is_empty());
        let request = DefineRequest {
            lang: Lang::Ts,
            name: "lazy".into(),
            source: "export function main(): number { return 42; }".into(),
            deps: BTreeMap::new(),
        };
        let accepted = service.define(request.clone()).await;
        assert!(accepted.ok, "{accepted:?}");
        assert!(accepted.result["def"]["component_hash"].is_null());
        assert_eq!(accepted.result["build"]["status"], "pending");
        let mut reformatted = request;
        reformatted.source = "export function main():number{\nreturn 42;\n}".into();
        let same = service.define(reformatted).await;
        assert!(same.ok, "{same:?}");
        assert_eq!(accepted.result["def"]["hash"], same.result["def"]["hash"]);
    }
    #[tokio::test]
    async fn redefinition_rehashes_typed_dependents_and_rejects_wrong_arguments() {
        let service = Service::new(
            Store::memory().unwrap(),
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
            vec![Lang::Ts],
        )
        .unwrap();
        let first = service
            .define(DefineRequest {
                lang: Lang::Ts,
                name: "add".into(),
                source: "export function main(x:number):number {return x+1;}".into(),
                deps: BTreeMap::new(),
            })
            .await;
        assert!(first.ok, "{first:?}");
        let hash = first.result["def"]["hash"].as_str().unwrap().to_owned();
        let mut deps = BTreeMap::new();
        deps.insert("add".into(), hash);
        let mut dependent=DefineRequest{lang:Lang::Ts,name:"caller".into(),source:"import {call} from 'loom'; import {add} from 'loom:defs'; export function main():unknown {return call(add,[41]);}".into(),deps};
        let before = service.define(dependent.clone()).await;
        assert!(before.ok, "{before:?}");
        dependent.name = "bad-caller".into();
        dependent.source = dependent.source.replace("[41]", "['wrong']");
        let rejected = service.define(dependent).await;
        assert!(!rejected.ok, "{rejected:?}");
        let second = service
            .define(DefineRequest {
                lang: Lang::Ts,
                name: "add".into(),
                source: "export function main(x:number):number {return x+2;}".into(),
                deps: BTreeMap::new(),
            })
            .await;
        assert!(second.ok, "{second:?}");
        assert_eq!(second.result["rehashed"].as_array().unwrap().len(), 1);
        let current = service.store.resolve("caller").unwrap().unwrap();
        assert_ne!(current.hash, before.result["def"]["hash"].as_str().unwrap());
        assert_eq!(
            service.store.definition_deps(&current.hash).unwrap()["add"],
            second.result["def"]["hash"].as_str().unwrap()
        );
    }
    #[tokio::test]
    async fn warm_ts_define_median_is_under_200_ms() {
        let service = Service::new(
            Store::memory().unwrap(),
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
            vec![Lang::Ts],
        )
        .unwrap();
        let mut durations = Vec::new();
        for index in 0..22 {
            let started = std::time::Instant::now();
            let response = service
                .define(DefineRequest {
                    lang: Lang::Ts,
                    name: "latency".into(),
                    source: format!("export function main(x:number):number {{return x+{index};}}"),
                    deps: BTreeMap::new(),
                })
                .await;
            assert!(response.ok, "{response:?}");
            if index >= 2 {
                durations.push(started.elapsed());
            }
        }
        durations.sort();
        let median = durations[durations.len() / 2];
        println!(
            "TS define p50={}ms n={}",
            median.as_millis(),
            durations.len()
        );
        assert!(
            median < Duration::from_millis(200),
            "TS define p50={median:?}"
        );
    }
    #[test]
    fn rust_macro_source_is_not_a_bundle_reference() {
        assert_eq!(
            source_reference("#[loom::def] pub fn add(x:i32)->i32{x+1}"),
            None
        );
        assert_eq!(
            source_reference("#![allow(dead_code)]\npub fn main() {}"),
            None
        );
        let reference = format!("#{}", "a".repeat(64));
        assert_eq!(source_reference(&reference), Some(&reference[1..]));
    }
    #[tokio::test]
    async fn missing_source_archive_reports_missing_reference() {
        let service = Service::new(
            Store::memory().unwrap(),
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
            vec![Lang::Rust],
        )
        .unwrap();
        let response = service
            .define(DefineRequest {
                lang: Lang::Rust,
                name: "missing".into(),
                source: format!("#{}", "a".repeat(64)),
                deps: BTreeMap::new(),
            })
            .await;
        assert!(!response.ok);
        assert!(
            response.result["error"]
                .as_str()
                .unwrap()
                .contains("Rust source bundle not found")
        );
        let source = "#[loom::def] pub fn main() { std::fs::read(\"secret\").unwrap(); }";
        let checked = service
            .define(DefineRequest {
                lang: Lang::Rust,
                name: "macro".into(),
                source: source.into(),
                deps: BTreeMap::new(),
            })
            .await;
        assert!(!checked.ok);
        assert!(!checked.diagnostics.is_empty(), "{checked:?}");
    }
    #[tokio::test]
    async fn read_scope_cannot_execute_or_define_through_service_or_http() {
        let service = Arc::new(
            Service::new(
                Store::memory().unwrap(),
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
                vec![Lang::Ts],
            )
            .unwrap(),
        );
        let authorizer = Authorizer::new(vec![TokenConfig {
            token: "reader".into(),
            scopes: [Scope::Read].into_iter().collect(),
        }])
        .unwrap();
        let reader = service.scoped(authorizer.authenticate("reader").unwrap());
        let response = reader
            .eval(EvalRequest {
                session: None,
                source: "42".into(),
                deps: BTreeMap::new(),
            })
            .await;
        assert!(!response.ok);
        assert_eq!(response.result["code"], "forbidden");
        let call = reader
            .command(CommandRequest {
                session: None,
                command: "call".into(),
                args: json!({"hash":"missing","args":[]}),
            })
            .await;
        assert_eq!(call.result["code"], "forbidden");
        assert_eq!(service.store.latest_seq().unwrap(), 0);
        let response = router(service, authorizer)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/define")
                    .header("authorization", "Bearer reader")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        r#"{"name":"denied","source":"export function main(){return 42;}"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    #[test]
    fn source_archive_preserves_binary_assets_and_rejects_escape_paths() {
        fn append(builder: &mut tar::Builder<Vec<u8>>, path: &str, bytes: &[u8]) {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_entry_type(tar::EntryType::Regular);
            header.as_mut_bytes()[..path.len()].copy_from_slice(path.as_bytes());
            header.set_cksum();
            builder.append(&header, bytes).unwrap();
        }
        let mut archive = tar::Builder::new(Vec::new());
        append(
            &mut archive,
            "Cargo.toml",
            b"[package]\nname='asset'\nversion='0.1.0'\n",
        );
        append(&mut archive, "src/lib.rs", b"pub fn answer()->u32 {42}");
        append(&mut archive, "assets/raw", &[0xff, 0, 0xfe]);
        let bytes = archive.into_inner().unwrap();
        let decoded: loom_check::SourceBundle =
            serde_json::from_str(&decode_source_bundle(&bytes).unwrap()).unwrap();
        assert_eq!(
            decoded.files["assets/raw"].bytes().unwrap(),
            vec![0xff, 0, 0xfe]
        );
        let mut escaped = tar::Builder::new(Vec::new());
        append(&mut escaped, "../outside", b"bad");
        assert!(
            decode_source_bundle(&escaped.into_inner().unwrap())
                .unwrap_err()
                .to_string()
                .contains("invalid source archive path")
        );
    }
    #[tokio::test]
    async fn auth_gates_every_operation_and_health_is_public() {
        for path in [
            "/v1/define",
            "/v1/eval",
            "/v1/command",
            "/v1/events",
            "/v1/cas/hash",
        ] {
            let response = app()
                .oneshot(
                    Request::builder()
                        .method(if path == "/v1/events" || path.starts_with("/v1/cas") {
                            "GET"
                        } else {
                            "POST"
                        })
                        .uri(path)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
        }
        let response = app()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    #[tokio::test]
    async fn unknown_commands_fail_and_store_queries_work() {
        use http_body_util::BodyExt;
        for command in ["undefined", "actors"] {
            let response = app()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/command")
                        .header("authorization", "Bearer test-secret")
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(
                            serde_json::to_vec(&json!({"command":command,"args":{}})).unwrap(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body: Response =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            assert_eq!(body.ok, command == "actors");
        }
    }
}

fn decode_source_bundle(bytes: &[u8]) -> Result<String> {
    use std::io::Read;
    ensure!(
        bytes.len() <= 16 * 1024 * 1024,
        "Rust source archive exceeds 16 MB"
    );
    let mut archive = tar::Archive::new(bytes);
    let mut files = BTreeMap::new();
    let mut total = 0_u64;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            continue;
        }
        ensure!(
            kind.is_file(),
            "Rust source archive permits regular files only"
        );
        let path = entry.path()?.into_owned();
        ensure!(
            path.components()
                .all(|part| matches!(part, std::path::Component::Normal(_))),
            "invalid source archive path"
        );
        ensure!(files.len() < 1024, "source archive exceeds 1024 files");
        total = total
            .checked_add(entry.size())
            .context("source archive size overflow")?;
        ensure!(
            total <= 16 * 1024 * 1024,
            "expanded source archive exceeds 16 MB"
        );
        let path = path
            .to_str()
            .context("source paths must be UTF-8")?
            .to_string();
        ensure!(
            path != "target" && !path.starts_with("target/"),
            "source archive contains build artifacts"
        );
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        let source = loom_check::SourceFile::from_bytes(bytes);
        ensure!(
            files.insert(path, source).is_none(),
            "duplicate archive path"
        );
    }
    ensure!(
        files.contains_key("Cargo.toml") && files.contains_key("src/lib.rs"),
        "source archive needs Cargo.toml and src/lib.rs"
    );
    Ok(serde_json::to_string(&json!({"files":files}))?)
}

struct BuildResolver {
    store: Store,
    builder: Arc<loom_build::Builder>,
    gate: tokio::sync::Mutex<()>,
}
impl loom_rt::ComponentResolver for BuildResolver {
    fn ensure_built<'a>(
        &'a self,
        hash: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let _guard = self.gate.lock().await;
            let mut def = self
                .store
                .definition(hash)?
                .context("definition not found")?;
            if def.component_hash.is_some() {
                return Ok(());
            }
            let checked = stored_definition(&self.store, hash)?;
            let dependencies = dependency_closure(&self.store, &checked.deps)?;
            let built = self
                .builder
                .build_with_dependencies(&checked, &dependencies)
                .await?;
            ensure!(
                built.diagnostics.is_empty(),
                "component build diagnostics: {}",
                serde_json::to_string(&built.diagnostics)?
            );
            ensure!(
                !built.component.is_empty(),
                "builder returned empty component"
            );
            let component_hash = self.store.put("component", &built.component)?;
            let logs_ref = self.store.put("blob", built.logs.as_bytes())?;
            def.component_hash = Some(component_hash.clone());
            self.store
                .define(&def, None, &checked.source, &checked.deps)?;
            self.store.append("system",&json!({"type":"component_built","component_hash":component_hash,"logs_ref":logs_ref,"ms":built.ms,"size":built.component.len()}),0)?;
            Ok(())
        })
    }
}
fn stored_definition(store: &Store, hash: &str) -> Result<loom_check::CheckedDef> {
    let def = store
        .definition(hash)?
        .context("dependency definition missing")?;
    Ok(loom_check::CheckedDef {
        hash: def.hash,
        lang: def.lang,
        name: store.definition_name(hash)?.unwrap_or_else(|| hash.into()),
        source: store.source(hash)?.context("definition source missing")?,
        deps: store.definition_deps(hash)?,
        sig: def.sig,
        diagnostics: vec![],
    })
}
fn dependency_closure(
    store: &Store,
    deps: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, loom_check::CheckedDef>> {
    let mut pending: Vec<String> = deps.values().cloned().collect();
    let mut closure = BTreeMap::new();
    while let Some(hash) = pending.pop() {
        if closure.contains_key(&hash) {
            continue;
        }
        ensure!(
            closure.len() < 1024,
            "definition closure exceeds 1024 definitions"
        );
        let checked = stored_definition(store, &hash)?;
        pending.extend(checked.deps.values().cloned());
        closure.insert(hash, checked);
    }
    Ok(closure)
}

#[derive(Default)]
struct Redefinitions {
    rehashed: Vec<Value>,
    stale_actors: Vec<String>,
}

fn dependency_signatures(
    store: &Store,
    deps: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, loom_proto::TypeSig>> {
    let mut signatures = BTreeMap::new();
    for entry in deps {
        signatures.insert(
            entry.0.clone(),
            store
                .definition(entry.1)?
                .context("dependency signature not found")?
                .sig,
        );
    }
    Ok(signatures)
}

fn source_reference(source: &str) -> Option<&str> {
    source
        .strip_prefix('#')
        .filter(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn valid_alias(alias: &str) -> bool {
    let mut bytes = alias.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'$'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$'))
}
