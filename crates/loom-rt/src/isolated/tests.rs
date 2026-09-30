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

    let first = call_main(&runtime, &pure, &[0x80], 0, &effects)
        .await
        .unwrap();
    assert_eq!(runtime.call_result_stats().stores, 1);
    let second = call_main(&runtime, &pure, &[0x80], 1, &effects)
        .await
        .unwrap();
    assert_eq!(first, second);
    let stats = runtime.call_result_stats();
    assert_eq!((stats.hits, stats.stores, stats.entries), (1, 1, 1));

    // Different arguments are a different call: a miss, then its own entry.
    call_main(&runtime, &pure, &[0x81], 2, &effects)
        .await
        .unwrap();
    assert_eq!(runtime.call_result_stats().entries, 2);

    // Dropping the callee's results makes the next identical call run again.
    assert_eq!(runtime.clear_call_results(Some(&pure)), 2);
    call_main(&runtime, &pure, &[0x80], 3, &effects)
        .await
        .unwrap();
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
    call_main(&runtime, &pure, &[0x80], 0, &EffectContext::default())
        .await
        .unwrap();
    // A row that names an effect is never eligible, whatever the callee did.
    call_main(&runtime, &effectful, &[0x80], 0, &traced())
        .await
        .unwrap();
    assert_eq!(runtime.call_result_stats().stores, 0);
    assert_eq!(runtime.call_result_stats().hits, 0);
    Ok(())
}

#[tokio::test]
async fn a_call_that_recorded_an_effect_is_not_stored_even_when_its_row_is_empty() -> Result<()> {
    // The static row can undercount; the trace decides. A callee declared pure that
    // performs `now` records an entry under its call scope, so its result is not kept.
    let store = Store::memory()?;
    let descriptor =
        loom_proto::encode(&json!({"op":"now","args":null})).map_err(anyhow::Error::msg)?;
    let hidden = register(&store, &perform_module(&descriptor)?, &[("main", 0)], &[])?;
    let runtime = Runtime::new(store)?;
    let effects = traced();
    let _ = call_main(&runtime, &hidden, &[0x80], 0, &effects).await;
    assert_eq!(
        runtime.call_result_stats().stores,
        0,
        "no effect-touching call is remembered"
    );
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
    let value =
        tokio::time::timeout(Duration::from_secs(60), runtime.call_def(&hash, json!([]))).await??;
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
    assert!(
        attempt.is_err() || attempt.as_ref().is_ok_and(|value| *value != json!(7)),
        "{attempt:?}"
    );
    assert_eq!(
        kernel.0.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "the kernel never ran"
    );
    Ok(())
}

#[tokio::test]
async fn a_callee_that_only_calls_pure_kernels_is_cached_until_the_kernel_versions_change()
-> Result<()> {
    let store = Store::memory()?;
    let hash = register(&store, &kernel_module(3)?, &[("main", 0)], &["kernel"])?;
    let runtime = Runtime::new(store)?;
    let kernel = Arc::new(Cbor7(Default::default()));
    runtime.register_kernel(kernel.clone())?;
    let effects = traced();
    let ran = || kernel.0.load(std::sync::atomic::Ordering::Relaxed);

    let first = call_main(&runtime, &hash, &[0x80], 0, &effects)
        .await
        .unwrap();
    let second = call_main(&runtime, &hash, &[0x80], 1, &effects)
        .await
        .unwrap();
    assert_eq!(first, second);
    assert_eq!(
        ran(),
        1,
        "the second call was answered from the result cache"
    );
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
        fn call(
            &self,
            _: &crate::KernelContext<'_>,
            _: &str,
            _: &[&[u8]],
        ) -> Result<Vec<u8>, String> {
            Ok(Vec::new())
        }
    }
    runtime.register_kernel(Arc::new(Other))?;
    call_main(&runtime, &hash, &[0x80], 2, &effects)
        .await
        .unwrap();
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
    call_main(&runtime, &hash, &[0x80], 0, &allowed)
        .await
        .unwrap();
    call_main(&runtime, &hash, &[0x80], 1, &allowed)
        .await
        .unwrap();
    assert_eq!(
        runtime.call_result_stats().hits,
        1,
        "the permitted caller is served from the cache"
    );

    let restricted = EffectContext {
        allowed: Some(["call".to_owned()].into()),
        ..traced()
    };
    let ran = kernel.0.load(std::sync::atomic::Ordering::Relaxed);
    let reference = call_main(&runtime, &hash, &[0x80], 3, &allowed)
        .await
        .unwrap();
    let hits = runtime.call_result_stats().hits;
    let outcome = call_main(&runtime, &hash, &[0x80], 2, &restricted).await;
    assert_eq!(
        runtime.call_result_stats().hits,
        hits,
        "a caller without `kernel` gets no cache hit"
    );
    assert_eq!(
        kernel.0.load(std::sync::atomic::Ordering::Relaxed),
        ran,
        "and the kernel did not run for it"
    );
    assert!(
        outcome.is_err() || outcome.as_ref().is_ok_and(|bytes| *bytes != reference),
        "{outcome:?}"
    );
    Ok(())
}

struct SlowCbor7(std::sync::atomic::AtomicU64);
impl crate::HostKernel for SlowCbor7 {
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
        std::thread::sleep(Duration::from_millis(400));
        Ok(vec![0x07])
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identical_pure_calls_in_flight_at_once_run_the_callee_once() -> Result<()> {
    let store = Store::memory()?;
    let hash = register(&store, &kernel_module(5)?, &[("main", 0)], &["kernel"])?;
    let runtime = Runtime::new(store)?;
    let kernel = Arc::new(SlowCbor7(Default::default()));
    runtime.register_kernel(kernel.clone())?;
    let effects = traced();
    let calls = (0..5).map(|i| call_main(&runtime, &hash, &[0x80], i, &effects));
    let results = futures::future::join_all(calls).await;
    assert!(
        results.iter().all(|result| result
            .as_ref()
            .is_ok_and(|bytes| bytes == results[0].as_ref().unwrap())),
        "{results:?}"
    );
    assert_eq!(
        kernel.0.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "five identical concurrent calls ran the kernel once"
    );
    assert!(
        runtime.inner.inflight.lock().unwrap().is_empty(),
        "the in-flight table drains"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_batch_runs_its_calls_at_the_same_time_and_answers_in_request_order() -> Result<()> {
    let store = Store::memory()?;
    let hashes: Vec<String> = (10u8..14)
        .map(|salt| register(&store, &kernel_module(salt)?, &[("main", 0)], &["kernel"]))
        .collect::<Result<_>>()?;
    let runtime = Runtime::new(store)?;
    let kernel = Arc::new(SlowCbor7(Default::default()));
    runtime.register_kernel(kernel.clone())?;
    let mut frames: Vec<Vec<u8>> = hashes
        .iter()
        .map(|hash| {
            Request {
                target: Target::Hash(loom_proto::isolated::parse_digest(hash).unwrap()),
                entry: "main",
                argc: 0,
                payload: &[0x80],
            }
            .encode()
        })
        .collect();
    frames.insert(2, vec![0xff, 0xff]);
    let slices: Vec<&[u8]> = frames.iter().map(Vec::as_slice).collect();
    let started = Instant::now();
    let outcomes = runtime.isolated_batch(&slices, "root", 0, &traced()).await;
    let elapsed = started.elapsed();
    assert_eq!(outcomes.len(), 5);
    assert!(
        outcomes[2].is_err(),
        "a malformed frame fails alone: {:?}",
        outcomes[2]
    );
    for index in [0, 1, 3, 4] {
        assert!(
            outcomes[index].is_ok(),
            "call {index}: {:?}",
            outcomes[index]
        );
    }
    assert_eq!(kernel.0.load(std::sync::atomic::Ordering::Relaxed), 4);
    assert!(
        elapsed < Duration::from_millis(1400),
        "four 400 ms callees took {elapsed:?}: they ran one after another"
    );
    Ok(())
}

/// A guest that yields the values 0..`count` (one byte each, CBOR small uints) and calls a kernel after
/// each accepted yield, so the kernel's counter says how far the producer got. It stops at the first
/// nonzero yield result and returns CBOR 7 from a final kernel call.
fn generator_module(count: u32, salt: u8) -> Result<Vec<u8>> {
    let mut artifact = wat::parse_str(format!(
        r#"(module
        (import "env" "memory" (memory 1 1 shared))
        (import "loom" "kernel" (func $kernel (param i32 i32 i32 i32) (result i64)))
        (import "loom" "yield_value" (func $yield (param i32 i32) (result i32)))
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
            (local $i i32) (local $packed i64) (local $src i32) (local $len i32) (local $dst i32)
            (block $done
              (loop $again
                i32.const 1200 local.get $i i32.store8
                i32.const 1200 i32.const 1 call $yield
                br_if $done
                i32.const 1024 i32.const 10 i32.const 1104 i32.const 0 call $kernel drop
                local.get $i i32.const 1 i32.add local.tee $i
                i32.const {count} i32.lt_u br_if $again))
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_generator_yields_in_order_then_returns_and_a_plain_call_is_told_it_is_not_streaming()
-> Result<()> {
    let store = Store::memory()?;
    let hash = register(
        &store,
        &generator_module(3, 20)?,
        &[("main", 0)],
        &["yield", "kernel"],
    )?;
    let runtime = Runtime::new(store)?;
    runtime.register_kernel(Arc::new(Cbor7(Default::default())))?;
    let mut stream = runtime.call_stream(&hash, Some("main"), json!([]))?;
    let mut seen = Vec::new();
    while let Some(item) = stream.next().await {
        seen.push(item?);
    }
    assert_eq!(seen, vec![json!(0), json!(1), json!(2)]);
    assert_eq!(stream.finish().await?, json!(7));
    // Not started as a stream: the yield reports 2 and the guest goes straight to its result.
    assert_eq!(runtime.call_def(&hash, json!([])).await?, json!(7));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_consumer_stalls_the_producer_and_dropping_the_stream_stops_it() -> Result<()> {
    let store = Store::memory()?;
    let hash = register(
        &store,
        &generator_module(20, 21)?,
        &[("main", 0)],
        &["yield", "kernel"],
    )?;
    let runtime = Runtime::new(store)?;
    let kernel = Arc::new(Cbor7(Default::default()));
    runtime.register_kernel(kernel.clone())?;
    let produced = || kernel.0.load(std::sync::atomic::Ordering::Relaxed);

    // Nobody reads: the channel holds a few values, then the guest waits inside `yield`.
    let mut stream = runtime.call_stream(&hash, Some("main"), json!([]))?;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let stalled = produced();
    assert!(
        (1..=6).contains(&stalled),
        "producer ran {stalled} steps ahead of nobody"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(produced(), stalled, "and it stays stalled");
    // Reading lets it run on to the end.
    let mut count = 0;
    while stream.next().await.is_some() {
        count += 1;
    }
    assert_eq!(count, 20);
    stream.finish().await?;

    // Drop after one value: the producer stops far short of 20.
    let before = produced();
    let mut stream = runtime.call_stream(&hash, Some("main"), json!([]))?;
    assert_eq!(stream.next().await.unwrap()?, json!(0));
    drop(stream);
    tokio::time::sleep(Duration::from_millis(600)).await;
    let ran = produced() - before;
    assert!(
        ran < 15,
        "a dropped stream let the producer run {ran} more steps"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_generator_whose_row_lacks_yield_gets_no_stream() -> Result<()> {
    let store = Store::memory()?;
    let hash = register(
        &store,
        &generator_module(3, 22)?,
        &[("main", 0)],
        &["kernel"],
    )?;
    let runtime = Runtime::new(store)?;
    runtime.register_kernel(Arc::new(Cbor7(Default::default())))?;
    let mut stream = runtime.call_stream(&hash, Some("main"), json!([]))?;
    assert!(stream.next().await.is_none(), "denied: nothing was yielded");
    assert_eq!(stream.finish().await?, json!(7));
    Ok(())
}

/// A callee that calls kernel op `test.cbor7` (like `kernel_module`) and then traps.
fn trapping_module(salt: u8) -> Result<Vec<u8>> {
    let mut artifact = wat::parse_str(format!(
        r#"(module
        (import "env" "memory" (memory 1 1 shared))
        (import "loom" "kernel" (func $kernel (param i32 i32 i32 i32) (result i64)))
        (global (export "__stack_pointer") (mut i32) (i32.const 65536))
        (global (export "__loom_stack_low") (mut i32) (i32.const 32768))
        (global (export "__loom_stack_high") (mut i32) (i32.const 65536))
        (func (export "loom_alloc") (param i32 i32) (result i32) i32.const 8192)
        (func (export "loom_dealloc") (param i32 i32 i32))
        (data (i32.const 1024) "test.cbor7")
        (data (i32.const 1100) "\{salt:02x}")
        (func (export "loom_call_main") (param i32 i32) (result i64)
            i32.const 1024 i32.const 10 i32.const 1104 i32.const 0 call $kernel
            drop
            unreachable)
    )"#
    ))?;
    loom_proto::core_protocol::stamp(&mut artifact);
    Ok(artifact)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_that_recorded_an_effect_is_not_handed_to_the_callers_waiting_on_it() -> Result<()> {
    // The row says empty and the callee performs `now` anyway: the effect is refused and recorded
    // under its own call scope. That run is not clean, so the identical calls that were waiting
    // on it each run their own attempt and each trace holds its own record.
    let store = Store::memory()?;
    let descriptor =
        loom_proto::encode(&json!({"op":"now","args":null})).map_err(anyhow::Error::msg)?;
    let hidden = register(&store, &perform_module(&descriptor)?, &[("main", 0)], &[])?;
    let runtime = Runtime::new(store)?;
    let effects = traced();
    let calls = (0..3).map(|i| call_main(&runtime, &hidden, &[0x80], i, &effects));
    futures::future::join_all(calls).await;
    assert_eq!(
        runtime.isolated_call_round_trip_us()["samples"],
        json!(3),
        "a waiter took the leader's result without running"
    );
    let trace = effects.trace.as_ref().unwrap();
    for occurrence in 0..3 {
        assert!(
            trace.has_effects_under(&format!("root/call:{occurrence}")),
            "call {occurrence} has no record of its own effect"
        );
    }
    assert_eq!(runtime.call_result_stats().stores, 0);
    assert!(runtime.inner.inflight.lock().unwrap().is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clean_failure_is_shared_with_the_callers_already_waiting_and_a_later_call_tries_again()
-> Result<()> {
    let store = Store::memory()?;
    let hash = register(&store, &trapping_module(30)?, &[("main", 0)], &["kernel"])?;
    let runtime = Runtime::new(store)?;
    let kernel = Arc::new(SlowCbor7(Default::default()));
    runtime.register_kernel(kernel.clone())?;
    let effects = traced();
    let ran = || kernel.0.load(std::sync::atomic::Ordering::Relaxed);
    let calls = (0..5).map(|i| call_main(&runtime, &hash, &[0x80], i, &effects));
    let results = futures::future::join_all(calls).await;
    assert!(results.iter().all(|result| result.is_err()), "{results:?}");
    // The waiters were given the callee's trap without the leader's trace scope.
    let waiters: Vec<_> = results[1..]
        .iter()
        .map(|result| result.as_ref().unwrap_err())
        .collect();
    assert!(
        waiters.iter().all(|error| *error == waiters[0]),
        "{waiters:?}"
    );
    assert!(
        matches!(waiters[0], CallError::Trapped { message, .. } if !message.contains("root/call:")),
        "{:?}",
        waiters[0]
    );
    assert_eq!(
        ran(),
        1,
        "five identical failing calls ran the callee once, not one after another"
    );
    assert!(runtime.inner.inflight.lock().unwrap().is_empty());
    // A separate request afterwards is a new flight.
    call_main(&runtime, &hash, &[0x80], 5, &effects)
        .await
        .unwrap_err();
    assert_eq!(ran(), 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_leader_dropped_mid_run_leaves_no_in_flight_entry() -> Result<()> {
    let store = Store::memory()?;
    let hash = register(&store, &kernel_module(31)?, &[("main", 0)], &["kernel"])?;
    let runtime = Runtime::new(store)?;
    runtime.register_kernel(Arc::new(SlowCbor7(Default::default())))?;
    let effects = traced();
    let outcome = tokio::time::timeout(
        Duration::from_millis(100),
        call_main(&runtime, &hash, &[0x80], 0, &effects),
    )
    .await;
    assert!(outcome.is_err(), "the 400 ms callee finished inside 100 ms");
    assert!(
        runtime.inner.inflight.lock().unwrap().is_empty(),
        "the dropped leader's entry is still in the table"
    );
    // The key is free again: an identical call leads its own flight and completes.
    call_main(&runtime, &hash, &[0x80], 1, &effects)
        .await
        .unwrap();
    assert!(runtime.inner.inflight.lock().unwrap().is_empty());
    Ok(())
}

/// The first call to reach it holds until every other call has been through (or 10 s pass), so
/// the batch finishes quickly only if the other calls run while the first is still held.
struct HeldFirst {
    seen: std::sync::atomic::AtomicU64,
    others: u64,
}
impl crate::HostKernel for HeldFirst {
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
        use std::sync::atomic::Ordering::SeqCst;
        if self.seen.fetch_add(1, SeqCst) == 0 {
            let give_up = Instant::now() + Duration::from_secs(10);
            while self.seen.load(SeqCst) <= self.others && Instant::now() < give_up {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        Ok(vec![0x07])
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_slow_call_in_a_batch_does_not_hold_up_the_calls_after_it() -> Result<()> {
    if std::thread::available_parallelism().map_or(1, |n| n.get()) < 2 {
        return Ok(()); // a single kernel slot cannot run two calls at once
    }
    const CALLS: u64 = 40;
    let store = Store::memory()?;
    let hash = register(&store, &kernel_module(32)?, &[("main", 0)], &["kernel"])?;
    let runtime = Runtime::new(store)?;
    let kernel = Arc::new(HeldFirst {
        seen: Default::default(),
        others: CALLS - 1,
    });
    runtime.register_kernel(kernel.clone())?;
    let digest = loom_proto::isolated::parse_digest(&hash).unwrap();
    // Distinct payloads: the callee ignores them, but each is its own call (no single flight).
    let frames: Vec<Vec<u8>> = (0..CALLS)
        .map(|i| {
            Request {
                target: Target::Hash(digest),
                entry: "main",
                argc: 0,
                payload: &[0x80, i as u8],
            }
            .encode()
        })
        .collect();
    let slices: Vec<&[u8]> = frames.iter().map(Vec::as_slice).collect();
    let started = Instant::now();
    let outcomes = runtime.isolated_batch(&slices, "root", 0, &traced()).await;
    let elapsed = started.elapsed();
    assert_eq!(outcomes.len(), CALLS as usize);
    assert!(
        outcomes.iter().all(|outcome| outcome.is_ok()),
        "{outcomes:?}"
    );
    assert!(
        elapsed < Duration::from_secs(8),
        "{CALLS} calls took {elapsed:?}: the calls behind the held one waited for it"
    );
    Ok(())
}

#[test]
fn results_are_charged_to_the_budget_in_request_order_so_the_same_calls_fail_every_time() {
    let frame = Request {
        target: Target::Hash([3; 32]),
        entry: "main",
        argc: 0,
        payload: &[0x80],
    }
    .encode();
    let frames: Vec<&[u8]> = vec![frame.as_slice(); 6];
    let cost = 10 + RESPONSE_FRAME_OVERHEAD;
    let outcomes = || -> Vec<Result<Vec<u8>, CallError>> {
        vec![
            Ok(vec![0; 10]),
            Err(CallError::Decode {
                message: "x".into(),
            }),
            Ok(vec![0; 10]),
            Ok(vec![0; 10]),
            Ok(vec![0; 1]),
            Ok(vec![0; 10]),
        ]
    };
    // Room for two results: the third Ok and everything after it is dropped, small ones too, and
    // the failed call keeps its own error and costs nothing.
    let mut charged = outcomes();
    charge_in_order(&frames, &mut charged, 2 * cost);
    assert!(charged[0].is_ok() && charged[2].is_ok());
    assert!(matches!(&charged[1], Err(CallError::Decode { .. })));
    for index in [3, 4, 5] {
        assert!(
            matches!(&charged[index], Err(CallError::Trapped { message, .. }) if message.contains("dropped")),
            "{index}: {:?}",
            charged[index]
        );
    }
    // The same results give the same outcome.
    let mut again = outcomes();
    charge_in_order(&frames, &mut again, 2 * cost);
    assert_eq!(charged, again);
    // With room for all, nothing changes.
    let mut roomy = outcomes();
    charge_in_order(&frames, &mut roomy, 100 * cost);
    assert_eq!(roomy, outcomes());
}

#[tokio::test]
async fn a_batch_that_holds_too_many_result_bytes_does_not_start_more_calls() -> Result<()> {
    let store = Store::memory()?;
    let pure = register(&store, &constant_module(7)?, &[("main", 0)], &[])?;
    let runtime = Runtime::new(store)?;
    let effects = traced();
    let frame = Request {
        target: Target::Hash(loom_proto::isolated::parse_digest(&pure).unwrap()),
        entry: "main",
        argc: 0,
        payload: &[0x80],
    }
    .encode();
    let held = AtomicUsize::new(BATCH_HOLD_BYTES);
    let refused = runtime
        .batch_element(&frame, "root", 0, &effects, &held)
        .await;
    assert!(
        matches!(&refused, Err(CallError::Trapped { message, .. }) if message.contains("not run")),
        "{refused:?}"
    );
    assert_eq!(runtime.isolated_call_round_trip_us()["samples"], json!(0));
    let held = AtomicUsize::new(0);
    let bytes = runtime
        .batch_element(&frame, "root", 1, &effects, &held)
        .await
        .unwrap();
    assert_eq!(held.load(std::sync::atomic::Ordering::Relaxed), bytes.len());
    Ok(())
}

/// A kernel that records a depth refusal on its runtime while the callee runs, as a call below
/// the callee would have when it reached the depth limit.
struct RefusesDepth(Mutex<Option<Runtime>>);
impl crate::HostKernel for RefusesDepth {
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
        if let Some(runtime) = self.0.lock().unwrap().as_ref() {
            runtime
                .inner
                .depth_refusals
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(vec![0x07])
    }
}

#[tokio::test]
async fn a_run_during_which_a_call_was_refused_for_depth_is_not_clean_and_is_not_stored()
-> Result<()> {
    let store = Store::memory()?;
    let hash = register(&store, &kernel_module(33)?, &[("main", 0)], &["kernel"])?;
    let runtime = Runtime::new(store)?;
    let kernel = Arc::new(RefusesDepth(Mutex::new(None)));
    runtime.register_kernel(kernel.clone())?;
    let effects = traced();
    let kernels = runtime.kernel_fingerprint();
    let request = Request {
        target: Target::Hash(loom_proto::isolated::parse_digest(&hash).unwrap()),
        entry: "main",
        argc: 0,
        payload: &[0x80],
    };
    // The control: nothing was refused, so the run is clean and stored.
    let ran = runtime
        .run_isolated(&hash, &request, "root", 0, &effects, true, &kernels)
        .await;
    assert!(ran.result.is_ok() && ran.clean);
    assert_eq!(runtime.call_result_stats().stores, 1);
    runtime.clear_call_results(None);
    // With a refusal inside the window the same run is neither clean nor stored.
    *kernel.0.lock().unwrap() = Some(runtime.clone());
    let ran = runtime
        .run_isolated(&hash, &request, "root", 1, &effects, true, &kernels)
        .await;
    assert!(ran.result.is_ok());
    assert!(!ran.clean, "a depth refusal during the run went unnoticed");
    assert_eq!(runtime.call_result_stats().stores, 1, "no new entry");
    // And the refusal itself is counted where it happens.
    let before = runtime
        .inner
        .depth_refusals
        .load(std::sync::atomic::Ordering::Relaxed);
    let deep = EffectContext {
        depth: MAX_DEPTH,
        ..traced()
    };
    call_main(&runtime, &hash, &[0x80], 2, &deep)
        .await
        .unwrap_err();
    assert_eq!(
        runtime
            .inner
            .depth_refusals
            .load(std::sync::atomic::Ordering::Relaxed),
        before + 1
    );
    *kernel.0.lock().unwrap() = None;
    Ok(())
}

#[test]
fn only_a_callees_own_deterministic_failures_are_shared_with_waiters() {
    let hash = "ab".repeat(32);
    let host =
        |message: &str| anyhow::anyhow!("{message}").context("shared execution call:x/call:0");
    for error in [
        host("shared execution deadline exceeded"),
        host("shared execution cancelled"),
        host("artifact missing"),
        anyhow::Error::new(GuestFailure::new("wasm trap: interrupt")),
        anyhow::Error::new(CallError::DepthExceeded { depth: 64 }),
        anyhow::Error::new(CallError::Trapped {
            hash: hash.clone(),
            message: "from a nested call, with its scope call:y/call:1".into(),
        }),
    ] {
        assert_eq!(shareable_failure(&hash, &error), None, "{error:#}");
    }
    let trap = anyhow::Error::new(GuestFailure::new("wasm trap: unreachable"))
        .context("shared execution call:x/call:0");
    assert_eq!(
        shareable_failure(&hash, &trap),
        Some(CallError::Trapped {
            hash: hash.clone(),
            message: "wasm trap: unreachable".into(),
        }),
        "the host's context (the leader's scope) is not shared"
    );
    let decode = anyhow::Error::new(CallError::Decode {
        message: "bad".into(),
    });
    assert!(shareable_failure(&hash, &decode).is_some());
    let missing = anyhow::Error::new(DefinitionNotFound { hash: hash.clone() });
    assert_eq!(
        shareable_failure(&hash, &missing),
        Some(CallError::NotFound { hash })
    );
}

/// `main` yields `length` bytes from address 1200 and returns the yield code as its CBOR value.
/// The host refuses an oversized item on its length alone, so the memory behind it does not matter.
fn yield_code_module(length: u32) -> Result<Vec<u8>> {
    let mut artifact = wat::parse_str(format!(
        r#"(module
        (import "env" "memory" (memory 1 1 shared))
        (import "loom" "yield_value" (func $yield (param i32 i32) (result i32)))
        (global (export "__stack_pointer") (mut i32) (i32.const 65536))
        (global (export "__loom_stack_low") (mut i32) (i32.const 32768))
        (global (export "__loom_stack_high") (mut i32) (i32.const 65536))
        (func (export "loom_alloc") (param i32 i32) (result i32) i32.const 8192)
        (func (export "loom_dealloc") (param i32 i32 i32))
        (data (i32.const 1024) "\00\00")
        (func (export "loom_call_main") (param i32 i32) (result i64)
            i32.const 1025
            i32.const 1200 i32.const {length} call $yield
            i32.store8
            i64.const 2 i64.const 32 i64.shl i64.const 1024 i64.or)
    )"#
    ))?;
    loom_proto::core_protocol::stamp(&mut artifact);
    Ok(artifact)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_item_over_the_limit_is_refused_with_code_four_and_a_small_one_is_accepted() -> Result<()>
{
    let store = Store::memory()?;
    let too_large = register(
        &store,
        &yield_code_module(crate::stream::MAX_ITEM_BYTES as u32 + 1)?,
        &[("main", 0)],
        &["yield"],
    )?;
    let small = register(&store, &yield_code_module(1)?, &[("main", 0)], &["yield"])?;
    let runtime = Runtime::new(store)?;
    let mut stream = runtime.call_stream(&too_large, Some("main"), json!([]))?;
    assert!(stream.next().await.is_none(), "nothing was queued");
    assert_eq!(stream.finish().await?, json!(4));
    let stream = runtime.call_stream(&small, Some("main"), json!([]))?;
    assert_eq!(
        stream.finish().await?,
        json!(0),
        "accepted, and finish read the item"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finish_lets_the_entry_run_on_and_a_dropped_finish_stops_it() -> Result<()> {
    // Unread values are discarded and the entry runs to its real result: it was not cancelled.
    let store = Store::memory()?;
    let hash = register(
        &store,
        &generator_module(6, 23)?,
        &[("main", 0)],
        &["yield", "kernel"],
    )?;
    let runtime = Runtime::new(store)?;
    let kernel = Arc::new(Cbor7(Default::default()));
    runtime.register_kernel(kernel.clone())?;
    let mut stream = runtime.call_stream(&hash, Some("main"), json!([]))?;
    assert_eq!(stream.next().await.unwrap()?, json!(0));
    assert_eq!(stream.finish().await?, json!(7));
    assert_eq!(
        kernel.0.load(std::sync::atomic::Ordering::Relaxed),
        7,
        "six steps after six accepted yields, then the final call"
    );

    // Dropping the `finish` future drops the stream, which aborts the entry.
    let store = Store::memory()?;
    let hash = register(
        &store,
        &generator_module(20, 24)?,
        &[("main", 0)],
        &["yield", "kernel"],
    )?;
    let runtime = Runtime::new(store)?;
    let kernel = Arc::new(SlowCbor7(Default::default()));
    runtime.register_kernel(kernel.clone())?;
    let stream = runtime.call_stream(&hash, Some("main"), json!([]))?;
    let outcome = tokio::time::timeout(Duration::from_millis(150), stream.finish()).await;
    assert!(
        outcome.is_err(),
        "twenty 400 ms steps finished inside 150 ms"
    );
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let stopped = kernel.0.load(std::sync::atomic::Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(
        kernel.0.load(std::sync::atomic::Ordering::Relaxed),
        stopped,
        "the entry kept running"
    );
    assert!(stopped <= 3, "{stopped} steps ran");
    Ok(())
}

#[test]
fn call_stream_outside_a_tokio_runtime_is_an_error_not_a_panic() -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    let runtime = rt.block_on(async { Runtime::new(Store::memory()?) })?;
    let error = runtime
        .call_stream(&"00".repeat(32), None, json!([]))
        .err()
        .context("call_stream outside a runtime must fail")?;
    assert!(format!("{error:#}").contains("tokio runtime"), "{error:#}");
    Ok(())
}
