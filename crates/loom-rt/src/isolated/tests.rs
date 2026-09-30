use super::*;
use loom_proto::isolated::encode_payload;

/// Publish `artifact` as a Rust definition whose exports are `(name, params)`
/// pairs, each with the same inferred effect row `labels`.
fn register(
    store: &Store,
    artifact: &[u8],
    exports: &[(&str, usize)],
    labels: &[&str],
) -> Result<String> {
    let component = store.put("component", artifact)?;
    let source = format!("isolated fixture {component}");
    let deps = std::collections::BTreeMap::new();
    let hash = blake3::hash(&loom_proto::definition_identity(
        loom_proto::Lang::Rust,
        &source,
        &deps,
        None,
    )?)
    .to_hex()
    .to_string();
    let effects = loom_proto::EffectSet {
        labels: labels.iter().map(|label| label.to_string()).collect(),
        unknown: false,
    };
    store.define(
        &loom_proto::Def {
            hash: hash.clone(),
            lang: loom_proto::Lang::Rust,
            component_hash: Some(component),
            sig: loom_proto::TypeSig {
                exports: exports
                    .iter()
                    .map(|(name, params)| loom_proto::ExportSig {
                        name: name.to_string(),
                        params: (0..*params)
                            .map(|index| loom_proto::ParamSig {
                                name: format!("argument_{index}"),
                                shape: loom_proto::ValueShape::Value,
                            })
                            .collect(),
                        returns: loom_proto::ValueShape::Value,
                        effects: effects.clone(),
                    })
                    .collect(),
                effects,
            },
            allowed_effects: None,
            observed_effects: Vec::new(),
        },
        None,
        &source,
        &deps,
    )?;
    Ok(hash)
}

/// A core module whose `main` hands the embedded bytes to the named `loom`
/// import and returns whatever `body` builds from the packed reply in `$packed`.
fn module(import: &str, data: &[u8], body: &str) -> Result<Vec<u8>> {
    let data_text = data
        .iter()
        .map(|byte| format!("\\{byte:02x}"))
        .collect::<String>();
    let mut artifact = wat::parse_str(format!(
        r#"(module
        (import "env" "memory" (memory 1 1 shared))
        (import "loom" "{import}" (func $host (param i32 i32) (result i64)))
        (global (export "__stack_pointer") (mut i32) (i32.const 65536))
        (global (export "__loom_stack_low") (mut i32) (i32.const 32768))
        (global (export "__loom_stack_high") (mut i32) (i32.const 65536))
        (global $heap (mut i32) (i32.const 8192))
        (func $alloc (export "loom_alloc") (param $size i32) (param i32) (result i32)
            (local $pointer i32)
            global.get $heap local.tee $pointer
            local.get $size i32.add global.set $heap local.get $pointer)
        (func (export "loom_dealloc") (param i32 i32 i32))
        (data (i32.const 1024) "{data_text}")
        (func (export "loom_call_main") (param i32 i32) (result i64)
            (local $packed i64) (local $src i32) (local $len i32) (local $dst i32)
            i32.const 1024 i32.const {length} call $host
            local.set $packed
            {body})
    )"#,
        length = data.len()
    ))?;
    loom_proto::core_protocol::stamp(&mut artifact);
    Ok(artifact)
}

/// `main` issues exactly the embedded isolated-call frame through `loom.call`
/// and returns the host's response frame as its own result frame (the two
/// frames share one layout).
fn caller_module(frame: &[u8]) -> Result<Vec<u8>> {
    module("call", frame, "local.get $packed")
}

/// `main` performs the embedded descriptor through `loom.perform` and returns
/// the whole reply envelope as a successful result frame, so the host hands
/// the envelope (`{ok: ...}` or `{error: ...}`) back as the entry's value.
fn perform_module(descriptor: &[u8]) -> Result<Vec<u8>> {
    module(
        "perform",
        descriptor,
        r#"local.get $packed i32.wrap_i64 local.set $src
            local.get $packed i64.const 32 i64.shr_u i32.wrap_i64 local.set $len
            local.get $len i32.const 1 i32.add i32.const 1 call $alloc local.set $dst
            local.get $dst i32.const 0 i32.store8
            local.get $dst i32.const 1 i32.add local.get $src local.get $len memory.copy
            local.get $len i32.const 1 i32.add i64.extend_i32_u i64.const 32 i64.shl
            local.get $dst i64.extend_i32_u i64.or"#,
    )
}

fn call_error(error: &anyhow::Error) -> Option<&CallError> {
    error.downcast_ref::<CallError>()
}

#[tokio::test]
async fn self_recursive_definition_stops_at_the_depth_limit_before_instantiating() -> Result<()> {
    let store = Store::memory()?;
    let frame = Request {
        target: Target::This,
        entry: "",
        argc: 0,
        payload: &[0x80],
    }
    .encode();
    let hash = register(&store, &caller_module(&frame)?, &[("main", 0)], &["call"])?;
    let runtime = Runtime::new(store)?;
    let error = tokio::time::timeout(Duration::from_secs(60), runtime.call_def(&hash, json!([])))
        .await?
        .unwrap_err();
    assert_eq!(
        call_error(&error),
        Some(&CallError::DepthExceeded { depth: MAX_DEPTH }),
        "{error:#}"
    );
    // MAX_DEPTH nested calls were attempted: the root plus one per level below
    // it, and the deepest refused without instantiating a callee.
    let samples = runtime.isolated_call_round_trip_us();
    assert_eq!(samples["samples"], json!(MAX_DEPTH));
    Ok(())
}

#[tokio::test]
async fn missing_definition_and_arity_mismatch_are_structured_and_precede_instantiation()
-> Result<()> {
    let store = Store::memory()?;
    let unary = register(&store, b"not a wasm module", &[("main", 1)], &[])?;
    let runtime = Runtime::new(store)?;
    let effects = EffectContext::default();
    let missing = runtime
        .isolated_call(
            Request {
                target: Target::Hash([1; 32]),
                entry: "",
                argc: 0,
                payload: &[0x80],
            },
            "root",
            0,
            &effects,
        )
        .await
        .unwrap_err();
    assert_eq!(
        missing,
        CallError::NotFound {
            hash: "01".repeat(32)
        }
    );
    let digest = loom_proto::isolated::parse_digest(&unary).unwrap();
    let arity = runtime
        .isolated_call(
            Request {
                target: Target::Hash(digest),
                entry: "main",
                argc: 0,
                payload: &[0x80],
            },
            "root",
            1,
            &effects,
        )
        .await
        .unwrap_err();
    assert_eq!(
        arity,
        CallError::Arity {
            hash: unary.clone(),
            entry: "main".into(),
            expected: 1,
            actual: 0,
        }
    );
    let unknown_entry = runtime
        .isolated_call(
            Request {
                target: Target::Hash(digest),
                entry: "absent",
                argc: 1,
                payload: &encode_payload(&(1u8,)).unwrap(),
            },
            "root",
            2,
            &effects,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(unknown_entry, CallError::Trapped { ref message, .. } if message.contains("requires an entry name")),
        "{unknown_entry}"
    );
    Ok(())
}

#[tokio::test]
async fn denied_and_unresolvable_calls_never_reach_the_store() -> Result<()> {
    let runtime = Runtime::new(Store::memory()?)?;
    let request = Request {
        target: Target::Hash([2; 32]),
        entry: "",
        argc: 0,
        payload: &[0x80],
    };
    let trace = trace::ExecutionTrace::fresh("root");
    let denied = EffectContext {
        trace: Some(trace.clone()),
        ..EffectContext::default().delegated("parent", Some(&[]))
    };
    let error = runtime
        .isolated_call(request, "root", 0, &denied)
        .await
        .unwrap_err();
    assert_eq!(
        error,
        CallError::Denied {
            effect: "call".into(),
            hash: "02".repeat(32),
        }
    );
    let bundle = trace.snapshot(None, true)?;
    assert_eq!(bundle.trace.entries.len(), 1);
    assert!(matches!(
        &bundle.trace.entries[0].outcome,
        loom_proto::TraceOutcome::Error { message } if message.contains("not allowed")
    ));
    assert!(bundle.observations.is_empty());

    let unresolved = runtime
        .isolated_call(
            Request {
                target: Target::This,
                ..request
            },
            "root",
            1,
            &EffectContext::default(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(unresolved, CallError::Decode { .. }),
        "{unresolved}"
    );

    let (sender, _receiver) = tokio::sync::mpsc::channel::<call::Request>(1);
    let borrowed = EffectContext {
        root: Some(sender),
        ..EffectContext::default()
    };
    let error = runtime
        .isolated_call(request, "root", 2, &borrowed)
        .await
        .unwrap_err();
    assert!(matches!(error, CallError::Denied { .. }), "{error}");

    let deep = EffectContext {
        depth: MAX_DEPTH,
        ..EffectContext::default()
    };
    let error = runtime
        .isolated_call(request, "root", 3, &deep)
        .await
        .unwrap_err();
    assert_eq!(error, CallError::DepthExceeded { depth: MAX_DEPTH });
    assert_eq!(runtime.isolated_call_round_trip_us()["samples"], json!(0));
    Ok(())
}

#[tokio::test]
async fn perform_refuses_the_value_shaped_call_descriptor_from_core_guests() -> Result<()> {
    let store = Store::memory()?;
    let descriptor = json!({"op":"call","args":{"def":"01".repeat(32),"args":[]}});
    let hash = register(
        &store,
        &perform_module(&encode(&descriptor)?)?,
        &[("main", 0)],
        &["call"],
    )?;
    let runtime = Runtime::new(store)?;
    let envelope = runtime.call_def(&hash, json!([])).await?;
    let refusal = envelope["error"]
        .as_str()
        .context("perform error envelope")?;
    assert!(refusal.contains("use loom::isolated::call"), "{envelope}");
    assert_eq!(runtime.isolated_call_round_trip_us()["samples"], json!(0));
    // The control: an ordinary op through the same fixture still succeeds.
    let store = Store::memory()?;
    let hash = register(
        &store,
        &perform_module(&encode(&json!({"op":"now","args":null}))?)?,
        &[("main", 0)],
        &["now"],
    )?;
    let envelope = Runtime::new(store)?.call_def(&hash, json!([])).await?;
    assert!(envelope["ok"].is_number(), "{envelope}");
    Ok(())
}

/// Not run in this lane. Build the fixture with the guest toolchain
/// (`scripts/bench/effects-fixtures/isolated.rs`, an admitted core module) and
/// point `LOOM_ISOLATED_BENCH_MODULE` at it. `main(rounds, size)` performs
/// `rounds` isolated calls of `echo` on itself, each carrying `size` payload
/// bytes as a CBOR byte string, and returns the summed echoed lengths. The
/// printed median and p99 are host-side per-call durations from parsed header
/// to callee result bytes (`Runtime::isolated_call_round_trip_us`).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires LOOM_ISOLATED_BENCH_MODULE built from scripts/bench/effects-fixtures/isolated.rs"]
async fn isolated_call_payload_benchmark() -> Result<()> {
    const ROUNDS: u64 = 1_000;
    const SIZE: u64 = 1 << 20;
    let bytes = std::fs::read(std::env::var("LOOM_ISOLATED_BENCH_MODULE")?)?;
    anyhow::ensure!(
        loom_proto::core_protocol::is_current(&bytes),
        "benchmark requires a current admitted core artifact"
    );
    let store = Store::memory()?;
    let hash = register(&store, &bytes, &[("main", 2), ("echo", 1)], &["call"])?;
    let runtime = Runtime::new(store)?;
    let started = Instant::now();
    let total = runtime
        .call_entry_timed(&hash, "main", json!([ROUNDS, SIZE]))
        .await?;
    let elapsed = started.elapsed();
    anyhow::ensure!(
        total.value == json!(ROUNDS * SIZE),
        "fixture echoed {} bytes, expected {}",
        total.value,
        ROUNDS * SIZE
    );
    let samples = runtime.isolated_call_round_trip_us();
    anyhow::ensure!(
        samples["samples"] == json!(ROUNDS),
        "expected {ROUNDS} isolated calls, observed {}",
        samples["samples"]
    );
    println!(
        "isolated call, {SIZE} B payload, {ROUNDS} calls: median {} us, p99 {} us, wall {:.1} ms",
        samples["median"],
        samples["p99"],
        elapsed.as_secs_f64() * 1000.0
    );
    Ok(())
}

/// A core module whose `main` returns a fixed success frame and touches nothing.
fn constant_module(result: u8) -> Result<Vec<u8>> {
    let mut artifact = wat::parse_str(format!(
        r#"(module
        (import "env" "memory" (memory 1 1 shared))
        (global (export "__stack_pointer") (mut i32) (i32.const 65536))
        (global (export "__loom_stack_low") (mut i32) (i32.const 32768))
        (global (export "__loom_stack_high") (mut i32) (i32.const 65536))
        (func (export "loom_alloc") (param i32 i32) (result i32) i32.const 8192)
        (func (export "loom_dealloc") (param i32 i32 i32))
        (data (i32.const 1024) "\00\{result:02x}")
        (func (export "loom_call_main") (param i32 i32) (result i64)
            i64.const 2 i64.const 32 i64.shl i64.const 1024 i64.or)
    )"#
    ))?;
    loom_proto::core_protocol::stamp(&mut artifact);
    Ok(artifact)
}

fn traced() -> EffectContext {
    EffectContext {
        trace: Some(trace::ExecutionTrace::fresh("root")),
        ..EffectContext::default()
    }
}

async fn call_main(
    runtime: &Runtime,
    hash: &str,
    payload: &[u8],
    occurrence: i64,
    effects: &EffectContext,
) -> Result<Vec<u8>, CallError> {
    runtime
        .isolated_call(
            Request {
                target: Target::Hash(loom_proto::isolated::parse_digest(hash).unwrap()),
                entry: "main",
                argc: 0,
                payload,
            },
            "root",
            occurrence,
            effects,
        )
        .await
}

#[tokio::test]
async fn a_pure_callee_runs_once_per_arguments_and_the_cache_answers_the_rest() -> Result<()> {
    let store = Store::memory()?;
    let pure = register(&store, &constant_module(7)?, &[("main", 0)], &[])?;
    let runtime = Runtime::new(store)?;
    let effects = traced();

    let first = call_main(&runtime, &pure, &[0x80], 0, &effects).await.unwrap();
    assert_eq!(runtime.call_result_stats().stores, 1);
    let second = call_main(&runtime, &pure, &[0x80], 1, &effects).await.unwrap();
    assert_eq!(first, second);
    let stats = runtime.call_result_stats();
    assert_eq!((stats.hits, stats.stores, stats.entries), (1, 1, 1));

    // Different arguments are a different call: a miss, then its own entry.
    call_main(&runtime, &pure, &[0x81], 2, &effects).await.unwrap();
    assert_eq!(runtime.call_result_stats().entries, 2);

    // Dropping the callee's results makes the next identical call run again.
    assert_eq!(runtime.clear_call_results(Some(&pure)), 2);
    call_main(&runtime, &pure, &[0x80], 3, &effects).await.unwrap();
    assert_eq!(runtime.call_result_stats().stores, 3);
    Ok(())
}

#[tokio::test]
async fn nothing_is_cached_without_a_trace_or_for_a_callee_with_effects() -> Result<()> {
    let store = Store::memory()?;
    let pure = register(&store, &constant_module(7)?, &[("main", 0)], &[])?;
    let effectful = register(&store, &constant_module(8)?, &[("main", 0)], &["now"])?;
    let runtime = Runtime::new(store)?;

    // No trace to prove the call did nothing else: run, do not remember.
    call_main(&runtime, &pure, &[0x80], 0, &EffectContext::default()).await.unwrap();
    // A row that names an effect is never eligible, whatever the callee did.
    call_main(&runtime, &effectful, &[0x80], 0, &traced()).await.unwrap();
    assert_eq!(runtime.call_result_stats().stores, 0);
    assert_eq!(runtime.call_result_stats().hits, 0);
    Ok(())
}

#[tokio::test]
async fn a_call_that_recorded_an_effect_is_not_stored_even_when_its_row_is_empty() -> Result<()> {
    // The static row can undercount; the trace decides. A callee declared pure that
    // performs `now` records an entry under its call scope, so its result is not kept.
    let store = Store::memory()?;
    let descriptor = loom_proto::encode(&json!({"op":"now","args":null})).map_err(anyhow::Error::msg)?;
    let hidden = register(&store, &perform_module(&descriptor)?, &[("main", 0)], &[])?;
    let runtime = Runtime::new(store)?;
    let effects = traced();
    let _ = call_main(&runtime, &hidden, &[0x80], 0, &effects).await;
    assert_eq!(runtime.call_result_stats().stores, 0, "no effect-touching call is remembered");
    Ok(())
}

/// A kernel family with one pure op that answers CBOR `7` and counts its calls.
struct Cbor7(std::sync::atomic::AtomicU64);
impl crate::HostKernel for Cbor7 {
    fn family(&self) -> &str {
        "test"
    }
    fn version(&self) -> u32 {
        1
    }
    fn ops(&self) -> &[&'static str] {
        &["cbor7"]
    }
    fn call(&self, _: &crate::KernelContext<'_>, _: &str, _: &[&[u8]]) -> Result<Vec<u8>, String> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(vec![0x07])
    }
}

/// A callee whose `main` calls kernel op `test.cbor7` through the `loom.kernel`
/// import (no buffers) and returns the reply as its own result frame: the
/// kernel's reply is the result bytes then a tag byte; the frame is a tag byte
/// then the result.
fn kernel_module(salt: u8) -> Result<Vec<u8>> {
    let mut artifact = wat::parse_str(format!(
        r#"(module
        (import "env" "memory" (memory 1 1 shared))
        (import "loom" "kernel" (func $kernel (param i32 i32 i32 i32) (result i64)))
        (global (export "__stack_pointer") (mut i32) (i32.const 65536))
        (global (export "__loom_stack_low") (mut i32) (i32.const 32768))
        (global (export "__loom_stack_high") (mut i32) (i32.const 65536))
        (global $heap (mut i32) (i32.const 8192))
        (func $alloc (export "loom_alloc") (param $size i32) (param i32) (result i32)
            (local $pointer i32)
            global.get $heap local.tee $pointer
            local.get $size i32.add global.set $heap local.get $pointer)
        (func (export "loom_dealloc") (param i32 i32 i32))
        (data (i32.const 1024) "test.cbor7")
        (data (i32.const 1100) "\{salt:02x}")
        (func (export "loom_call_main") (param i32 i32) (result i64)
            (local $packed i64) (local $src i32) (local $len i32) (local $dst i32)
            i32.const 1024 i32.const 10 i32.const 1104 i32.const 0 call $kernel
            local.set $packed
            local.get $packed i32.wrap_i64 local.set $src
            local.get $packed i64.const 32 i64.shr_u i32.wrap_i64 local.set $len
            local.get $len i32.const 1 call $alloc local.set $dst
            local.get $dst i32.const 0 i32.store8
            local.get $dst i32.const 1 i32.add local.get $src local.get $len i32.const 1 i32.sub memory.copy
            local.get $len i64.extend_i32_u i64.const 32 i64.shl
            local.get $dst i64.extend_i32_u i64.or)
    )"#
    ))?;
    loom_proto::core_protocol::stamp(&mut artifact);
    Ok(artifact)
}

#[tokio::test]
async fn a_guest_calls_a_host_kernel_through_the_import_and_reads_its_reply() -> Result<()> {
    let store = Store::memory()?;
    let hash = register(&store, &kernel_module(1)?, &[("main", 0)], &["kernel"])?;
    let runtime = Runtime::new(store)?;
    let kernel = Arc::new(Cbor7(Default::default()));
    runtime.register_kernel(kernel.clone())?;
    let value = tokio::time::timeout(Duration::from_secs(60), runtime.call_def(&hash, json!([])))
        .await??;
    assert_eq!(value, json!(7));
    assert_eq!(kernel.0.load(std::sync::atomic::Ordering::Relaxed), 1);
    Ok(())
}

#[tokio::test]
async fn an_unregistered_op_or_a_row_without_the_kernel_label_is_refused_not_run() -> Result<()> {
    let store = Store::memory()?;
    // The row is empty, so the callee's allowed effects exclude `kernel`.
    let denied = register(&store, &kernel_module(2)?, &[("main", 0)], &[])?;
    let runtime = Runtime::new(store)?;
    let kernel = Arc::new(Cbor7(Default::default()));
    runtime.register_kernel(kernel.clone())?;
    let attempt = runtime.call_def(&denied, json!([])).await;
    assert!(attempt.is_err() || attempt.as_ref().is_ok_and(|value| *value != json!(7)), "{attempt:?}");
    assert_eq!(kernel.0.load(std::sync::atomic::Ordering::Relaxed), 0, "the kernel never ran");
    Ok(())
}

#[tokio::test]
async fn a_callee_that_only_calls_pure_kernels_is_cached_until_the_kernel_versions_change() -> Result<()> {
    let store = Store::memory()?;
    let hash = register(&store, &kernel_module(3)?, &[("main", 0)], &["kernel"])?;
    let runtime = Runtime::new(store)?;
    let kernel = Arc::new(Cbor7(Default::default()));
    runtime.register_kernel(kernel.clone())?;
    let effects = traced();
    let ran = || kernel.0.load(std::sync::atomic::Ordering::Relaxed);

    let first = call_main(&runtime, &hash, &[0x80], 0, &effects).await.unwrap();
    let second = call_main(&runtime, &hash, &[0x80], 1, &effects).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(ran(), 1, "the second call was answered from the result cache");
    assert_eq!(runtime.call_result_stats().hits, 1);

    // Another kernel family appears: the fingerprint moves, so the old result is
    // not served, and the callee runs again.
    struct Other;
    impl crate::HostKernel for Other {
        fn family(&self) -> &str {
            "other"
        }
        fn version(&self) -> u32 {
            1
        }
        fn ops(&self) -> &[&'static str] {
            &[]
        }
        fn call(&self, _: &crate::KernelContext<'_>, _: &str, _: &[&[u8]]) -> Result<Vec<u8>, String> {
            Ok(Vec::new())
        }
    }
    runtime.register_kernel(Arc::new(Other))?;
    call_main(&runtime, &hash, &[0x80], 2, &effects).await.unwrap();
    assert_eq!(ran(), 2, "a changed kernel set is a different key");
    Ok(())
}

#[tokio::test]
async fn a_cached_kernel_result_is_not_served_to_a_caller_that_may_not_use_kernels() -> Result<()> {
    let store = Store::memory()?;
    let hash = register(&store, &kernel_module(4)?, &[("main", 0)], &["kernel"])?;
    let runtime = Runtime::new(store)?;
    let kernel = Arc::new(Cbor7(Default::default()));
    runtime.register_kernel(kernel.clone())?;
    let allowed = traced();
    call_main(&runtime, &hash, &[0x80], 0, &allowed).await.unwrap();
    call_main(&runtime, &hash, &[0x80], 1, &allowed).await.unwrap();
    assert_eq!(runtime.call_result_stats().hits, 1, "the permitted caller is served from the cache");

    let restricted = EffectContext {
        allowed: Some(["call".to_owned()].into()),
        ..traced()
    };
    let ran = kernel.0.load(std::sync::atomic::Ordering::Relaxed);
    let reference = call_main(&runtime, &hash, &[0x80], 3, &allowed).await.unwrap();
    let hits = runtime.call_result_stats().hits;
    let outcome = call_main(&runtime, &hash, &[0x80], 2, &restricted).await;
    assert_eq!(runtime.call_result_stats().hits, hits, "a caller without `kernel` gets no cache hit");
    assert_eq!(kernel.0.load(std::sync::atomic::Ordering::Relaxed), ran, "and the kernel did not run for it");
    assert!(outcome.is_err() || outcome.as_ref().is_ok_and(|bytes| *bytes != reference), "{outcome:?}");
    Ok(())
}
