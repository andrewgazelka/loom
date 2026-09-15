use loom_sandbox::{CallEffects, Sandbox};
use loom_v8::{Limits, V8Engine, V8Sandbox};
use serde_json::{Value, json};
use std::{future::Future, pin::Pin, time::Duration};

#[derive(Default)]
struct EchoEffects {
    descriptors: Vec<Value>,
}

impl CallEffects for EchoEffects {
    fn perform(
        &mut self,
        descriptor: Value,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send + '_>> {
        self.descriptors.push(descriptor.clone());
        Box::pin(async move { Ok(descriptor) })
    }
}

fn engine() -> V8Engine {
    V8Engine::new(Limits::default()).unwrap()
}

#[tokio::test]
async fn native_smoke_spreads_arguments_and_reads_schema() {
    let sandbox = engine()
        .compile("const LOOM_SCHEMA = 'CREATE TABLE counter(value INTEGER);'; function main(a, b) { return {sum: a + b}; }")
        .await
        .unwrap();
    assert_eq!(sandbox.schema(), "CREATE TABLE counter(value INTEGER);");
    let mut effects = EchoEffects::default();
    assert_eq!(
        sandbox.call(json!([20, 22]), &mut effects).await.unwrap(),
        json!({"sum":42})
    );
    assert!(effects.descriptors.is_empty());
}

#[tokio::test]
async fn omitted_schema_is_empty_and_non_array_arguments_fail() {
    let sandbox = engine()
        .compile("function main() { return 42; }")
        .await
        .unwrap();
    assert_eq!(sandbox.schema(), "");
    assert!(
        sandbox
            .call(json!({"value":42}), &mut EchoEffects::default())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn concurrent_guest_effects_reach_the_borrowed_host() {
    let sandbox = engine()
        .compile(
            r#"
        async function main() {
            return await Promise.all([
                loom.perform('example.send', {target: 'first', message: 1}),
                loom.perform('example.send', {target: 'second', message: 2})
            ]);
        }
    "#,
        )
        .await
        .unwrap();
    let mut effects = EchoEffects::default();
    let output = sandbox.call(json!([]), &mut effects).await.unwrap();
    let expected = json!([
        {"op":"example.send", "args":{"target":"first", "message":1}},
        {"op":"example.send", "args":{"target":"second", "message":2}}
    ]);
    assert_eq!(output, expected);
    assert_eq!(Value::Array(effects.descriptors), expected);
}

struct FailingEffects;

impl CallEffects for FailingEffects {
    fn perform(
        &mut self,
        _: Value,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send + '_>> {
        Box::pin(async { Err(std::io::Error::other("durable storage unavailable").into()) })
    }
}

#[tokio::test]
async fn guest_cannot_swallow_typed_host_failure() {
    let sandbox = engine()
        .compile(
            r#"
        async function main() {
            try { await loom.perform('sql', {}); }
            catch (_) { return 'incorrectly committed'; }
            return 'also incorrect';
        }
    "#,
        )
        .await
        .unwrap();
    let error = sandbox
        .call(json!([]), &mut FailingEffects)
        .await
        .unwrap_err();
    assert!(error.is::<std::io::Error>(), "{error:#}");
    assert!(error.to_string().contains("durable storage unavailable"));
}

#[tokio::test]
async fn each_call_has_fresh_globals_and_intrinsics() {
    let sandbox = engine()
        .compile(
            r#"
        let count = 0;
        function main() {
            const prior = Object.prototype.polluted === true;
            Object.prototype.polluted = true;
            return {count: ++count, prior};
        }
    "#,
        )
        .await
        .unwrap();
    for _ in 0..3 {
        assert_eq!(
            sandbox
                .call(json!([]), &mut EchoEffects::default())
                .await
                .unwrap(),
            json!({"count":1,"prior":false})
        );
    }
}

#[tokio::test]
async fn compilation_rejects_invalid_contracts() {
    let engine = engine();
    for source in [
        "function main( {",
        "const other = () => 1;",
        "const main = 42;",
        "const LOOM_SCHEMA = 42; function main() { return 1; }",
        "return {main:42,schema:\"\"};",
    ] {
        assert!(engine.compile(source).await.is_err(), "accepted: {source}");
    }
}

#[tokio::test]
async fn cpu_loops_terminate_during_compile_and_call() {
    let engine = V8Engine::new(Limits {
        timeout: Duration::from_millis(100),
        workers: 1,
        ..Limits::default()
    })
    .unwrap();
    assert!(
        engine
            .compile("while (true) {} function main() {}")
            .await
            .is_err()
    );
    let sandbox = engine
        .compile("function main() { while (true) {} }")
        .await
        .unwrap();
    assert!(
        sandbox
            .call(json!([]), &mut EchoEffects::default())
            .await
            .is_err()
    );
    // A successful job after both terminations proves the worker remains usable.
    let healthy = engine
        .compile("function main() { return 42; }")
        .await
        .unwrap();
    assert_eq!(
        healthy
            .call(json!([]), &mut EchoEffects::default())
            .await
            .unwrap(),
        json!(42)
    );
}

#[tokio::test]
async fn unresolved_promise_fails_instead_of_occupying_worker() {
    let sandbox = engine()
        .compile("function main() { return new Promise(() => {}); }")
        .await
        .unwrap();
    let mut effects = EchoEffects::default();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        sandbox.call(json!([]), &mut effects),
    )
    .await;
    assert!(
        result
            .expect("unresolvable promise should fail promptly")
            .is_err()
    );
}

struct PendingEffects {
    started: Option<tokio::sync::oneshot::Sender<()>>,
}

impl CallEffects for PendingEffects {
    fn perform(
        &mut self,
        _: Value,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send + '_>> {
        if let Some(started) = self.started.take() {
            let _ = started.send(());
        }
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn dropping_call_releases_worker_while_host_effect_is_pending() {
    let engine = V8Engine::new(Limits {
        timeout: Duration::from_secs(30),
        workers: 1,
        ..Limits::default()
    })
    .unwrap();
    let sandbox = engine
        .compile("async function main() { return await loom.perform('wait', {}); }")
        .await
        .unwrap();
    let (started, receiver) = tokio::sync::oneshot::channel();
    let mut effects = PendingEffects {
        started: Some(started),
    };
    {
        let call = sandbox.call(json!([]), &mut effects);
        tokio::pin!(call);
        tokio::select! {
            result = &mut call => panic!("call ended before cancellation: {result:?}"),
            result = receiver => result.unwrap(),
            _ = tokio::time::sleep(Duration::from_secs(2)) => panic!("host effect never started"),
        }
    }
    // The original 30-second deadline cannot explain completion within 2 seconds.
    let healthy = tokio::time::timeout(
        Duration::from_secs(2),
        engine.compile("function main() { return 42; }"),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        healthy
            .call(json!([]), &mut EchoEffects::default())
            .await
            .unwrap(),
        json!(42)
    );
}

#[tokio::test]
async fn ambient_host_and_external_allocation_apis_are_absent() {
    let sandbox = engine()
        .compile(
            r#"
        function main() {
            return {
                denied: ['fetch','process','require','Deno','WebAssembly','ArrayBuffer',
                    'SharedArrayBuffer','DataView','Uint8Array','Uint8ClampedArray','Int8Array','Uint16Array',
                    'Int16Array','Uint32Array','Int32Array','Float32Array','Float64Array',
                    'BigInt64Array','BigUint64Array','Float16Array','Atomics','Date','Intl',
                    'Temporal','WeakRef','FinalizationRegistry',
                    '__loomPerform','setTimeout','setInterval']
                    .filter(name => typeof globalThis[name] !== 'undefined'),
                frozen: Object.isFrozen(loom),
                frozenPerform: Object.isFrozen(loom.perform),
                random: typeof Math.random,
            };
        }
    "#,
        )
        .await
        .unwrap();
    assert_eq!(
        sandbox
            .call(json!([]), &mut EchoEffects::default())
            .await
            .unwrap(),
        json!({"denied":[],"frozen":true,"frozenPerform":true,"random":"undefined"})
    );
}

#[derive(Default)]
struct ConvenienceEffects {
    descriptors: Vec<Value>,
}

impl CallEffects for ConvenienceEffects {
    fn perform(
        &mut self,
        descriptor: Value,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send + '_>> {
        self.descriptors.push(descriptor.clone());
        Box::pin(async move {
            if descriptor["op"] == "sql" {
                Ok(json!({"columns":[],"rows":[]}))
            } else {
                Ok(descriptor)
            }
        })
    }
}

#[tokio::test]
async fn convenience_methods_use_the_same_effect_bridge() {
    let sandbox = engine()
        .compile(
            r#"
        async function main() {
            return [await loom.now(), await loom.random(8),
                await loom.sql('SELECT ?', [42]), await loom.sql('SELECT 1')];
        }
    "#,
        )
        .await
        .unwrap();
    let mut effects = ConvenienceEffects::default();
    assert_eq!(
        sandbox.call(json!([]), &mut effects).await.unwrap(),
        json!([
            {"op":"now","args":null},
            {"op":"random","args":{"n":8}},
            [],
            []
        ])
    );
    assert_eq!(
        Value::Array(effects.descriptors),
        json!([
            {"op":"now","args":null},
            {"op":"random","args":{"n":8}},
            {"op":"sql","args":{"sql":"SELECT ?","params":[{"type":"integer","value":42}]}},
            {"op":"sql","args":{"sql":"SELECT 1","params":[]}}
        ])
    );
}

#[tokio::test]
async fn serialization_effects_and_their_microtasks_finish_before_return() {
    let sandbox = engine()
        .compile(
            r#"
        function main() {
            return {
                toJSON() {
                    loom.perform('first', {}).then(() => loom.perform('second', {}));
                    return 42;
                }
            };
        }
    "#,
        )
        .await
        .unwrap();
    let mut effects = EchoEffects::default();
    assert_eq!(
        sandbox.call(json!([]), &mut effects).await.unwrap(),
        json!(42)
    );
    assert_eq!(
        Value::Array(effects.descriptors),
        json!([
            {"op":"first","args":{}},
            {"op":"second","args":{}}
        ])
    );
}

#[tokio::test]
async fn source_can_declare_its_own_schema_variable() {
    let sandbox = engine()
        .compile("const schema = 42; function main() { return schema; }")
        .await
        .unwrap();
    assert_eq!(sandbox.schema(), "");
    assert_eq!(
        sandbox
            .call(json!([]), &mut EchoEffects::default())
            .await
            .unwrap(),
        json!(42)
    );
}

#[tokio::test]
async fn functions_without_return_values_complete_with_null() {
    let engine = engine();
    let plain = engine.compile("function main() {}").await.unwrap();
    let mut effects = EchoEffects::default();
    assert_eq!(
        plain.call(json!([]), &mut effects).await.unwrap(),
        Value::Null
    );
    assert!(effects.descriptors.is_empty());

    let asynchronous = engine
        .compile("async function main() { await loom.perform('effect', {}); }")
        .await
        .unwrap();
    assert_eq!(
        asynchronous.call(json!([]), &mut effects).await.unwrap(),
        Value::Null
    );
    assert_eq!(effects.descriptors, vec![json!({"op":"effect","args":{}})]);
}

struct UnsafeIntegerReply;

impl CallEffects for UnsafeIntegerReply {
    fn perform(
        &mut self,
        _: Value,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send + '_>> {
        Box::pin(async { Ok(json!({"nested":[9_007_199_254_740_993_u64]})) })
    }
}

#[tokio::test]
async fn host_integers_outside_javascript_exact_range_are_rejected() {
    let engine = engine();
    let sandbox = engine
        .compile("function main(value) { return value; }")
        .await
        .unwrap();
    let mut effects = EchoEffects::default();
    assert!(
        sandbox
            .call(
                json!([{ "nested": [9_007_199_254_740_993_u64] }]),
                &mut effects
            )
            .await
            .is_err()
    );
    assert!(effects.descriptors.is_empty());
    assert_eq!(
        sandbox
            .call(json!([9_007_199_254_740_991_u64]), &mut effects)
            .await
            .unwrap(),
        json!(9_007_199_254_740_991_u64)
    );

    let reply = engine
        .compile("async function main() { await loom.perform('read', {}); return 42; }")
        .await
        .unwrap();
    assert!(
        reply
            .call(json!([]), &mut UnsafeIntegerReply)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn invalid_effect_descriptors_never_reach_the_host() {
    let engine = engine();
    for source in [
        "async function main() { return await loom.perform(42, {}); }",
        "async function main() { return await loom.perform('missing'); }",
    ] {
        let sandbox = engine.compile(source).await.unwrap();
        let mut effects = EchoEffects::default();
        assert!(sandbox.call(json!([]), &mut effects).await.is_err());
        assert!(effects.descriptors.is_empty());
    }
}

#[tokio::test]
async fn subsequent_isolates_consume_the_compiled_code_cache() {
    let engine = engine();
    let sandbox = engine
        .compile("function main(a) { return a + 1; }")
        .await
        .unwrap();
    let compiled = engine.cache_stats();
    assert_eq!(compiled.compilations, 1);
    for value in [1, 2, 3] {
        assert_eq!(
            sandbox
                .call(json!([value]), &mut EchoEffects::default())
                .await
                .unwrap(),
            json!(value + 1)
        );
    }
    let called = engine.cache_stats();
    assert_eq!(called.compilations, compiled.compilations);
    assert_eq!(called.cache_hits - compiled.cache_hits, 3);
}

#[tokio::test]
async fn effect_api_cannot_be_replaced_or_recovered_through_global_function() {
    let sandbox = engine()
        .compile(
            r#"
        function main() {
            const original = loom;
            Reflect.set(globalThis, 'loom', {});
            const deleted = Reflect.deleteProperty(globalThis, 'loom');
            return {
                unchanged: loom === original,
                deleted,
                hiddenBridge: Function('return typeof __loomPerform')(),
                hiddenBuffer: Function('return typeof ArrayBuffer')(),
            };
        }
    "#,
        )
        .await
        .unwrap();
    assert_eq!(
        sandbox
            .call(json!([]), &mut EchoEffects::default())
            .await
            .unwrap(),
        json!({
            "unchanged":true,"deleted":false,"hiddenBridge":"undefined","hiddenBuffer":"undefined"
        })
    );
}

#[tokio::test]
async fn source_input_output_and_effect_messages_are_bounded() {
    let engine = V8Engine::new(Limits {
        max_source_bytes: 1024,
        max_message_bytes: 128,
        ..Limits::default()
    })
    .unwrap();
    assert!(engine.compile(&" ".repeat(1025)).await.is_err());
    let echo = engine
        .compile("function main(value) { return value; }")
        .await
        .unwrap();
    assert!(
        echo.call(json!(["x".repeat(256)]), &mut EchoEffects::default())
            .await
            .is_err()
    );
    let output = engine
        .compile("function main() { return 'x'.repeat(256); }")
        .await
        .unwrap();
    assert!(
        output
            .call(json!([]), &mut EchoEffects::default())
            .await
            .is_err()
    );
    let effect = engine
        .compile("async function main() { return await loom.perform('large', 'x'.repeat(256)); }")
        .await
        .unwrap();
    let mut effects = EchoEffects::default();
    assert!(effect.call(json!([]), &mut effects).await.is_err());
    assert!(effects.descriptors.is_empty());
}

struct OversizedReply;

impl CallEffects for OversizedReply {
    fn perform(
        &mut self,
        _: Value,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send + '_>> {
        Box::pin(async { Ok(json!("x".repeat(256))) })
    }
}

#[tokio::test]
async fn oversized_host_reply_cannot_enter_guest() {
    let engine = V8Engine::new(Limits {
        max_message_bytes: 128,
        ..Limits::default()
    })
    .unwrap();
    let sandbox = engine
        .compile("async function main() { await loom.perform('read', {}); return 42; }")
        .await
        .unwrap();
    assert!(sandbox.call(json!([]), &mut OversizedReply).await.is_err());
}

#[tokio::test]
async fn pending_effect_limit_rejects_a_burst() {
    let engine = V8Engine::new(Limits {
        max_pending_effects: 1,
        ..Limits::default()
    })
    .unwrap();
    let sandbox = engine
        .compile(
            r#"
            async function main() {
                return await Promise.all([
                    loom.perform('one', {}),
                    loom.perform('two', {}),
                    loom.perform('three', {})
                ]);
            }
        "#,
        )
        .await
        .unwrap();
    assert!(
        sandbox
            .call(json!([]), &mut EchoEffects::default())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn heap_exhaustion_returns_an_error_and_worker_recovers() {
    let engine = V8Engine::new(Limits {
        heap_bytes: 8 * 1024 * 1024,
        workers: 1,
        ..Limits::default()
    })
    .unwrap();
    let sandbox = engine
        .compile(
            r#"
        function main() {
            const retained = [];
            while (true) {
                retained.push({payload: new Array(128).fill(retained.length)});
            }
        }
    "#,
        )
        .await
        .unwrap();
    let error = sandbox
        .call(json!([]), &mut EchoEffects::default())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("heap"), "{error:#}");
    let healthy = engine
        .compile("function main() { return 42; }")
        .await
        .unwrap();
    assert_eq!(
        healthy
            .call(json!([]), &mut EchoEffects::default())
            .await
            .unwrap(),
        json!(42)
    );
}

#[tokio::test]
async fn deadline_interrupts_microtasks_and_output_serialization() {
    let engine = V8Engine::new(Limits {
        timeout: Duration::from_millis(100),
        workers: 1,
        ..Limits::default()
    })
    .unwrap();
    for source in [
        "function main() { const spin = () => Promise.resolve().then(spin); return spin(); }",
        "function main() { return {toJSON() { while (true) {} }}; }",
    ] {
        let sandbox = engine.compile(source).await.unwrap();
        let mut effects = EchoEffects::default();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            sandbox.call(json!([]), &mut effects),
        )
        .await
        .expect("deadline did not release the call");
        assert!(result.is_err(), "accepted: {source}");
    }
    let healthy = engine
        .compile("function main() { return 42; }")
        .await
        .unwrap();
    assert_eq!(
        healthy
            .call(json!([]), &mut EchoEffects::default())
            .await
            .unwrap(),
        json!(42)
    );
}

struct RecursiveEffects {
    sandbox: V8Sandbox,
    calls: usize,
}

impl CallEffects for RecursiveEffects {
    fn perform(
        &mut self,
        descriptor: Value,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send + '_>> {
        Box::pin(async move {
            self.calls += 1;
            let depth = descriptor["args"]["n"].as_u64().expect("recursive depth");
            let sandbox = self.sandbox.clone();
            sandbox.call(json!([depth - 1]), self).await
        })
    }
}

const RECURSIVE_SOURCE: &str = r#"
    async function main(n) {
        if (n === 0) return 42;
        return await loom.perform('recurse', {n});
    }
"#;

#[tokio::test]
async fn nested_host_calls_progress_with_one_worker() {
    let engine = V8Engine::new(Limits {
        workers: 1,
        max_reentrant_depth: 16,
        ..Limits::default()
    })
    .unwrap();
    let sandbox = engine.compile(RECURSIVE_SOURCE).await.unwrap();
    let mut effects = RecursiveEffects {
        sandbox: sandbox.clone(),
        calls: 0,
    };
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        sandbox.call(json!([6]), &mut effects),
    )
    .await
    .expect("nested calls deadlocked the sole worker")
    .unwrap();
    assert_eq!(result, json!(42));
    assert_eq!(effects.calls, 6);
}

#[tokio::test]
async fn nested_host_calls_fail_at_the_reentrancy_limit() {
    let engine = V8Engine::new(Limits {
        workers: 1,
        max_reentrant_depth: 2,
        ..Limits::default()
    })
    .unwrap();
    let sandbox = engine.compile(RECURSIVE_SOURCE).await.unwrap();
    let mut effects = RecursiveEffects {
        sandbox: sandbox.clone(),
        calls: 0,
    };
    let error = tokio::time::timeout(
        Duration::from_secs(2),
        sandbox.call(json!([6]), &mut effects),
    )
    .await
    .expect("depth limit did not release the call")
    .unwrap_err();
    assert!(error.is::<loom_sandbox::GuestFailure>(), "{error:#}");
    assert!(error.to_string().contains("depth"), "{error:#}");
    assert!(effects.calls < 6);
    assert_eq!(
        sandbox.call(json!([0]), &mut effects).await.unwrap(),
        json!(42)
    );
}
