use super::*;
use loom_sandbox::Sandbox;
use std::{future::Future, pin::Pin};

/// A stored Wasm program using the same borrowed effect contract as V8.
pub struct WasmSandbox {
    runtime: Runtime,
    hash: String,
}

impl WasmSandbox {
    pub fn new(runtime: Runtime, hash: String) -> Self {
        Self { runtime, hash }
    }
}

impl Sandbox for WasmSandbox {
    fn call<'a>(
        &'a self,
        args: Value,
        effects: &'a mut dyn CallEffects,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>> {
        Box::pin(self.runtime.call_with_effects(&self.hash, args, effects))
    }
}

impl Runtime {
    /// Rust-only callers do not start V8 workers. Clones share this engine.
    pub fn v8_engine(&self) -> Result<Arc<loom_v8::V8Engine>> {
        let mut engine = self.inner.v8_engine.lock().unwrap();
        if let Some(engine) = engine.as_ref() {
            return Ok(engine.clone());
        }
        let initialized = Arc::new(loom_v8::V8Engine::new(loom_v8::Limits::default())?);
        *engine = Some(initialized.clone());
        Ok(initialized)
    }

    pub async fn javascript_program(&self, hash: &str) -> Result<Arc<loom_v8::V8Sandbox>> {
        let mut programs = self.inner.javascript_programs.lock().await;
        if let Some(program) = programs.get(hash) {
            return Ok(program.clone());
        }
        let source = self
            .inner
            .store
            .javascript_source(hash, loom_v8::ABI_VERSION)?;
        let program = Arc::new(self.v8_engine()?.compile(&source).await?);
        // Definitions remain in CAS; eviction only drops compiled code. Bound
        // this cache independently from the number of definitions in the store.
        if programs.len() >= 128 {
            if let Some(victim) = programs.keys().next().cloned() {
                programs.remove(&victim);
            }
        }
        programs.insert(hash.to_owned(), program.clone());
        Ok(program)
    }

    pub(super) async fn javascript_call(
        &self,
        definition: &loom_proto::Def,
        args: Value,
        scope: &str,
        effects: &EffectContext,
    ) -> Result<EncodedCall> {
        let start = Instant::now();
        let program = self.javascript_program(&definition.hash).await?;
        let load_ms = elapsed_ms(start);
        let mut effects =
            effects.delegated(&definition.hash, definition.allowed_effects.as_deref());
        // JavaScript effects are dynamic. Only an explicitly known row can
        // narrow them; treating an unknown row as empty would forbid all calls.
        if !definition.sig.effects.unknown {
            effects = effects.with_inferred(&definition.sig.effects.labels);
        }
        let mut handler = RuntimeEffects {
            runtime: self,
            scope,
            effects,
            occurrence: 0,
        };
        let run_start = Instant::now();
        let result = program.call(args, &mut handler).await;
        if let Some(trace) = &handler.effects.trace {
            trace.finish_scope(scope);
        }
        let value = result?;
        Ok(EncodedCall {
            output: EffectOutput::value(&value)?,
            timing: RuntimeTiming {
                component_hash: definition
                    .component_hash
                    .clone()
                    .context("JavaScript executable identity missing")?,
                total_ms: elapsed_ms(start),
                load_ms,
                run_ms: elapsed_ms(run_start),
                ..RuntimeTiming::default()
            },
        })
    }
}

struct RuntimeEffects<'a> {
    runtime: &'a Runtime,
    scope: &'a str,
    effects: EffectContext,
    occurrence: i64,
}

impl CallEffects for RuntimeEffects<'_> {
    fn perform(
        &mut self,
        descriptor: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + '_>> {
        Box::pin(async move {
            let occurrence = self.occurrence;
            self.occurrence = self
                .occurrence
                .checked_add(1)
                .context("effect occurrence overflow")?;
            self.runtime
                .dispatch_root(descriptor, self.scope, occurrence, self.effects.clone())
                .await?
                .decode()
        })
    }
}
