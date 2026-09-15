use axum::{
    Router,
    body::{Body, Bytes},
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use loom_api::{Authorizer, Scope, Service, ServiceDirectory, TenantId, TokenConfig};
use loom_proto::{DAG_CBOR_CODEC, RAW_CODEC};
use loom_store::Store;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

struct Fixture {
    router: Router,
    blue: Store,
    red: Store,
    _root: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let blue = Store::memory().unwrap();
        let red = Store::memory().unwrap();
        let services = [
            Arc::new(
                Service::new(blue.clone(), root.path().join("blue"), Vec::new())
                    .unwrap()
                    .with_tenant(TenantId::new("blue").unwrap()),
            ),
            Arc::new(
                Service::new(red.clone(), root.path().join("red"), Vec::new())
                    .unwrap()
                    .with_tenant(TenantId::new("red").unwrap()),
            ),
        ];
        let authorizer = Authorizer::new(vec![
            TokenConfig {
                tenant: TenantId::new("blue").unwrap(),
                token: "blue-writer".into(),
                scopes: [Scope::Define, Scope::Read].into_iter().collect(),
            },
            TokenConfig {
                tenant: TenantId::new("red").unwrap(),
                token: "red-reader".into(),
                scopes: [Scope::Read].into_iter().collect(),
            },
            TokenConfig {
                tenant: TenantId::new("blue").unwrap(),
                token: "blue-reader".into(),
                scopes: [Scope::Read].into_iter().collect(),
            },
            TokenConfig {
                tenant: TenantId::new("absent").unwrap(),
                token: "missing".into(),
                scopes: [Scope::Define].into_iter().collect(),
            },
        ])
        .unwrap();
        Self {
            router: loom_api::router(ServiceDirectory::new(services).unwrap(), authorizer),
            blue,
            red,
            _root: root,
        }
    }
    async fn upload(&self, token: &str, kind: &str, body: Body) -> axum::response::Response {
        self.router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/cas")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", kind)
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn raw_and_json_uploads_return_canonical_tenant_references() {
    let fixture = Fixture::new();
    let chunks = vec![
        Ok::<_, std::io::Error>(Bytes::from_static(b"hello ")),
        Ok(Bytes::from_static(b"world")),
    ];
    let response = fixture
        .upload(
            "blue-writer",
            "application/octet-stream",
            Body::from_stream(futures_util::stream::iter(chunks)),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let value: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let cid = value["$ref"].as_str().unwrap();
    assert_eq!(loom_proto::parse_reference(cid).unwrap().codec, RAW_CODEC);
    assert_eq!(fixture.blue.get(cid).unwrap().unwrap(), b"hello world");
    assert!(fixture.red.get(cid).unwrap().is_none());
    let denied = fixture
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/cas/{cid}"))
                .header("authorization", "Bearer red-reader")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
    let document = json!({"file":{"$ref":cid},"mode":493});
    let response = fixture
        .upload(
            "blue-writer",
            "application/json",
            Body::from(serde_json::to_vec(&document).unwrap()),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let value: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let cid = value["$ref"].as_str().unwrap();
    assert_eq!(
        loom_proto::parse_reference(cid).unwrap().codec,
        DAG_CBOR_CODEC
    );
    assert_eq!(
        fixture.blue.get_value::<Value>(cid).unwrap().unwrap(),
        document
    );
}

#[tokio::test]
async fn upload_refuses_wrong_scope_tenant_type_and_json() {
    let fixture = Fixture::new();
    let before = fixture
        .blue
        .with_connection(|connection| {
            Ok(connection.query_row("SELECT count(*) FROM cas", [], |row| row.get::<_, i64>(0))?)
        })
        .unwrap();
    for token in ["blue-reader", "missing"] {
        assert_eq!(
            fixture
                .upload(token, "application/octet-stream", Body::from("denied"))
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        fixture
            .upload(
                "bad-token",
                "application/octet-stream",
                Body::from("denied")
            )
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        fixture
            .upload("blue-writer", "text/plain", Body::from("no"))
            .await
            .status(),
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    for body in ["{", r#"{"$ref":"not-a-cid"}"#] {
        assert_eq!(
            fixture
                .upload("blue-writer", "application/json", Body::from(body))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        fixture
            .blue
            .with_connection(|connection| Ok(connection.query_row(
                "SELECT count(*) FROM cas",
                [],
                |row| row.get::<_, i64>(0)
            )?))
            .unwrap(),
        before
    );
}

#[tokio::test]
async fn upload_caps_apply_to_declared_and_streamed_bodies() {
    let fixture = Fixture::new();
    let response = fixture
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/cas")
                .header("authorization", "Bearer blue-writer")
                .header("content-type", "application/octet-stream")
                .header("content-length", (512usize * 1024 * 1024 + 1).to_string())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let chunks = (0..17).map(|_| Ok::<_, std::io::Error>(Bytes::from(vec![b' '; 1024 * 1024])));
    let response = fixture
        .upload(
            "blue-writer",
            "application/json",
            Body::from_stream(futures_util::stream::iter(chunks)),
        )
        .await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}
