use super::*;
#[test]
fn uncertain_send_is_not_reported_as_a_dead_letter() {
    use crate::message_failure::{ActorMessageFailure, MessageOutcome};
    let service = Service::new(
        Store::memory().unwrap(),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        vec![Lang::Rust],
    )
    .unwrap();
    let pending = service.response(Err(ActorMessageFailure {
        id: "actor".into(),
        seq: 7,
        cause: "delivery still pending".into(),
        outcome: MessageOutcome::Pending,
    }
    .into()));
    assert!(!pending.ok);
    assert_eq!(pending.result["code"], "actor_message_pending");
    assert_eq!(pending.result["id"], "actor");
    assert_eq!(pending.result["seq"], 7);
    assert_eq!(pending.result["cause"], "delivery still pending");
    let failed = service.response(Err(ActorMessageFailure {
        id: "actor".into(),
        seq: 7,
        cause: "trap".into(),
        outcome: MessageOutcome::Failed,
    }
    .into()));
    assert_eq!(failed.result["code"], "actor_message_failed");
}
#[tokio::test]
async fn read_scope_cannot_create_view_directly_or_through_command() {
    let authorizer = Authorizer::new(vec![crate::auth::TokenConfig {
        tenant: Default::default(),
        token: "reader".into(),
        scopes: [Scope::Read].into_iter().collect(),
    }])
    .unwrap();
    let service = Service::new(
        Store::memory().unwrap(),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        vec![Lang::Rust],
    )
    .unwrap()
    .scoped(authorizer.authenticate("reader").unwrap())
    .unwrap();
    let args = json!({"actor":"a","table":"t","template":"h","order_by":[]});
    for request in [
        CommandRequest {
            session: None,
            command: "view".into(),
            args: args.clone(),
        },
        CommandRequest {
            session: None,
            command: "command".into(),
            args: json!({"command":"view","args":args}),
        },
    ] {
        let response = service.command(request).await;
        assert!(!response.ok);
        assert_eq!(response.result["code"], "forbidden");
        assert!(
            response.result["error"]
                .as_str()
                .unwrap()
                .contains("Execute")
        );
    }
}
#[tokio::test]
async fn auth_gates_every_operation_and_health_is_public() {
    for path in [
        "/v1/command",
        "/v1/events",
        "/v1/cas/hash",
        "/v1/builds/active",
    ] {
        let response = app()
            .oneshot(
                Request::builder()
                    .method(if path != "/v1/command" { "GET" } else { "POST" })
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
    for command in ["undefined", "defs"] {
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
        assert_eq!(body.ok, command == "defs");
    }
}
