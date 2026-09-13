use super::*;
#[tokio::test]
async fn inline_dag_reference_resolves_as_json_and_serves_canonical_bytes() {
    use http_body_util::BodyExt;
    let service = Arc::new(
        Service::new(
            Store::memory().unwrap(),
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
            vec![Lang::Rust],
        )
        .unwrap(),
    );
    let value = json!({"large":"x".repeat(9000)});
    let response = service.inline(service.response(Ok(value.clone())));
    assert!(response.ok, "{response:?}");
    assert_eq!(response.result.as_object().unwrap().len(), 1);
    let cid = response.result["$ref"].as_str().unwrap();
    assert_eq!(
        loom_proto::parse_reference(cid).unwrap().codec,
        loom_proto::DAG_CBOR_CODEC
    );
    assert_eq!(
        service.store.get_value::<Value>(cid).unwrap(),
        Some(value.clone())
    );
    let app = router(
        service.clone(),
        Authorizer::single("test-secret".into()).unwrap(),
    );
    let resolved = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/command")
                .header("authorization", "Bearer test-secret")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({"command":"resolve","args":{"hash":cid}}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let resolved: Response =
        serde_json::from_slice(&resolved.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(resolved.result, value);
    let raw = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/cas/{cid}"))
                .header("authorization", "Bearer test-secret")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        raw.headers()["content-type"],
        "application/vnd.ipld.dag-cbor"
    );
    assert_eq!(raw.headers()["cache-control"], "public, max-age=31536000, immutable");
    assert_eq!(
        loom_proto::decode::<Value>(&raw.into_body().collect().await.unwrap().to_bytes()).unwrap(),
        value
    );
    let json = app
        .oneshot(
            Request::builder()
                .uri(format!("/v1/cas/{cid}"))
                .header("authorization", "Bearer test-secret")
                .header("accept", "application/json")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(json.headers()["content-type"], "application/json");
    assert_eq!(json.headers()["cache-control"], "public, max-age=31536000, immutable");
    assert_eq!(
        serde_json::from_slice::<Value>(&json.into_body().collect().await.unwrap().to_bytes())
            .unwrap(),
        value
    );
}
#[tokio::test]
async fn invalid_refs_are_structured_rejections_before_commands_run() {
    use http_body_util::BodyExt;
    let service = Arc::new(
        Service::new(
            Store::memory().unwrap(),
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
            vec![Lang::Rust],
        )
        .unwrap(),
    );
    let response = service
        .command(CommandRequest {
            session: None,
            command: "stats".into(),
            args: json!({"nested":{"$ref":"not-a-cid"}}),
        })
        .await;
    assert!(!response.ok);
    assert_eq!(response.result["code"], "operation_failed");
    assert_eq!(service.store.latest_seq().unwrap(), 0);
    let app = router(service, Authorizer::single("test-secret".into()).unwrap());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/cas/not-a-cid")
                .header("authorization", "Bearer test-secret")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response: Response =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert!(!response.ok);
}
