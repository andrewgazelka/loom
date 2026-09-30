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

#[tokio::test]
#[ignore = "requires Rust guest toolchain and LOOM_COMPILER_CACHE_OWNER"]
async fn eval_accepts_unit_variants_and_derived_default_enums() {
    // Effect analysis once died with an ICE (`Instance::try_resolve` on `Ctor(Variant, Const)`) for
    // `Self::A` and `#[derive(Default)] #[default]`: a constructor builds a value and runs no body.
    let service = service();
    let reply = eval(
        &service,
        json!({"source":"#[derive(Clone, Copy, Default)]\nenum K { A, #[default] B }\nimpl K { fn pick(x: bool) -> Self { if x { Self::A } else { Self::B } } }\npub fn f() -> u32 { K::pick(true) as u32 + 10 * K::default() as u32 }","entry":"f"}),
    )
    .await;
    assert!(reply.ok, "{reply:?}");
    assert_eq!(reply.result["output"], 10);
}

const CRATES_IO: &str = "registry+https://github.com/rust-lang/crates.io-index";

fn crate_manifest(dependency: &str) -> String {
    format!(
        "[package]\nname = \"cell\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[dependencies]\n{dependency}\n"
    )
}

fn registry_lock(name: &str, version: &str, checksum: &str) -> String {
    format!(
        "version = 4\n\n[[package]]\nname = \"{name}\"\nversion = \"{version}\"\nsource = \"{CRATES_IO}\"\nchecksum = \"{checksum}\"\n"
    )
}

/// A crates.io crate through a supplied `Cargo.lock`, on the host toolchain:
/// no build script, no proc macro, no Linux sandbox. The archive is in the
/// host's cargo registry cache (`cargo fetch` it, or run with
/// `LOOM_ALLOW_CARGO_FETCH=1`).
#[tokio::test]
#[ignore = "requires Rust guest toolchain, LOOM_COMPILER_CACHE_OWNER and robust 1.2.0 in ~/.cargo/registry"]
async fn eval_uses_a_locked_crates_io_crate() {
    let service = service();
    let manifest = crate_manifest("robust = \"=1.2.0\"");
    let lock = registry_lock(
        "robust",
        "1.2.0",
        "4e27ee8bb91ca0adcf0ecb116293afa12d393f9c2b9b9cd54d33e8078fe19839",
    );
    let cell = "let a = robust::Coord { x: 0.0, y: 0.0 };\nlet b = robust::Coord { x: 1.0, y: 0.0 };\nlet c = robust::Coord { x: 0.0, y: 1.0 };\nrobust::orient2d(a, b, c) > 0.0";
    let reply = eval(
        &service,
        json!({"source":cell,"manifest":manifest,"lock":lock}),
    )
    .await;
    assert!(reply.ok, "{reply:?}");
    assert_eq!(reply.result["output"], true);
    assert_eq!(reply.result["entry"], "eval");

    // The same cell again reuses the build; nothing was named or kept.
    let again = eval(
        &service,
        json!({"source":cell,"manifest":manifest,"lock":lock}),
    )
    .await;
    assert!(again.ok, "{again:?}");
    assert_eq!(again.result["hash"], reply.result["hash"]);
    assert!(service.store.current_names().unwrap().is_empty());

    // A lock naming a different checksum is refused before anything compiles:
    // Cargo rejects the archive, or the pin check does.
    let forged = registry_lock("robust", "1.2.0", &"0".repeat(64));
    let refused = eval(
        &service,
        json!({"source":cell,"manifest":manifest,"lock":forged}),
    )
    .await;
    assert!(!refused.ok, "{refused:?}");

    // A git source in the lock never reaches the host toolchain.
    let git = "version = 4\n\n[[package]]\nname = \"robust\"\nversion = \"1.2.0\"\nsource = \"git+https://example.com/robust#abc\"\n";
    let refused = eval(
        &service,
        json!({"source":cell,"manifest":manifest,"lock":git}),
    )
    .await;
    assert!(!refused.ok, "{refused:?}");
    let message = refused.result["error"].as_str().unwrap();
    assert!(message.contains("only crates.io"), "{message}");
}

#[tokio::test]
#[ignore = "requires Rust guest toolchain, LOOM_COMPILER_CACHE_OWNER and smallvec 1.16.2 in ~/.cargo/registry"]
async fn eval_uses_smallvec_through_its_cfg_attr_feature_gates() {
    let service = service();
    // smallvec's crate root carries `#![cfg_attr(feature = "specialization",
    // feature(specialization))]`; the gate is dormant, so the crate is admitted.
    let manifest = crate_manifest("smallvec = \"=1.16.2\"");
    let lock = registry_lock(
        "smallvec",
        "1.16.2",
        "f9395f0f0eee849a9b707b2f06bb92a6a422090e2123bb2ef8e87a0e61892a8e",
    );
    let cell = "let mut values: smallvec::SmallVec<[i64; 4]> = smallvec::SmallVec::new();\nfor value in 0..6 {\n    values.push(value);\n}\n(values.len(), values.spilled(), values.iter().sum::<i64>())";
    let reply = eval(
        &service,
        json!({"source":cell,"manifest":manifest,"lock":lock}),
    )
    .await;
    assert!(reply.ok, "{reply:?}");
    assert_eq!(reply.result["output"], json!([6, true, 15]));

    // The macro stays refused in guest source; the constructor functions do not.
    let refused = eval(
        &service,
        json!({"source":"let values: smallvec::SmallVec<[i64; 2]> = smallvec::smallvec![1, 2];\nvalues.len()","manifest":manifest,"lock":lock}),
    )
    .await;
    assert!(!refused.ok, "{refused:?}");
}

/// Without a lock a non-SDK dependency still needs the Linux sandbox worker;
/// on a host without it the error says to supply a `Cargo.lock`.
#[tokio::test]
#[ignore = "requires Rust guest toolchain and LOOM_COMPILER_CACHE_OWNER; asserts the macOS message"]
async fn eval_of_a_crate_without_a_lock_says_to_supply_one() {
    if cfg!(target_os = "linux") {
        return;
    }
    let service = service();
    let reply = eval(
        &service,
        json!({"source":"1","manifest":crate_manifest("robust = \"=1.2.0\"")}),
    )
    .await;
    assert!(!reply.ok, "{reply:?}");
    let message = reply.result["error"].as_str().unwrap();
    assert!(message.contains("Cargo.lock"), "{message}");
}
