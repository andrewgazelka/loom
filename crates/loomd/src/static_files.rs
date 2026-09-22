use axum::{
    Router,
    extract::Request,
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::path::PathBuf;
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};

pub fn router(directory: PathBuf) -> Router {
    let index = directory.join("index.html");
    let documents = Router::new().fallback(move |request: Request| {
        let index = index.clone();
        async move {
            if !is_page_route(request.uri().path()) {
                return StatusCode::NOT_FOUND.into_response();
            }
            ServeFile::new(index)
                .oneshot(request)
                .await
                .unwrap_or_else(|never| match never {})
                .into_response()
        }
    });
    Router::new()
        .fallback_service(ServeDir::new(directory).fallback(documents))
        .layer(axum::middleware::from_fn(cache_policy))
}

/// The SvelteKit build is a single-page app: every page route renders
/// client-side from `index.html`, so a path that names no file on disk is a
/// page route when its last segment carries no extension. `/_app/` holds only
/// built assets, and a missing asset must stay a 404: serving HTML where a
/// stale page expects a script would execute the document as code.
fn is_page_route(path: &str) -> bool {
    !path.starts_with("/_app/") && !path.rsplit('/').next().unwrap_or(path).contains('.')
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

    #[tokio::test]
    async fn page_routes_serve_index_html_and_missing_assets_stay_404() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join("_app/immutable")).unwrap();
        std::fs::write(
            directory.path().join("index.html"),
            "<!doctype html><div id='loom-spa-shell'></div>",
        )
        .unwrap();
        let app = router(directory.path().into());
        for route in ["/board", "/view", "/board/actors/a0-1"] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(route).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{route}");
            assert_eq!(
                response.headers()[header::CACHE_CONTROL],
                "no-store",
                "{route}"
            );
            assert!(!response.headers().contains_key(header::LAST_MODIFIED));
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert!(
                std::str::from_utf8(&body)
                    .unwrap()
                    .contains("loom-spa-shell"),
                "{route} did not serve index.html"
            );
        }
        for path in ["/missing.js", "/_app/immutable/x.js", "/_app/version"] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        }
    }

    #[test]
    fn page_route_classification() {
        assert!(is_page_route("/board"));
        assert!(is_page_route("/view"));
        assert!(is_page_route("/nested/route"));
        assert!(!is_page_route("/missing.js"));
        assert!(!is_page_route("/nested/file.css"));
        assert!(!is_page_route("/_app/immutable/x.js"));
        assert!(!is_page_route("/_app/version"));
    }
}
