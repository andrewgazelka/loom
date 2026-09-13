use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn ingress_credentials_are_route_exclusive_before_handler_execution() {
    let writes = Arc::new(AtomicUsize::new(0));
    let handler_writes = writes.clone();
    let app = protect(
        Router::new()
            .route("/v1/ingress", post(move || {
                let writes = handler_writes.clone();
                async move {
                    writes.fetch_add(1, Ordering::SeqCst);
                    StatusCode::OK
                }
            }))
            .route("/v1/command", post(|| async { StatusCode::OK })),
        Authorizer::single("user-token".into()).unwrap().with_ingress_bearer(Some("cluster-token".into())),
    );
    struct Case {
        path: &'static str,
        token: &'static str,
        status: StatusCode,
    }
    for case in [
        Case { path: "/v1/ingress", token: "wrong-cluster", status: StatusCode::UNAUTHORIZED },
        Case { path: "/v1/ingress", token: "user-token", status: StatusCode::FORBIDDEN },
        Case { path: "/v1/command", token: "cluster-token", status: StatusCode::FORBIDDEN },
    ] {
        let response = app.clone().oneshot(Request::builder().method("POST").uri(case.path)
            .header("authorization", format!("Bearer {}", case.token))
            .body(axum::body::Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), case.status);
        assert_eq!(writes.load(Ordering::SeqCst), 0, "rejected ingress reached the write handler");
    }
    let response = app.oneshot(Request::builder().method("POST").uri("/v1/ingress")
        .header("authorization", "Bearer cluster-token")
        .body(axum::body::Body::empty()).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(writes.load(Ordering::SeqCst), 1, "valid ingress control never reached the handler");
}
