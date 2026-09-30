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
    assert!(
        message.contains("one") && message.contains("two"),
        "{message}"
    );
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
    let broken = eval(
        &service,
        json!({"source":"let x = 1;\nlet y: u8 = \"no\";\ny"}),
    )
    .await;
    assert!(!broken.ok);
    let message = broken.result["error"].as_str().unwrap();
    assert!(message.contains("mismatched types"), "{message}");
    assert!(
        broken
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.line == 2),
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

#[tokio::test]
#[ignore = "requires Rust guest toolchain, LOOM_COMPILER_CACHE_OWNER and glam 0.33.10 in ~/.cargo/registry"]
async fn eval_uses_glam_whose_tests_directory_and_dormant_include_are_not_compiled() {
    // glam ships `tests/support.rs` with a `#[path]` attribute and `include!("features/..")` under an
    // inactive feature. Only a dependency's compiled library sources are scanned, and a relative
    // in-package `include!` is admitted, so the crate builds.
    let service = service();
    let manifest = crate_manifest("glam = \"=0.33.10\"");
    let lock = registry_lock(
        "glam",
        "0.33.10",
        "928452f9c953e142b2f0973e4bd5f34445fcaa8069556ea97a3c9d34d15a4cf8",
    );
    let cell = "use glam::DVec3;\nlet a = DVec3::new(1.0, 2.0, 3.0);\n(a.cross(DVec3::Y).length() * 1000.0) as u32";
    let reply = eval(
        &service,
        json!({"source":cell,"manifest":manifest,"lock":lock}),
    )
    .await;
    assert!(reply.ok, "{reply:?}");
    assert_eq!(reply.result["output"], 3162);
}

#[tokio::test]
#[ignore = "requires Rust guest toolchain and LOOM_COMPILER_CACHE_OWNER"]
async fn eval_accepts_cells_that_name_variables_like_the_words_admission_watches() {
    // `path`, `link` and `env` are ordinary identifiers as macro arguments; only a macro that glues
    // a metavariable into an attribute or a macro name is refused, where it is defined.
    let service = service();
    let cell = "let path = \"a/b\";\nlet link = 3usize;\nlet env = vec![1, 2];\nformat!(\"{path} {} {:?} {}\", link, env, path.len())";
    let reply = eval(&service, json!({"source":cell})).await;
    assert!(reply.ok, "{reply:?}");
    assert_eq!(reply.result["output"], "a/b 3 [1, 2] 3");
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

#[tokio::test]
#[ignore = "requires Rust guest toolchain and LOOM_COMPILER_CACHE_OWNER"]
async fn run_with_sites_names_the_cell_line_that_performed_each_effect() {
    // Effects handled inside the guest never reach the host handler, so the only record of where
    // they came from is the call stack at the `loom.perform` import, read through the module's DWARF.
    let service = service();
    let source = "use loom::{Continuation, Effect, Reply, Value};\n\
pub fn run(_n: u32) -> u32 {\n\
    let mut seen = 0;\n\
    loom::handle([\"tick\"], |_e: Effect, _k: Continuation| {\n\
        seen += 1;\n\
        Reply::Resume(Value::Null)\n\
    }, || {\n\
        let _ = loom::perform::<()>(\"tick\", 1);\n\
        let _ = loom::perform::<()>(\"tick\", 2);\n\
        let _ = loom::perform::<()>(\"tick\", 3);\n\
    }).unwrap();\n\
    seen\n\
}\n";
    let built = eval(&service, json!({"source":source,"args":[0]})).await;
    assert!(built.ok, "{built:?}");
    let hash = built.result["hash"].as_str().unwrap();
    let reply = service
        .command(CommandRequest {
            session: None,
            command: "run".into(),
            args: json!({"target":hash,"args":[0],"sites":true}),
        })
        .await;
    assert!(reply.ok, "{reply:?}");
    let sites = reply.result["sites"].as_array().unwrap();
    assert_eq!(sites.len(), 3, "one entry per performed effect: {sites:?}");
    // Lines 8, 9 and 10 of the cell each issue one `perform`; the innermost cell line is listed.
    let lines: Vec<u64> = sites
        .iter()
        .map(|frames| {
            frames
                .as_array()
                .unwrap()
                .iter()
                .filter_map(Value::as_u64)
                .find(|line| (8..=10).contains(line))
                .expect("a cell line on the stack")
        })
        .collect();
    assert_eq!(lines, [8, 9, 10]);
    // Without the flag there is no `sites` key and no backtrace cost.
    let plain = service
        .command(CommandRequest {
            session: None,
            command: "run".into(),
            args: json!({"target":hash,"args":[0]}),
        })
        .await;
    assert!(plain.ok && plain.result.get("sites").is_none());
}

#[tokio::test]
#[ignore = "requires Rust guest toolchain and LOOM_COMPILER_CACHE_OWNER"]
async fn a_compile_error_is_reported_on_the_line_the_caller_wrote_past_comments_and_blank_lines() {
    // The compiler reads the cell unparsed (no comments, no blank lines), so its own line numbers are
    // smaller; the reply must count the caller's text.
    let service = service();
    let source = "// one\n// two\n\npub fn f() -> u32 {\n\n    // about to fail\n    let x: u32 = \"no\";\n    x\n}\n";
    let reply = eval(&service, json!({"source": source})).await;
    assert!(!reply.ok, "{reply:?}");
    let error = reply.diagnostics.iter().find(|d| d.message.contains("mismatched")).expect("a type error");
    assert_eq!(error.line, 7, "{:?}", reply.diagnostics);
}

#[tokio::test]
#[ignore = "requires Rust guest toolchain and LOOM_COMPILER_CACHE_OWNER"]
async fn a_dyn_call_is_refused_unless_the_host_admits_unresolved_calls_for_unpolicied_code() {
    // Effect rows come from resolving every call. A call through `&dyn Fn` cannot be resolved, so by default
    // it is an error; a host started with LOOM_ALLOW_UNRESOLVED_CALLS=1 admits it for a definition with no
    // allowed-effects list (the row becomes `unknown`), and never for one that has a list.
    let service = service();
    // A table of trait objects that rustc cannot devirtualize (a lone `&dyn Fn` bound to one closure is folded
    // into a direct call and needs no admission).
    let source = "pub fn f(x: u32) -> u32 {\n    let double = |v: u32| v * 2;\n    let triple = |v: u32| v * 3;\n    let table: [(&dyn Fn(u32) -> u32, u32); 2] = [(&double, 10), (&triple, 100)];\n    let mut total = 0;\n    for (op, base) in table {\n        total += op(x) + base;\n    }\n    total\n}\n";
    let reply = eval(&service, json!({"source": source, "args": [4]})).await;
    if loom_build::host_allows_unresolved_calls() {
        assert!(reply.ok, "{reply:?}");
        assert_eq!(reply.result["output"], 4 * 2 + 10 + 4 * 3 + 100);
        let policed = eval(&service, json!({"source": source, "args": [4], "allowed_effects": []})).await;
        assert!(!policed.ok && policed.diagnostics.iter().any(|d| d.message.contains("cannot be resolved")));
    } else {
        assert!(!reply.ok, "{reply:?}");
        let message = format!("{reply:?}");
        assert!(message.contains("cannot resolve") || message.contains("cannot be resolved"), "{message}");
    }
}

#[tokio::test]
#[ignore = "requires Rust guest toolchain and LOOM_COMPILER_CACHE_OWNER"]
async fn one_source_has_one_hash_on_every_daemon() {
    // A definition that names the loom SDK (or any crate with a build script) hashes that crate by its strict
    // version hash, which moves with the path of every file it was compiled from, including build-script
    // output under the daemon's own build directory. Definitions shared through git are checked against the
    // hashes in `loom.lock`, so the same source must hash the same in two build directories.
    let source = "pub fn count(bytes: Vec<u8>) -> usize { loom::Packed::<f32>::from_bytes(&bytes).map(|p| p.0.len()).unwrap_or(0) }";
    let mut hashes = Vec::new();
    for _ in 0..2 {
        let build = tempfile::tempdir().unwrap();
        // SAFETY: the only test that touches this variable; `Builder::new` reads it once per service.
        unsafe { std::env::set_var("LOOM_BUILD_DIR", build.path()) };
        let service = service();
        let reply = eval(&service, json!({"source": source, "args": [[0, 0, 128, 63]]})).await;
        assert!(reply.ok, "{reply:?}");
        hashes.push(reply.result["hash"].as_str().unwrap().to_owned());
    }
    assert_eq!(hashes[0], hashes[1], "the same source hashed differently in two build directories");
}
