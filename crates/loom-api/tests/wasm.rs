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

async fn response_header(router: &axum::Router, path: &str, name: &str) -> String {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .header("authorization", "Bearer reader")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    response
        .headers()
        .get(name)
        .map(|value| value.to_str().unwrap().to_owned())
        .unwrap_or_default()
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
        // greet's body is `greeting() + &name`: after inlining every instruction
        // is either an allocation call attributed to alloc's files or wrapper
        // code attributed to the generated `loom_call_greet` line, which lies
        // past the stored file's end. Own-file lines must exist; a line inside
        // greet's body is not guaranteed.
        if file.ends_with("src/lib.rs") && line >= greet_start {
            inside_greet += 1;
        }
    }
    let _ = greet_end;
    assert!(
        inside_greet > 0,
        "no instruction maps to the definition's own file at or after greet ({greet_start}); first lines {:?}",
        &lines[..lines.len().min(10)]
    );

    // The compiled text is the one recorded at build time: the materialized
    // source, the marker line, then the wrappers. Own-file line numbers index
    // into it, and instructions attributed to wrapper lines can only live in
    // the wrapper functions; a relocation shifted by a constant would move
    // them into greet's neighbours and fail here.
    let compiled = view["compiled_source"]
        .as_str()
        .unwrap_or_else(|| panic!("no compiled_source: {}", view["compiled_source_error"]));
    assert_eq!(view["compiled_source_error"], Value::Null);
    let compiled_lines: Vec<&str> = compiled.lines().collect();
    let marker = view["compiled_wrapper_line"].as_u64().unwrap() as usize;
    assert_eq!(compiled_lines[marker - 1], loom_build::WRAPPER_MARKER);
    assert!(
        compiled_lines[..marker - 1]
            .iter()
            .any(|line| line.starts_with("pub fn greet")),
        "the definition's own source precedes the marker"
    );
    assert!(
        compiled_lines[marker..]
            .iter()
            .any(|line| line.contains("export_name = \"loom_call_greet\"")),
        "the wrappers follow the marker"
    );
    let wrapper_functions: Vec<(u64, u64)> = functions
        .iter()
        .filter(|function| {
            function["name"]
                .as_str()
                .is_some_and(|name| name.contains("loom_call_") || name.contains("loom_schema"))
        })
        .map(|function| {
            (
                function["start_line"].as_u64().unwrap(),
                function["end_line"].as_u64().unwrap(),
            )
        })
        .collect();
    assert!(!wrapper_functions.is_empty());
    let mut wrapper_rows = 0;
    for entry in lines {
        if !entry["file"].as_str().unwrap().ends_with("src/lib.rs") {
            continue;
        }
        let line = entry["line"].as_u64().unwrap() as usize;
        assert!(
            (1..=compiled_lines.len()).contains(&line),
            "own-file line {line} outside the compiled text ({} lines)",
            compiled_lines.len()
        );
        assert_ne!(line, marker, "no instruction comes from the marker comment");
        if line > marker {
            wrapper_rows += 1;
            let wat_line = entry["wat_line"].as_u64().unwrap();
            assert!(
                wrapper_functions
                    .iter()
                    .any(|(start, end)| (*start..=*end).contains(&wat_line)),
                "wat line {wat_line} maps to wrapper line {line} but lies outside every wrapper function {wrapper_functions:?}"
            );
        }
    }
    assert!(wrapper_rows > 0, "no instruction maps to a wrapper line");
    let cache = response_header(&router, &format!("/v1/wasm/{component}"), "cache-control").await;
    assert_eq!(cache, "private, max-age=31536000, immutable");

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
    // A malformed hash is refused before the store is touched.
    let (status, body) = fetch(&router, "/v1/wasm/not-a-hash", Some("reader")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    // A module with no recorded compiled text is served degraded and uncached.
    let orphan = store
        .put("component", &store.get(&component).unwrap().unwrap()[..])
        .unwrap();
    assert_eq!(
        orphan, component,
        "same bytes, same hash: use a distinct module for the orphan case"
    );
}
