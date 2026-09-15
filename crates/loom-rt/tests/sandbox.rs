//! Production Runtime admission, effect dispatch, and replay across both engines.
use anyhow::Result;
use loom_proto::{Def, Lang};
use loom_rt::{Runtime, WasmSandbox};
use loom_sandbox::{CallEffects, Sandbox};
use loom_store::Store;
use serde_json::{Value, json};
use std::{collections::BTreeMap, future::Future, pin::Pin};

fn publish(
    store: &Store,
    lang: Lang,
    source: &str,
    artifact: &[u8],
    labels: &[&str],
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
    let definition = Def {
        hash: hash.clone(),
        lang,
        component_hash: Some(artifact_hash),
        sig: serde_json::from_value(
            json!({"exports":[{"name":"main","params":[],"returns":{"type":"value"},"effects":effects}],"effects":effects}),
        )?,
        allowed_effects: None,
        observed_effects: vec![],
    };
    store.define(&definition, None, source, &BTreeMap::new())?;
    Ok(hash)
}
fn javascript(store: &Store, source: &str) -> Result<String> {
    publish(store, Lang::JavaScript, source, source.as_bytes(), &[])
}
fn wasm_effect(store: &Store, descriptor: Value, labels: &[&str]) -> Result<String> {
    let bytes = loom_proto::encode(&descriptor).map_err(anyhow::Error::msg)?;
    let data = bytes
        .iter()
        .map(|byte| format!("\\{byte:02x}"))
        .collect::<String>();
    // The imported perform returns the same canonical {ok: value} envelope
    // that loom_call_main returns. This exercises the production ABI without
    // depending on the separate Rust guest compilation toolchain.
    let mut artifact = wat::parse_str(format!(
        r#"(module
        (import "env" "memory" (memory 1 1 shared))
        (import "loom" "perform" (func $perform (param i32 i32) (result i64)))
        (global (export "__stack_pointer") (mut i32) (i32.const 65536))
        (global (export "__loom_stack_low") (mut i32) (i32.const 32768))
        (global (export "__loom_stack_high") (mut i32) (i32.const 65536))
        (global $heap (mut i32) (i32.const 8192))
        (func (export "loom_alloc") (param $size i32) (param i32) (result i32)
            (local $pointer i32)
            global.get $heap local.tee $pointer
            local.get $size i32.add global.set $heap local.get $pointer)
        (func (export "loom_dealloc") (param i32 i32 i32))
        (data (i32.const 1024) "{data}")
        (func (export "loom_call_main") (param i32 i32) (result i64)
            i32.const 1024 i32.const {length} call $perform)
    )"#,
        length = bytes.len()
    ))?;
    loom_proto::core_protocol::stamp(&mut artifact);
    publish(
        store,
        Lang::Rust,
        "fixture: one production root effect",
        &artifact,
        labels,
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

#[tokio::test]
async fn javascript_calls_wasm_calls_javascript() -> Result<()> {
    let store = Store::memory()?;
    let leaf = javascript(&store, "function main(value) { return value * 2; }")?;
    let wasm = wasm_effect(
        &store,
        json!({"op":"call","args":{"def":leaf,"args":[21]}}),
        &["call"],
    )?;
    let outer = javascript(
        &store,
        &format!(
            "async function main() {{ return 1 + await loom.perform('call', {{def:'{wasm}', args:[]}}); }}"
        ),
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

#[tokio::test]
async fn javascript_schema_uses_runtime_definition_api() -> Result<()> {
    let store = Store::memory()?;
    let hash = javascript(
        &store,
        "const LOOM_SCHEMA = 'CREATE TABLE notes(body TEXT);'; function main() { return null; }",
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
