use super::*;
use http_body_util::BodyExt;

#[tokio::test]
async fn active_build_route_shares_scoped_progress_and_requires_read_access() {
    let service = Arc::new(
        Service::new(
            Store::memory().unwrap(),
            PathBuf::from("."),
            vec![Lang::Rust],
        )
        .unwrap(),
    );
    let scoped = service.scoped(Access::owner()).unwrap();
    let progress = scoped.build_progress.start("example");
    progress.stage("compile");
    let authorizer = Authorizer::new(vec![
        TokenConfig {
            tenant: Default::default(),
            token: "read".into(),
            scopes: [Scope::Read].into_iter().collect(),
        },
        TokenConfig {
            tenant: Default::default(),
            token: "define".into(),
            scopes: [Scope::Define].into_iter().collect(),
        },
    ])
    .unwrap();
    let app = router(service, authorizer);
    let request = |token: &str| {
        Request::builder()
            .uri("/v1/builds/active")
            .header("authorization", format!("Bearer {token}"))
            .body(axum::body::Body::empty())
            .unwrap()
    };
    assert_eq!(
        app.clone()
            .oneshot(request("define"))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let response = app.clone().oneshot(request("read")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Response =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert!(body.ok);
    assert_eq!(body.result["active"]["name"], "example");
    assert_eq!(body.result["active"]["stage"], "compile");
    assert!(body.result["active"]["elapsed_ms"].is_u64());
    drop(progress);
    let response = app.oneshot(request("read")).await.unwrap();
    let body: Response =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert!(body.ok);
    assert_eq!(body.result, json!({"active":null}));
}

#[tokio::test]
async fn active_build_clears_after_preflight_failure() {
    let root = tempfile::tempdir().unwrap();
    let service = Service::new(
        Store::memory().unwrap(),
        root.path().into(),
        vec![Lang::Rust],
    )
    .unwrap()
    .with_driver_path(root.path().join("missing-driver"));
    let response = service
        .define(DefineRequest {
            name: "failure".into(),
            lang: Lang::Rust,
            source: "pub fn main() {}".into(),
            deps: BTreeMap::new(),
            allowed_effects: None,
        })
        .await;
    assert!(!response.ok);
    assert_eq!(service.build_progress.snapshot(), json!({"active":null}));
}

#[tokio::test]
async fn active_build_clears_when_task_is_cancelled() {
    let progress = crate::build_progress::BuildProgress::default();
    let task_progress = progress.clone();
    let (started, ready) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let _guard = task_progress.start("cancelled");
        started.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    ready.await.unwrap();
    assert_eq!(progress.snapshot()["active"]["stage"], "preflight");
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(progress.snapshot(), json!({"active":null}));
}
