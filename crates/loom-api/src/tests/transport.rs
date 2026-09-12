use super::*;
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
