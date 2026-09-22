//! Production Runtime admission, effect dispatch, and replay across both engines.
use anyhow::Result;
use loom_proto::{Def, Lang};
use loom_rt::{Runtime, WasmSandbox};
use loom_sandbox::{CallEffects, Sandbox};
use loom_store::Store;
use serde_json::{Value, json};
use std::{collections::BTreeMap, future::Future, pin::Pin};

/// `params` is the arity of `main`; the host checks every call's declared
/// argument count against it before instantiating anything.
fn publish(
    store: &Store,
    lang: Lang,
    source: &str,
    artifact: &[u8],
    labels: &[&str],
    params: usize,
) -> Result<String> {
    let artifact_hash = store.put(
        if lang == Lang::Rust {
            "component"
        } else {
            "javascript_source"
        },
        artifact,
    )?;
    let hash = if lang == Lang::JavaScript {
        blake3::hash(&loom_proto::javascript_definition_identity(
            source,
            &BTreeMap::new(),
            None,
            loom_v8::ABI_VERSION,
        )?)
        .to_hex()
        .to_string()
    } else {
        blake3::hash(artifact).to_hex().to_string()
    };
    let effects = json!({"labels":labels,"unknown":lang == Lang::JavaScript});
    let params = (0..params)
        .map(|index| json!({"name":format!("argument_{index}"),"shape":{"type":"value"}}))
        .collect::<Vec<_>>();
    let definition = Def {
        hash: hash.clone(),
        lang,
        component_hash: Some(artifact_hash),
        sig: serde_json::from_value(
            json!({"exports":[{"name":"main","params":params,"returns":{"type":"value"},"effects":effects}],"effects":effects}),
        )?,
        allowed_effects: None,
        observed_effects: vec![],
    };
    store.define(&definition, None, source, &BTreeMap::new())?;
    Ok(hash)
}
fn javascript(store: &Store, source: &str, params: usize) -> Result<String> {
    publish(
        store,
        Lang::JavaScript,
        source,
        source.as_bytes(),
        &[],
        params,
    )
}
/// A core module whose `main` hands the embedded bytes to the named `loom`
/// import and returns whatever `body` builds from the packed reply in
/// `$packed`. This exercises the production ABI without depending on the
/// separate Rust guest compilation toolchain.
fn core_module(import: &str, data: &[u8], body: &str) -> Result<Vec<u8>> {
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
/// `main` performs one root effect and returns its value. The perform reply
/// is the `{ok: value}` envelope (`a1 62 6f 6b` + value); the entry frame is
/// `[0]` + value, so the fixture drops four bytes and prepends the ok tag.
fn wasm_effect(store: &Store, descriptor: Value, labels: &[&str]) -> Result<String> {
    let bytes = loom_proto::encode(&descriptor).map_err(anyhow::Error::msg)?;
    let artifact = core_module(
        "perform",
        &bytes,
        r#"local.get $packed i32.wrap_i64 local.set $src
            local.get $packed i64.const 32 i64.shr_u i32.wrap_i64 local.set $len
            local.get $len i32.const 3 i32.sub i32.const 1 call $alloc local.set $dst
            local.get $dst i32.const 0 i32.store8
            local.get $dst i32.const 1 i32.add
            local.get $src i32.const 4 i32.add
            local.get $len i32.const 4 i32.sub
            memory.copy
            local.get $len i32.const 3 i32.sub i64.extend_i32_u i64.const 32 i64.shl
            local.get $dst i64.extend_i32_u i64.or"#,
    )?;
    publish(
        store,
        Lang::Rust,
        "fixture: one production root effect",
        &artifact,
        labels,
        0,
    )
}
/// `main` issues one isolated call through `loom.call` and returns the host's
/// response frame as its own result frame; the two frames share one layout.
fn wasm_call(store: &Store, request: loom_proto::isolated::Request<'_>) -> Result<String> {
    let artifact = core_module("call", &request.encode(), "local.get $packed")?;
    publish(
        store,
        Lang::Rust,
        "fixture: one isolated call",
        &artifact,
        &["call"],
        0,
    )
}

struct Probe {
    descriptors: Vec<Value>,
}
impl CallEffects for Probe {
    fn perform(
        &mut self,
        descriptor: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + '_>> {
        Box::pin(async move {
            self.descriptors.push(descriptor.clone());
            anyhow::ensure!(
                descriptor == json!({"op":"probe","args":{"number":21}}),
                "unexpected effect"
            );
            Ok(json!({"number":42,"bytes":[0,128,255]}))
        })
    }
}

#[tokio::test]
async fn wasm_and_v8_share_borrowed_sandbox_effect_contract() -> Result<()> {
    let store = Store::memory()?;
    let hash = wasm_effect(
        &store,
        json!({"op":"probe","args":{"number":21}}),
        &["probe"],
    )?;
    let runtime = Runtime::new(store)?;
    let wasm = WasmSandbox::new(runtime.clone(), hash);
    let v8 = runtime
        .v8_engine()?
        .compile("async function main() { return await loom.perform('probe', {number:21}); }")
        .await?;
    let mut probe = Probe {
        descriptors: vec![],
    };
    for sandbox in [&wasm as &dyn Sandbox, &v8 as &dyn Sandbox] {
        assert_eq!(
            sandbox.call(json!([]), &mut probe).await?,
            json!({"number":42,"bytes":[0,128,255]})
        );
    }
    assert_eq!(probe.descriptors.len(), 2);
    Ok(())
}

#[tokio::test]
async fn javascript_runtime_effect_trace_replays_after_restart() -> Result<()> {
    let store = Store::memory()?;
    let hash = javascript(
        &store,
        "async function main() { return await loom.perform('random', {}); }",
        0,
    )?;
    let first = Runtime::new(store.clone())?
        .call_def_timed(&hash, json!([]))
        .await?;
    let trace = store
        .load_call_trace(&first.scope)?
        .expect("recorded trace");
    assert_eq!(trace.trace.entries.len(), 1);
    let replay = Runtime::new(store)?
        .replay_def_timed(&hash, json!([]), &first.scope)
        .await?;
    assert_eq!(replay.value, first.value);
    Ok(())
}

/// JavaScript reaches a definition through the `{"op":"call"}` descriptor
/// (the `Value` adapter); the core guest reaches the JavaScript leaf through
/// the `loom.call` frame with a typed payload the host never decodes. Both
/// meet in `Runtime::isolated_call`, and the trace replays the whole chain.
#[tokio::test]
async fn javascript_calls_wasm_calls_javascript() -> Result<()> {
    let store = Store::memory()?;
    let leaf = javascript(&store, "function main(value) { return value * 2; }", 1)?;
    let payload = loom_proto::isolated::encode_payload(&(21u8,)).map_err(anyhow::Error::msg)?;
    let wasm = wasm_call(
        &store,
        loom_proto::isolated::Request {
            target: loom_proto::isolated::Target::from_hex(&leaf).map_err(anyhow::Error::msg)?,
            entry: "",
            argc: 1,
            payload: &payload,
        },
    )?;
    let outer = javascript(
        &store,
        &format!(
            "async function main() {{ return 1 + await loom.perform('call', {{def:'{wasm}', args:[]}}); }}"
        ),
        0,
    )?;
    let runtime = Runtime::new(store)?;
    let first = runtime.call_def_timed(&outer, json!([])).await?;
    assert_eq!(first.value, json!(43));
    assert_eq!(
        runtime
            .replay_def_timed(&outer, json!([]), &first.scope)
            .await?
            .value,
        json!(43)
    );
    Ok(())
}

/// The core guest's frame declares one argument; the JavaScript leaf's stored
/// signature takes two. The host refuses from the header, before V8 runs.
#[tokio::test]
async fn isolated_call_arity_is_checked_against_the_stored_signature() -> Result<()> {
    let store = Store::memory()?;
    let leaf = javascript(&store, "function main(a, b) { return a + b; }", 2)?;
    let payload = loom_proto::isolated::encode_payload(&(21u8,)).map_err(anyhow::Error::msg)?;
    let wasm = wasm_call(
        &store,
        loom_proto::isolated::Request {
            target: loom_proto::isolated::Target::from_hex(&leaf).map_err(anyhow::Error::msg)?,
            entry: "",
            argc: 1,
            payload: &payload,
        },
    )?;
    let error = Runtime::new(store)?
        .call_def(&wasm, json!([]))
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<loom_proto::isolated::CallError>(),
        Some(&loom_proto::isolated::CallError::Arity {
            hash: leaf,
            entry: "main".into(),
            expected: 2,
            actual: 1,
        }),
        "{error:#}"
    );
    Ok(())
}

#[tokio::test]
async fn javascript_schema_uses_runtime_definition_api() -> Result<()> {
    let store = Store::memory()?;
    let hash = javascript(
        &store,
        "const LOOM_SCHEMA = 'CREATE TABLE notes(body TEXT);'; function main() { return null; }",
        0,
    )?;
    assert_eq!(
        Runtime::new(store)?.definition_schema(&hash).await?,
        "CREATE TABLE notes(body TEXT);"
    );
    Ok(())
}

#[tokio::test]
async fn javascript_nested_calls_exceed_worker_count() -> Result<()> {
    let store = Store::memory()?;
    let hash = javascript(
        &store,
        "async function main(depth, hash) { if (depth === 0) return 0; return 1 + await loom.perform('call', {def:hash, args:[depth-1,hash]}); }",
        2,
    )?;
    let runtime = Runtime::new(store)?;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        runtime.call_def(&hash, json!([6, hash])),
    )
    .await??;
    assert_eq!(result, json!(6));
    Ok(())
}
