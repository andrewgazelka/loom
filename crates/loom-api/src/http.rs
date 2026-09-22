mod upload;
use super::*;
pub(crate) use crate::tenant::TenantService;

#[derive(Clone)]
struct ApiState {
    directory: ServiceDirectory,
    authorizer: Authorizer,
}
pub fn router(directory: impl Into<ServiceDirectory>, authorizer: Authorizer) -> Router {
    let directory = directory.into();
    let authorizer = directory.authorizer(authorizer);
    let state = ApiState {
        directory,
        authorizer,
    };
    Router::new()
        .route("/v1/command", post(command))
        .route("/v1/ingress", post(ingress))
        .route("/v1/cas/{hash}", get(cas))
        .route("/v1/wasm/{hash}", get(wasm))
        .route("/v1/cas", post(upload::upload))
        .route("/v1/events", get(events))
        .route("/v1/defs/{name}", get(definition))
        .route("/v1/graph/deps/{hash}", get(deps))
        .route("/v1/builds/{hash}", get(build))
        .route("/v1/builds/active", get(active_build))
        .route("/v1/stream", get(stream))
        .route(
            "/v1/actors/{name}/websocket",
            get(crate::actor_websocket::upgrade),
        )
        .route("/health", get(|| async { Json(json!({"ok":true})) }))
        .layer(DefaultBodyLimit::max(16 * 1024 * 1024))
        .route_layer(middleware::from_fn_with_state(
            state.directory.clone(),
            select_tenant,
        ))
        .route_layer(middleware::from_fn_with_state(
            state.authorizer.clone(),
            authorize_token,
        ))
        .with_state(state)
}
pub fn protect(router: Router, authorizer: Authorizer) -> Router {
    router.layer(middleware::from_fn_with_state(authorizer, authorize_token))
}
/// Public assets still reject credentials reserved for cluster ingress.
pub fn protect_public(router: Router, mut authorizer: Authorizer) -> Router {
    authorizer.public = true;
    protect(router, authorizer)
}
async fn authorize_token(
    State(authorizer): State<Authorizer>,
    mut request: Request<axum::body::Body>,
    next: Next,
) -> HttpResponse {
    let header_token = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_owned);
    let actor_websocket = request.method() == axum::http::Method::GET
        && request.uri().path().starts_with("/v1/actors/")
        && request.uri().path().ends_with("/websocket");
    let browser_token = if actor_websocket {
        match crate::actor_websocket::browser_bearer(request.headers()) {
            Ok(token) => token,
            Err(_) => return StatusCode::BAD_REQUEST.into_response(),
        }
    } else {
        None
    };
    if browser_token.is_some() && request.headers().contains_key("authorization") {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let credential = header_token.or(browser_token);
    let token = credential.as_deref();
    let ingress_route = request.uri().path() == "/v1/ingress";
    if token.is_some_and(|token| authorizer.is_ingress(token)) {
        return if ingress_route {
            let Some(tenant) = token.and_then(|token| authorizer.ingress_tenant(token)) else {
                return StatusCode::FORBIDDEN.into_response();
            };
            request
                .extensions_mut()
                .insert(Access::owner().for_tenant(tenant));
            next.run(request).await
        } else {
            StatusCode::FORBIDDEN.into_response()
        };
    }
    if authorizer.public || matches!(request.uri().path(), "/health" | "/v1/stream") {
        return next.run(request).await;
    }
    let access = token.and_then(|token| authorizer.authenticate(token));
    if ingress_route {
        return if access.is_some() {
            StatusCode::FORBIDDEN
        } else {
            StatusCode::UNAUTHORIZED
        }
        .into_response();
    }
    let Some(access) = access else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if request.method() == axum::http::Method::GET {
        let scope = if request.uri().path().starts_with("/v1/actors/")
            && request.uri().path().ends_with("/websocket")
        {
            Scope::Execute
        } else {
            Scope::Read
        };
        if !access.allows(scope) {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    request.extensions_mut().insert(access);
    next.run(request).await
}
async fn select_tenant(
    State(directory): State<ServiceDirectory>,
    mut request: Request<axum::body::Body>,
    next: Next,
) -> HttpResponse {
    if matches!(request.uri().path(), "/health" | "/v1/stream") {
        return next.run(request).await;
    }
    let Some(access) = request.extensions().get::<Access>().cloned() else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Ok(service) = directory.scoped(access) else {
        return StatusCode::FORBIDDEN.into_response();
    };
    request.extensions_mut().insert(TenantService {
        service: Arc::new(service),
    });
    next.run(request).await
}

#[derive(Deserialize)]
struct IngressRequest {
    ops: Vec<loom_actor::DeliveryOp>,
}
async fn ingress(
    axum::Extension(s): axum::Extension<TenantService>,
    Json(request): Json<IngressRequest>,
) -> HttpResponse {
    let Some(actors) = &s.service.actors else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let mut acks = Vec::new();
    let mut status = StatusCode::OK;
    for op in request.ops {
        let ack = match op {
            loom_actor::DeliveryOp::Command { target, verb, args } => {
                actors.ingress_command(target, verb, args).await
            }
            op => {
                let Some(ack) = actors.node.apply_ingress(vec![op]).await.into_iter().next() else {
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                };
                ack
            }
        };
        let failed = !ack.ok;
        if failed {
            status = if ack.conflict {
                StatusCode::CONFLICT
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
        }
        acks.push(ack);
        if failed {
            break;
        }
    }
    let mut response = Json(loom_actor::IngressResponse::new(acks)).into_response();
    *response.status_mut() = status;
    response
}
fn protocol_response(response: Response) -> HttpResponse {
    let forbidden = response.result["code"] == "forbidden";
    let mut response = Json(response).into_response();
    if forbidden {
        *response.status_mut() = StatusCode::FORBIDDEN;
    }
    response
}
async fn command(
    axum::Extension(s): axum::Extension<TenantService>,
    request: Result<Json<CommandRequest>, axum::extract::rejection::JsonRejection>,
) -> HttpResponse {
    let service = s.service;
    match request {
        Ok(Json(request)) => protocol_response(service.command(request).await),
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
async fn cas(
    axum::Extension(s): axum::Extension<TenantService>,
    Path(hash): Path<String>,
    headers: HeaderMap,
) -> HttpResponse {
    let result = (|| -> Result<Option<CasBlock>> {
        s.service.store.flush()?;
        let Some(codec) = s.service.store.codec(&hash)? else {
            return Ok(None);
        };
        let bytes = s
            .service
            .store
            .get(&hash)?
            .context("CAS block disappeared")?;
        Ok(Some(CasBlock { codec, bytes }))
    })();
    match result {
        Ok(Some(block)) => {
            let wants_json = headers
                .get(axum::http::header::ACCEPT)
                .and_then(|header| header.to_str().ok())
                .is_some_and(|accept| {
                    accept
                        .split(',')
                        .any(|item| item.trim().split(';').next() == Some("application/json"))
                });
            let mut response = if wants_json {
                if block.codec != loom_proto::DAG_CBOR_CODEC {
                    let mut failure = s.service.response(Err(anyhow::anyhow!(
                        "raw CAS blocks have no JSON representation"
                    )));
                    failure.result["code"] = json!("unsupported_representation");
                    let mut response = Json(failure).into_response();
                    *response.status_mut() = StatusCode::NOT_ACCEPTABLE;
                    return response;
                }
                match loom_proto::decode::<Value>(&block.bytes) {
                    Ok(value) => Json(value).into_response(),
                    Err(error) => {
                        let mut response = Json(s.service.response(Err(anyhow::Error::msg(error))))
                            .into_response();
                        *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                        return response;
                    }
                }
            } else {
                let mut response = block.bytes.into_response();
                response.headers_mut().insert(
                    axum::http::header::CONTENT_TYPE,
                    axum::http::HeaderValue::from_static(
                        if block.codec == loom_proto::DAG_CBOR_CODEC {
                            "application/vnd.ipld.dag-cbor"
                        } else {
                            "application/octet-stream"
                        },
                    ),
                );
                response
            };
            response.headers_mut().insert(
                axum::http::header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("private, max-age=31536000, immutable"),
            );
            response.headers_mut().insert(
                axum::http::header::VARY,
                axum::http::HeaderValue::from_static("Accept, Authorization"),
            );
            response
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => {
            let mut response = Json(s.service.response(Err(error))).into_response();
            *response.status_mut() = StatusCode::BAD_REQUEST;
            response
        }
    }
}
struct CasBlock {
    codec: u64,
    bytes: Vec<u8>,
}

enum Fetched {
    Missing,
    TooLarge(u64),
    Bytes(Vec<u8>),
}

/// A build artifact as WebAssembly text joined to its source lines
/// (`crate::wasm`). Read scope like every GET. An unknown hash is 404 and a
/// module over `MAX_MODULE_BYTES` is 413, both in the command error envelope;
/// bytes that are not a core module, or DWARF that does not parse, are 422.
async fn wasm(
    axum::Extension(s): axum::Extension<TenantService>,
    Path(hash): Path<String>,
) -> HttpResponse {
    let service = &s.service;
    let fetched = (|| -> Result<Fetched> {
        // Shape first: a durable barrier per request is not something a
        // malformed path should be able to trigger.
        ensure!(
            hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "artifact hash must be 64 hex characters"
        );
        service.store.flush()?;
        let Some(entry) = service.store.cas_entry(&hash)? else {
            return Ok(Fetched::Missing);
        };
        if entry.size > crate::wasm::MAX_MODULE_BYTES {
            return Ok(Fetched::TooLarge(entry.size));
        }
        let bytes = service.store.get(&hash)?.context("CAS block disappeared")?;
        Ok(Fetched::Bytes(bytes))
    })();
    let envelope = |status: StatusCode, error: anyhow::Error| {
        let mut response = Json(service.response(Err(error))).into_response();
        *response.status_mut() = status;
        response
    };
    let bytes = match fetched {
        Ok(Fetched::Bytes(bytes)) => bytes,
        Ok(Fetched::Missing) => {
            return envelope(
                StatusCode::NOT_FOUND,
                anyhow::anyhow!("build artifact {hash} not found"),
            );
        }
        Ok(Fetched::TooLarge(size)) => {
            return envelope(
                StatusCode::PAYLOAD_TOO_LARGE,
                anyhow::anyhow!(
                    "module {hash} is {size} bytes; the text view stops at {} bytes",
                    crate::wasm::MAX_MODULE_BYTES
                ),
            );
        }
        Err(error) => return envelope(StatusCode::BAD_REQUEST, error),
    };
    // Printing a module is CPU work proportional to its size; keep it off the
    // request executor.
    let view = tokio::task::spawn_blocking(move || crate::wasm::wasm_view(&bytes))
        .await
        .map_err(anyhow::Error::from)
        .and_then(|view| view);
    let view = match view {
        Ok(mut view) => {
            match crate::wasm::compiled_source(&service.store, &hash) {
                Ok((text, wrapper_line)) => {
                    view.compiled_source = Some(text);
                    view.compiled_wrapper_line = Some(wrapper_line);
                }
                Err(error) => view.compiled_source_error = Some(format!("{error:#}")),
            }
            Ok(view)
        }
        Err(error) => Err(error),
    };
    match view {
        Ok(view) => {
            // The whole body is a function of the artifact once its compiled
            // text is recorded; a body missing that text may gain it later
            // and must not be kept.
            let cache = if view.compiled_source_error.is_some() {
                "no-store"
            } else {
                "private, max-age=31536000, immutable"
            };
            let mut response = Json(view).into_response();
            response.headers_mut().insert(
                axum::http::header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static(cache),
            );
            response
        }
        Err(error) => envelope(StatusCode::UNPROCESSABLE_ENTITY, error),
    }
}

#[derive(Deserialize)]
struct EventQuery {
    #[serde(default)]
    after: i64,
    limit: Option<usize>,
}
async fn events(
    axum::Extension(s): axum::Extension<TenantService>,
    Query(q): Query<EventQuery>,
) -> Json<Response> {
    Json(
        s.service.response(
            s.service
                .store
                .definition_events(q.after, q.limit.unwrap_or(1000).min(1000))
                .and_then(|v| Ok(serde_json::to_value(v)?)),
        ),
    )
}
async fn definition(
    axum::Extension(s): axum::Extension<TenantService>,
    Path(name): Path<String>,
) -> Json<Response> {
    Json(
        s.service.response(
            s.service
                .store
                .resolve(&name)
                .and_then(|v| Ok(serde_json::to_value(v.context("definition not found")?)?)),
        ),
    )
}
async fn deps(
    axum::Extension(s): axum::Extension<TenantService>,
    Path(hash): Path<String>,
) -> Json<Response> {
    Json(
        s.service.response(
            s.service
                .store
                .dependencies(&hash)
                .and_then(|v| Ok(serde_json::to_value(v)?)),
        ),
    )
}
async fn build(
    axum::Extension(s): axum::Extension<TenantService>,
    Path(hash): Path<String>,
) -> Json<Response> {
    Json(s.service.response(s.service.build_record(&hash)))
}
async fn active_build(axum::Extension(s): axum::Extension<TenantService>) -> Json<Response> {
    Json(s.service.response(Ok(s.service.build_progress.snapshot())))
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
}
async fn stream_events(s: ApiState, mut socket: WebSocket) {
    let request = tokio::time::timeout(Duration::from_secs(5), socket.recv()).await;
    let Ok(Some(Ok(Message::Text(text)))) = request else {
        return;
    };
    let Ok(mut subscription) = serde_json::from_str::<Subscription>(&text) else {
        return;
    };
    if s.authorizer.is_ingress(&subscription.token) {
        return;
    }
    let Some(access) = s.authorizer.authenticate(&subscription.token) else {
        return;
    };
    if !access.allows(Scope::Read) {
        return;
    }
    let Ok(service) = s.directory.scoped(access) else {
        return;
    };
    let s = TenantService {
        service: Arc::new(service),
    };
    if socket
        .send(Message::Text("{\"ok\":true}".into()))
        .await
        .is_err()
    {
        return;
    }
    let Some(actors) = s.service.actors.as_ref() else {
        if let Err(error) = definition_stream(&s, &mut socket, &mut subscription).await {
            let _ = socket
                .send(Message::Text(
                    json!({"error":format!("{error:#}")}).to_string().into(),
                ))
                .await;
        }
        return;
    };
    let Ok(mut host) = actors.node.open_stream().await else {
        return;
    };
    // The socket loop owns this ws subscriber; close_stream removes its rows on every exit.
    let result = actor_stream(&s, &mut socket, &mut subscription, &mut host).await;
    let cleanup = actors.node.close_stream(&host.id).await;
    if let Err(error) = result {
        let _ = socket
            .send(Message::Text(
                json!({"error":format!("{error:#}")}).to_string().into(),
            ))
            .await;
    }
    if let Err(error) = cleanup {
        let _ = socket
            .send(Message::Text(
                json!({"error":format!("{error:#}")}).to_string().into(),
            ))
            .await;
    }
}

async fn definition_tick(
    s: &TenantService,
    socket: &mut WebSocket,
    subscription: &mut Subscription,
) -> Result<()> {
    let events = s
        .service
        .store
        .definition_events(subscription.after, 1000)?;
    s.service.store.flush()?;
    for event in events {
        subscription.after = event.seq;
        socket
            .send(Message::Text(serde_json::to_string(&event)?.into()))
            .await?;
    }
    Ok(())
}

async fn definition_stream(
    s: &TenantService,
    socket: &mut WebSocket,
    subscription: &mut Subscription,
) -> Result<()> {
    let mut interval = tokio::time::interval(Duration::from_millis(100));
    loop {
        tokio::select! {
            _ = interval.tick() => definition_tick(s, socket, subscription).await?,
            message = socket.recv() => match message {
                Some(Ok(Message::Text(_))) => anyhow::bail!("actor <none> seq -1: subscribe: actor node is not configured"),
                Some(Ok(Message::Ping(bytes))) => socket.send(Message::Pong(bytes)).await?,
                Some(Ok(Message::Close(_))) | None => return Ok(()),
                Some(Err(error)) => return Err(error.into()),
                _ => {}
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubscribeFrame {
    subscribe: SubscribeTarget,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubscribeTarget {
    actor: String,
    table: String,
    cap: String,
}

async fn actor_stream(
    s: &TenantService,
    socket: &mut WebSocket,
    subscription: &mut Subscription,
    host: &mut loom_actor::HostStream,
) -> Result<()> {
    let node = &s
        .service
        .actors
        .as_ref()
        .context("actor node is not configured")?
        .node;
    let mut interval = tokio::time::interval(Duration::from_millis(100));
    loop {
        tokio::select! {
            _ = interval.tick() => definition_tick(s, socket, subscription).await?,
            frame = host.receiver.recv() => match frame {
                Some(bytes) => socket.send(Message::Text(String::from_utf8(bytes)?.into())).await?,
                None => return Ok(()),
            },
            message = socket.recv() => match message {
                Some(Ok(Message::Text(text))) => {
                    let request: SubscribeFrame = serde_json::from_str(&text)?;
                    let target = request.subscribe;
                    // Decode only at the host boundary; JavaScript transports this JSON token unchanged.
                    let cap: loom_actor::Cap = serde_json::from_str(&target.cap)
                        .with_context(|| format!("actor {} seq -1: subscribe cap token", target.actor))?;
                    ensure!(target.actor == cap.target, "actor {} seq -1: subscribe cap target mismatch", target.actor);
                    node.subscribe_stream(&host.id, &cap, &target.table).await?;
                }
                Some(Ok(Message::Ping(bytes))) => socket.send(Message::Pong(bytes)).await?,
                Some(Ok(Message::Close(_))) | None => return Ok(()),
                Some(Err(error)) => return Err(error.into()),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod cap_wire_tests {
    use super::*;
    #[test]
    fn subscription_cap_stays_opaque_until_host_decode() {
        let cap = loom_actor::Cap {
            target: "view".into(),
            cap_id: u64::MAX,
            epoch: u64::MAX,
            rights: loom_actor::Rights::INSPECT,
            mac: [7; 32],
        };
        let token = serde_json::to_string(&cap).unwrap();
        let wire = json!({"subscribe":{"actor":"view","table":"tree","cap":token}});
        let frame: SubscribeFrame = serde_json::from_value(wire).unwrap();
        assert_eq!(
            serde_json::from_str::<loom_actor::Cap>(&frame.subscribe.cap).unwrap(),
            cap
        );
        assert!(
            serde_json::from_value::<SubscribeFrame>(
                json!({"subscribe":{"actor":"view","table":"tree","cap":cap}})
            )
            .is_err()
        );
    }
}
