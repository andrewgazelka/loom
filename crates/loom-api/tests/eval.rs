//! `eval`: compile a Rust cell and run one of its entries in a single call,
//! without a name, a revision or a staged copy of the store.
use loom_api::Service;
use loom_proto::{CommandRequest, Lang, Response, Value};
use loom_store::Store;
use serde_json::json;
use std::path::PathBuf;

fn service() -> Service {
    Service::new(
        Store::memory().unwrap(),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        vec![Lang::Rust],
    )
    .unwrap()
}

async fn eval(service: &Service, args: Value) -> Response {
    service
        .command(CommandRequest {
            session: None,
            command: "eval".into(),
            args,
        })
        .await
}

#[tokio::test]
#[ignore = "requires Rust guest toolchain and LOOM_COMPILER_CACHE_OWNER"]
async fn eval_runs_a_cell_and_reports_where_the_time_went() {
    let service = service();
    let reply = eval(
        &service,
        json!({"source":"pub fn double(x: i64) -> i64 { x * 2 }","args":[21]}),
    )
    .await;
    assert!(reply.ok, "{reply:?}");
    assert_eq!(reply.result["output"], 42);
    assert_eq!(reply.result["entry"], "double");
    let timings = &reply.result["timings_ms"];
    assert!(timings["compile"].is_u64() && timings["run"].is_u64() && timings["total"].is_u64());

    // A cell is addressed by its hash: nothing was named and no revision made.
    assert!(service.store.current_names().unwrap().is_empty());
}

#[tokio::test]
#[ignore = "requires Rust guest toolchain and LOOM_COMPILER_CACHE_OWNER"]
async fn eval_of_the_same_cell_again_reuses_the_build() {
    let service = service();
    let source = "pub fn triple(x: i64) -> i64 { x * 3 }";
    let first = eval(&service, json!({"source":source,"args":[2]})).await;
    let second = eval(&service, json!({"source":source,"args":[5]})).await;
    assert!(first.ok && second.ok, "{first:?} {second:?}");
    assert_eq!(first.result["hash"], second.result["hash"]);
    assert_eq!(second.result["output"], 15);
    assert_eq!(second.result["build"]["rustc_invocations"], 0);
}

#[tokio::test]
#[ignore = "requires Rust guest toolchain and LOOM_COMPILER_CACHE_OWNER"]
async fn eval_names_the_entry_when_the_cell_exports_several() {
    let service = service();
    let source = "pub fn one() -> i64 { 1 }\npub fn two() -> i64 { 2 }";
    let ambiguous = eval(&service, json!({"source":source})).await;
    assert!(!ambiguous.ok);
    let message = ambiguous.result["error"].as_str().unwrap().to_owned();
    assert!(message.contains("one") && message.contains("two"), "{message}");
    let chosen = eval(&service, json!({"source":source,"entry":"two"})).await;
    assert!(chosen.ok, "{chosen:?}");
    assert_eq!(chosen.result["output"], 2);
    let unknown = eval(&service, json!({"source":source,"entry":"three"})).await;
    assert!(!unknown.ok);
}

#[tokio::test]
#[ignore = "requires Rust guest toolchain and LOOM_COMPILER_CACHE_OWNER"]
async fn eval_reports_a_compile_error_with_its_location() {
    let service = service();
    let reply = eval(
        &service,
        json!({"source":"pub fn broken() -> u32 {\n    \"text\"\n}"}),
    )
    .await;
    assert!(!reply.ok);
    let message = reply.result["error"].as_str().unwrap();
    assert!(message.contains("compile failed"), "{message}");
    assert!(message.contains("mismatched types"), "{message}");
    // The failure left no definition behind, and the next cell still builds.
    assert!(service.store.current_names().unwrap().is_empty());
    let next = eval(&service, json!({"source":"pub fn fine() -> u32 { 7 }"})).await;
    assert!(next.ok, "{next:?}");
    assert_eq!(next.result["output"], 7);
}

#[tokio::test]
#[ignore = "requires Rust guest toolchain and LOOM_COMPILER_CACHE_OWNER"]
async fn eval_optimize_builds_the_same_cell_at_standard_optimization() {
    let service = service();
    let source = "pub fn sum(n: u64) -> u64 { (0..n).sum() }";
    let quick = eval(&service, json!({"source":source,"args":[1000]})).await;
    let optimized = eval(&service, json!({"source":source,"args":[1000],"optimize":true})).await;
    assert!(quick.ok && optimized.ok, "{quick:?} {optimized:?}");
    assert_eq!(quick.result["output"], 499500);
    assert_eq!(optimized.result["output"], 499500);
    // Optimization never changes what a definition is.
    assert_eq!(quick.result["hash"], optimized.result["hash"]);
}

#[tokio::test]
#[ignore = "requires Rust guest toolchain and LOOM_COMPILER_CACHE_OWNER"]
async fn eval_runs_a_bare_block_as_a_cell() {
    let service = service();
    let reply = eval(
        &service,
        json!({"source":"let values = vec![1, 2, 3, 4];\nvalues.iter().sum::<i32>() * 10"}),
    )
    .await;
    assert!(reply.ok, "{reply:?}");
    assert_eq!(reply.result["output"], 100);
    assert_eq!(reply.result["entry"], "eval");

    // A compile error in a bare cell keeps the line the caller wrote.
    let broken = eval(&service, json!({"source":"let x = 1;\nlet y: u8 = \"no\";\ny"})).await;
    assert!(!broken.ok);
    let message = broken.result["error"].as_str().unwrap();
    assert!(message.contains("mismatched types"), "{message}");
}
