mod call;
mod sandbox;
pub use sandbox::WasmSandbox;
mod compilation_cache;
pub use call::{CallEffects, GuestFailure};
pub use compilation_cache::{CompilationCacheStats, LoomCompilationCache};
mod calls;
mod commands;
mod filesystem;
mod machine;
mod root_handler;
mod sharedcore;
mod trace;
pub mod wasm_engine;
use anyhow::{Context, Result, bail};
use loom_proto::Value;
use loom_store::Store;
use serde_json::json;
use std::{
    collections::{BTreeSet, HashMap},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Mutex as AsyncMutex;
use wasmtime::Engine;

#[derive(Clone)]
pub struct Runtime {
    inner: Arc<Inner>,
}
pub trait ComponentResolver: Send + Sync {
    fn ensure_built<'a>(
        &'a self,
        hash: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>>;
}
struct Inner {
    model: loom_model::Model,
    processes: loom_process::Supervisor,
    resolver: Option<Arc<dyn ComponentResolver>>,
    store: Store,
    core_engine: Engine,
    v8_engine: Mutex<Option<Arc<loom_v8::V8Engine>>>,
    javascript_programs: AsyncMutex<HashMap<String, Arc<loom_v8::V8Sandbox>>>,
    compilation_cache: Arc<LoomCompilationCache>,
    core_executor: futures::executor::ThreadPool,
    core_modules: Mutex<HashMap<String, wasmtime::Module>>,
    component_locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    effect_locks: Mutex<HashMap<String, Weak<AsyncMutex<()>>>>,
    handler_round_trip_us: Mutex<HandlerMeasurements>,
    effect_wire_bytes: AtomicU64,
    trace_locks: Mutex<HashMap<String, Weak<AsyncMutex<()>>>>,
    machine_roots: Mutex<HashMap<String, Arc<filesystem::PinnedRoot>>>,
}
#[derive(Default)]
struct HandlerMeasurements {
    scope: String,
    samples: Vec<f64>,
}
#[derive(Clone, Default)]
struct EffectContext {
    root: Option<tokio::sync::mpsc::Sender<call::Request>>,
    def_hash: Option<String>,
    allowed: Option<BTreeSet<String>>,
    trace: Option<Arc<trace::ExecutionTrace>>,
}
impl EffectContext {
    fn delegated(&self, def_hash: &str, allowed: Option<&[String]>) -> Self {
        let requested = allowed.map(|labels| labels.iter().cloned().collect::<BTreeSet<_>>());
        let allowed = match (&self.allowed, requested) {
            (Some(parent), Some(child)) => Some(parent.intersection(&child).cloned().collect()),
            (Some(parent), None) => Some(parent.clone()),
            (None, child) => child,
        };
        Self {
            root: self.root.clone(),
            def_hash: Some(def_hash.into()),
            allowed,
            trace: self.trace.clone(),
        }
    }
    fn with_inferred(mut self, labels: &[String]) -> Self {
        let inferred = labels.iter().cloned().collect::<BTreeSet<_>>();
        self.allowed = Some(match self.allowed.take() {
            Some(allowed) => allowed.intersection(&inferred).cloned().collect(),
            None => inferred,
        });
        self
    }
    fn permits(&self, op: &str) -> bool {
        self.allowed
            .as_ref()
            .is_none_or(|allowed| allowed.contains(op))
    }
}
#[derive(Debug, Clone)]
struct EffectOutput {
    bytes: Vec<u8>,
}
impl EffectOutput {
    fn value(value: &Value) -> Result<Self> {
        Ok(Self {
            bytes: encode(value)?,
        })
    }
    fn decode(&self) -> Result<Value> {
        loom_proto::decode_host(&self.bytes).map_err(anyhow::Error::msg)
    }
}
fn encode(value: &Value) -> Result<Vec<u8>> {
    loom_proto::encode(value).map_err(anyhow::Error::msg)
}
#[derive(Debug, Default, serde::Serialize)]
pub struct RuntimeTiming {
    pub component_hash: String,
    pub cache_hit: bool,
    pub total_ms: f64,
    pub compile_wait_ms: f64,
    pub resolve_ms: f64,
    pub load_ms: f64,
    pub compile_ms: f64,
    pub link_ms: f64,
    pub instantiate_ms: f64,
    pub run_ms: f64,
}
#[derive(Debug, serde::Serialize)]
pub struct TimedCall {
    pub scope: String,
    pub value: Value,
    pub timing: RuntimeTiming,
}
fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}
struct EncodedCall {
    output: EffectOutput,
    timing: RuntimeTiming,
}
impl Runtime {
    fn effect_lock(&self, key: String) -> Arc<AsyncMutex<()>> {
        let mut locks = self.inner.effect_locks.lock().unwrap();
        locks.retain(|_, lock| lock.strong_count() != 0);
        if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(AsyncMutex::new(()));
        locks.insert(key, Arc::downgrade(&lock));
        lock
    }
    fn trace_lock(&self, scope: &str) -> Arc<AsyncMutex<()>> {
        let mut locks = self.inner.trace_locks.lock().unwrap();
        locks.retain(|_, lock| lock.strong_count() != 0);
        if let Some(lock) = locks.get(scope).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(AsyncMutex::new(()));
        locks.insert(scope.into(), Arc::downgrade(&lock));
        lock
    }
    /// Host-observed handler samples from the latest completed core execution.
    /// Includes request copy/decode, guest handler execution and response copy;
    /// excludes the performer's effect encoding and final response decoding.
    pub fn handler_round_trip_us(&self) -> Value {
        let measurements = self.inner.handler_round_trip_us.lock().unwrap();
        let mut samples = measurements.samples.clone();
        samples.sort_by(f64::total_cmp);
        if samples.is_empty() {
            return json!({"scope": measurements.scope, "measurement": "host_dispatch", "samples": 0, "median": null, "p99": null});
        }
        json!({"scope": measurements.scope, "measurement": "host_dispatch", "samples": samples.len(), "median": samples[samples.len() / 2],
            "p99": samples[(samples.len() * 99 / 100).min(samples.len() - 1)]})
    }
    pub fn effect_wire_bytes(&self) -> u64 {
        self.inner.effect_wire_bytes.load(Ordering::Relaxed)
    }
    pub fn new(store: Store) -> Result<Self> {
        Self::create(store, None)
    }
    /// Candidate function-cache hits and native compiler storage diagnostics.
    pub fn compilation_cache_stats(&self) -> CompilationCacheStats {
        self.inner.compilation_cache.stats()
    }
    pub fn with_resolver(store: Store, resolver: Arc<dyn ComponentResolver>) -> Result<Self> {
        Self::create(store, Some(resolver))
    }
    fn create(store: Store, resolver: Option<Arc<dyn ComponentResolver>>) -> Result<Self> {
        let (core_engine, compilation_cache) = sharedcore::engine(store.clone())?;
        let runtime = Self {
            inner: Arc::new(Inner {
                model: loom_model::Model::from_env(store.clone())?,
                processes: loom_process::Supervisor::new(store.clone())?,
                resolver,
                store,
                core_engine,
                v8_engine: Mutex::new(None),
                javascript_programs: AsyncMutex::new(HashMap::new()),
                compilation_cache,
                core_executor: futures::executor::ThreadPoolBuilder::new()
                    .pool_size(8)
                    .name_prefix("loom-guest-")
                    .create()?,
                core_modules: Mutex::new(HashMap::new()),
                component_locks: Mutex::new(HashMap::new()),
                effect_locks: Mutex::new(HashMap::new()),
                handler_round_trip_us: Mutex::new(HandlerMeasurements::default()),
                effect_wire_bytes: AtomicU64::new(0),
                trace_locks: Mutex::new(HashMap::new()),
                machine_roots: Mutex::new(HashMap::new()),
            }),
        };
        let weak = Arc::downgrade(&runtime.inner);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(10));
                let Some(inner) = weak.upgrade() else { break };
                inner.core_engine.increment_epoch();
            }
        });
        Ok(runtime)
    }
    pub fn perform<'a>(
        &'a self,
        desc: Value,
        scope: &'a str,
        occurrence: i64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value>> + Send + 'a>> {
        Box::pin(async move {
            let lock = self.trace_lock(scope);
            let _guard = lock.lock().await;

            let execution = match self.inner.store.load_call_trace(scope)? {
                Some(bundle) => trace::ExecutionTrace::loaded(bundle)?,
                None => trace::ExecutionTrace::fresh(scope),
            };
            let session = trace::TraceSession::new(self.inner.store.clone(), execution.clone());
            let effects = EffectContext {
                trace: Some(execution),
                ..EffectContext::default()
            };
            let outcome = self.dispatch_root(desc, scope, occurrence, effects).await;
            session.finish(&outcome)?;
            outcome?.decode()
        })
    }
    fn dispatch_root<'a>(
        &'a self,
        desc: Value,
        scope: &'a str,
        occurrence: i64,
        effects: EffectContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<EffectOutput>> + Send + 'a>>
    {
        root_handler::dispatch(self, desc, scope, occurrence, effects)
    }
}
fn capture_paths(args: &Value) -> Result<Vec<std::path::PathBuf>> {
    match args.get("capture_paths") {
        None => Ok(Vec::new()),
        Some(value) => value
            .as_array()
            .context("capture_paths must be an array")?
            .iter()
            .map(|path| {
                path.as_str()
                    .map(std::path::PathBuf::from)
                    .context("capture path must be a string")
            })
            .collect(),
    }
}

fn required_str<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("{key} must be a string"))
}

fn resolve_self(value: &mut Value, hash: &str) {
    if let Some("call") = value.get("op").and_then(Value::as_str)
        && value.pointer("/args/def").and_then(Value::as_str) == Some("$self")
    {
        value["args"]["def"] = json!(hash);
    }
}

#[cfg(test)]
mod tests;
