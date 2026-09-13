use super::*;

#[derive(Clone)]
struct ApiState {
    service: Arc<Service>,
    authorizer: Authorizer,
}
pub fn router(service: Arc<Service>, authorizer: Authorizer) -> Router {
    let authorizer = authorizer.with_ingress_bearer(
        service
            .actors
            .as_ref()
            .and_then(|actors| actors.node.ingress_bearer()),
    );
    let state = ApiState {
        service,
        authorizer,
    };
    Router::new()
        .route("/v1/command", post(command))
        .route("/v1/ingress", post(ingress))
        .route("/v1/cas/{hash}", get(cas))
        .route("/v1/events", get(events))
        .route("/v1/defs/{name}", get(definition))
        .route("/v1/graph/deps/{hash}", get(deps))
        .route("/v1/builds/{hash}", get(build))
        .route("/v1/builds/active", get(active_build))
        .route("/v1/stream", get(stream))
        .route("/health", get(|| async { Json(json!({"ok":true})) }))
        .layer(DefaultBodyLimit::max(16 * 1024 * 1024))
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
    let token = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let ingress_route = request.uri().path() == "/v1/ingress";
    if token.is_some_and(|token| authorizer.is_ingress(token)) {
        return if ingress_route {
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
    if request.method() == axum::http::Method::GET && !access.allows(Scope::Read) {
        return StatusCode::FORBIDDEN.into_response();
    }
    request.extensions_mut().insert(access);
    next.run(request).await
}
#[derive(Deserialize)]
struct IngressRequest {
    ops: Vec<loom_actor::DeliveryOp>,
}
async fn ingress(State(s): State<ApiState>, Json(request): Json<IngressRequest>) -> HttpResponse {
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
    State(s): State<ApiState>,
    axum::Extension(access): axum::Extension<Access>,
    request: Result<Json<CommandRequest>, axum::extract::rejection::JsonRejection>,
) -> HttpResponse {
    let service = s.service.scoped(access);
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
    State(s): State<ApiState>,
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
                axum::http::header::VARY,
                axum::http::HeaderValue::from_static("Accept"),
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

#[derive(Deserialize)]
struct EventQuery {
    #[serde(default)]
    after: i64,
    limit: Option<usize>,
}
async fn events(State(s): State<ApiState>, Query(q): Query<EventQuery>) -> Json<Response> {
    Json(
        s.service.response(
            s.service
                .store
                .definition_events(q.after, q.limit.unwrap_or(1000).min(1000))
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
async fn active_build(State(s): State<ApiState>) -> Json<Response> {
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
    if s.authorizer.is_ingress(&subscription.token)
        || s.authorizer
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
        tokio::select! {_ = interval.tick()=>{let Ok(events)=s.service.store.definition_events(subscription.after,1000) else{return};if s.service.store.flush().is_err(){return};for event in events{subscription.after=event.seq;let Ok(text)=serde_json::to_string(&event)else{return};if socket.send(Message::Text(text.into())).await.is_err(){return}}},message=socket.recv()=>match message{Some(Ok(Message::Ping(bytes)))=>{if socket.send(Message::Pong(bytes)).await.is_err(){return}},Some(Ok(Message::Close(_)))|None|Some(Err(_))=>return,_=>{}}}
    }
}
