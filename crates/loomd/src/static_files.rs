use axum::{Router, extract::Request, http::header, middleware::Next, response::Response};
use std::path::PathBuf;

pub fn router(directory: PathBuf) -> Router {
    Router::new()
        .fallback_service(tower_http::services::ServeDir::new(directory))
        .layer(axum::middleware::from_fn(cache_policy))
}

async fn cache_policy(mut request: Request, next: Next) -> Response {
    let immutable = request.uri().path().starts_with("/_app/immutable/");
    // Nix normalizes mtimes across releases. A date validator cannot establish
    // document identity, including for clients holding a pre-policy response.
    if !immutable {
        request.headers_mut().remove(header::IF_MODIFIED_SINCE);
        request.headers_mut().remove(header::IF_UNMODIFIED_SINCE);
        request.headers_mut().remove(header::IF_NONE_MATCH);
    }
    let mut response = next.run(request).await;
    let policy = if immutable && response.status().is_success() {
        "public, max-age=31536000, immutable"
    } else {
        response.headers_mut().remove(header::LAST_MODIFIED);
        "no-store"
    };
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static(policy),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn deployment_documents_ignore_newer_browser_mtime() {
        let directory = tempfile::tempdir().unwrap();
        let index = directory.path().join("index.html");
        std::fs::write(&index, "<script src='/_app/immutable/new.js'></script>").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&index)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
            .unwrap();
        let response = router(directory.path().into())
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::IF_MODIFIED_SINCE, "Wed, 09 Sep 2026 12:00:00 GMT")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert!(!response.headers().contains_key(header::LAST_MODIFIED));
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(std::str::from_utf8(&body).unwrap().contains("new.js"));
    }

    #[tokio::test]
    async fn hashed_assets_are_immutable_but_missing_files_are_not_cached() {
        let directory = tempfile::tempdir().unwrap();
        let assets = directory.path().join("_app/immutable");
        std::fs::create_dir_all(&assets).unwrap();
        std::fs::write(assets.join("new.js"), "export {};").unwrap();
        let app = router(directory.path().into());
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/_app/immutable/new.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        let missing = app
            .oneshot(
                Request::builder()
                    .uri("/_app/immutable/old.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert_eq!(missing.headers()[header::CACHE_CONTROL], "no-store");
    }
}
