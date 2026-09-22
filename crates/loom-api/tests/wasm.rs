//! `GET /v1/wasm/{component_hash}` on a real guest build: the text view, its
//! function table, and the DWARF join back to `src/lib.rs`.
#[path = "support/guest.rs"]
mod guest;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use loom_api::{Authorizer, Scope, Service, TokenConfig};
use loom_proto::{CommandRequest, Lang, Value};
use loom_store::Store;
use serde_json::json;
use std::{path::PathBuf, sync::Arc};
use tower::ServiceExt;

const TEST: &str = "wasm_text_joins_greet_instructions_to_its_source_lines";

#[test]
fn wasm_text_joins_greet_instructions_to_its_source_lines() {
    guest::run(TEST, workflow);
}

async fn fetch(router: &axum::Router, path: &str, token: Option<&str>) -> (StatusCode, Value) {
    let mut request = Request::builder().uri(path);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

async fn workflow() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let store = Store::memory().unwrap();
    let service = Service::new(store.clone(), root.clone(), vec![Lang::Rust]).unwrap();
    let source = std::fs::read_to_string(root.join("examples/unison/greet.rs")).unwrap();
    let added = service
        .command(CommandRequest {
            session: None,
            command: "add".into(),
            args: json!({"name":"greet","source":source,"lang":"rust"}),
        })
        .await;
    assert!(added.ok, "{added:?}");
    let hash = added.result["hash"].as_str().unwrap();
    let component = store
        .definition(hash)
        .unwrap()
        .unwrap()
        .component_hash
        .expect("a built definition pins its module");
    let authorizer = Authorizer::new(vec![
        TokenConfig {
            tenant: Default::default(),
            token: "reader".into(),
            scopes: [Scope::Read].into_iter().collect(),
        },
        TokenConfig {
            tenant: Default::default(),
            token: "runner".into(),
            scopes: [Scope::Execute].into_iter().collect(),
        },
    ])
    .unwrap();
    let router = loom_api::router(Arc::new(service), authorizer);

    let (status, view) = fetch(&router, &format!("/v1/wasm/{component}"), Some("reader")).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["debug"], true, "the build carries DWARF");
    let wat = view["wat"].as_str().unwrap();
    let wat_lines: Vec<&str> = wat.lines().collect();
    assert!(wat.starts_with("(module"), "{}", &wat[..wat.len().min(80)]);

    let functions = view["functions"].as_array().unwrap();
    assert!(!functions.is_empty());
    let entry = functions
        .iter()
        .find(|function| function["name"] == "loom_call_greet")
        .unwrap_or_else(|| panic!("no loom_call_greet in {functions:?}"));
    assert_eq!(entry["exported"], true);
    let start = entry["start_line"].as_u64().unwrap() as usize;
    let end = entry["end_line"].as_u64().unwrap() as usize;
    assert!(
        start >= 1 && start <= end && end <= wat_lines.len(),
        "{entry}"
    );
    assert!(
        wat_lines[start - 1].contains("(func $loom_call_greet"),
        "{}",
        wat_lines[start - 1]
    );
    assert!(
        wat_lines[end - 1].trim_end().ends_with(')'),
        "{}",
        wat_lines[end - 1]
    );
    for function in functions {
        assert!(function["index"].is_u64() && function["exported"].is_boolean());
        assert!(function["start_line"].as_u64().unwrap() <= function["end_line"].as_u64().unwrap());
    }
    // `pub fn greet` and its body in examples/unison/greet.rs. The optimizer
    // may inline `greet` into `loom_call_greet`; its source lines then appear
    // as inlined rows and still join to instructions.
    let greet_start = source
        .lines()
        .position(|line| line.starts_with("pub fn greet"))
        .unwrap() as u64
        + 1;
    let greet_end = source
        .lines()
        .enumerate()
        .skip(greet_start as usize)
        .find(|(_, line)| *line == "}")
        .unwrap()
        .0 as u64
        + 1;
    let lines = view["lines"].as_array().unwrap();
    assert!(!lines.is_empty());
    let mut inside_greet = 0;
    for entry in lines {
        let wat_line = entry["wat_line"].as_u64().unwrap();
        assert!(
            wat_line >= 1 && wat_line as usize <= wat_lines.len(),
            "{entry}"
        );
        assert!(
            entry["file"].is_string() && entry["line"].is_u64(),
            "{entry}"
        );
        let file = entry["file"].as_str().unwrap();
        let line = entry["line"].as_u64().unwrap();
        if file.ends_with("src/lib.rs") && (greet_start..=greet_end).contains(&line) {
            inside_greet += 1;
        }
    }
    assert!(
        inside_greet > 0,
        "no instruction maps into greet ({greet_start}..={greet_end}); first lines {:?}",
        &lines[..lines.len().min(10)]
    );

    // Convention check: every DWARF sequence starts at a function body, which
    // means the join's base (the code section's contents) is the one rustc and
    // rust-lld used. `functions` and `lines` agree on that base when every
    // mapped wat line lies inside some function.
    let function_ranges: Vec<(u64, u64)> = functions
        .iter()
        .map(|function| {
            (
                function["start_line"].as_u64().unwrap(),
                function["end_line"].as_u64().unwrap(),
            )
        })
        .collect();
    for entry in lines {
        let wat_line = entry["wat_line"].as_u64().unwrap();
        assert!(
            function_ranges
                .iter()
                .any(|(start, end)| (*start..=*end).contains(&wat_line)),
            "mapped line {wat_line} lies in no function"
        );
    }

    // Missing artifact: 404 in the command error envelope.
    let (status, body) = fetch(
        &router,
        &format!("/v1/wasm/{}", "0".repeat(64)),
        Some("reader"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["ok"], false, "{body}");
    assert!(
        body["result"]["error"]
            .as_str()
            .unwrap()
            .contains("not found")
    );
    // Read scope is required.
    let (status, _) = fetch(&router, &format!("/v1/wasm/{component}"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = fetch(&router, &format!("/v1/wasm/{component}"), Some("runner")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // A CAS object that is not a module is refused, not rendered.
    let text = store.put("blob", b"not a module").unwrap();
    let (status, body) = fetch(&router, &format!("/v1/wasm/{text}"), Some("reader")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["ok"], false);
}
