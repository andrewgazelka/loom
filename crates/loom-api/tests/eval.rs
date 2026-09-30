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

    // A cell is addressed by its hash and lives in memory: nothing was named,
    // no revision made, and no definition row written.
    assert!(service.store.current_names().unwrap().is_empty());
    let hash = reply.result["hash"].as_str().unwrap();
    assert!(service.store.resolve(hash).unwrap().is_some());
    assert_eq!(durable_definitions(&service), 0);
}

fn durable_definitions(service: &Service) -> i64 {
    service
        .store
        .with_connection(|connection| {
            Ok(connection.query_row("SELECT count(*) FROM defs", [], |row| row.get(0))?)
        })
        .unwrap()
}

/// A trivial cell, so the dependency graph is warm and the next builds take the
/// replay path that production uses after the loomd prewarm. `tag` keeps the
/// cells of tests that share one build directory apart.
async fn warm(service: &Service, tag: u32) {
    let reply = eval(service, json!({"source":format!("{tag}")})).await;
    assert!(reply.ok, "{reply:?}");
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
    assert_eq!(reply.result["code"], "compile_failed");
    // The same failure, structured: the offending expression is on line 2.
    assert!(
        reply
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.line == 2 && diagnostic.message.contains("mismatched")),
        "{:?}",
        reply.diagnostics
    );
    // The failure left no definition behind, and the next cell still builds.
    assert!(service.store.current_names().unwrap().is_empty());
    let next = eval(&service, json!({"source":"pub fn fine() -> u32 { 7 }"})).await;
    assert!(next.ok, "{next:?}");
    assert_eq!(next.result["output"], 7);
}

#[tokio::test]
#[ignore = "requires Rust guest toolchain and LOOM_COMPILER_CACHE_OWNER"]
async fn eval_builds_one_cell_at_either_optimization_in_any_order() {
    let service = service();
    warm(&service, 101).await;
    let source = "pub fn sum(n: u64) -> u64 { (0..n).sum() }";
    let mut hashes = Vec::new();
    for optimize in [false, true, false, true] {
        let reply = eval(
            &service,
            json!({"source":source,"args":[1000],"optimize":optimize}),
        )
        .await;
        assert!(reply.ok, "optimize={optimize}: {reply:?}");
        assert_eq!(reply.result["output"], 499500);
        hashes.push(reply.result["hash"].clone());
    }
    // Optimization never changes what a definition is, and a cell in memory is
    // replaced by the next build of it, never refused as a conflicting publication.
    assert!(hashes.windows(2).all(|pair| pair[0] == pair[1]));
    assert_eq!(durable_definitions(&service), 0);
}

#[tokio::test]
#[ignore = "requires Rust guest toolchain and LOOM_COMPILER_CACHE_OWNER"]
async fn a_cell_and_a_definition_of_the_same_source_do_not_conflict() {
    let service = service();
    warm(&service, 102).await;
    let source = "pub fn twice(x: i64) -> i64 { x * 2 }";
    let add = |name: &'static str| {
        let service = service.clone();
        async move {
            service
                .command(CommandRequest {
                    session: None,
                    command: "add".into(),
                    args: json!({"name":name,"source":source,"lang":"rust"}),
                })
                .await
        }
    };
    // Cell first, then keep it; and the other way round.
    let cell = eval(&service, json!({"source":source,"args":[4]})).await;
    assert!(cell.ok, "{cell:?}");
    let kept = add("kept").await;
    assert!(kept.ok, "{kept:?}");
    assert_eq!(kept.result["hash"], cell.result["hash"]);
    let again = eval(&service, json!({"source":source,"args":[5]})).await;
    assert!(again.ok, "{again:?}");
    assert_eq!(again.result["output"], 10);
    assert_eq!(durable_definitions(&service), 1);
}

#[tokio::test]
#[ignore = "requires Rust guest toolchain and LOOM_COMPILER_CACHE_OWNER"]
async fn eval_arithmetic_wraps_like_a_release_build_at_every_optimization() {
    let service = service();
    warm(&service, 103).await;
    let source = "pub fn bump(x: u8) -> u8 { x + 250 }";
    for optimize in [false, true] {
        let reply = eval(
            &service,
            json!({"source":source,"args":[10],"optimize":optimize}),
        )
        .await;
        assert!(reply.ok, "optimize={optimize}: {reply:?}");
        assert_eq!(reply.result["output"], 4, "optimize={optimize}");
    }
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
    assert!(
        broken.diagnostics.iter().any(|diagnostic| diagnostic.line == 2),
        "the error is on the second line the caller wrote: {:?}",
        broken.diagnostics
    );
}
