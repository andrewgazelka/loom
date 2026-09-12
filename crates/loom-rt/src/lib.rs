mod filesystem;
mod machine;
mod root_handler;
mod sharedcore;
mod trace;
use anyhow::{Context, Result, bail};
use loom_proto::{Actor, Value};
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
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, StoreLimits, StoreLimitsBuilder};

wasmtime::component::bindgen!({path: "../../loom-wit", world: "handler", imports: { default: async }, exports: { default: async }});

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
    engine: Engine,
    core_engine: Engine,
    core_executor: futures::executor::ThreadPool,
    core_modules: Mutex<HashMap<String, wasmtime::Module>>,
    components: Mutex<HashMap<String, HandlerPre<ContextData>>>,
    component_locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    actor_locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
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
    def_hash: Option<String>,
    actor_id: Option<String>,
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
            def_hash: Some(def_hash.into()),
            actor_id: self.actor_id.clone(),
            allowed,
            trace: self.trace.clone(),
        }
    }
    fn with_declared(mut self, declared: Option<&[String]>) -> Self {
        if let Some(declared) = declared {
            let declared = declared.iter().cloned().collect::<BTreeSet<_>>();
            self.allowed = Some(match self.allowed.take() {
                Some(allowed) => allowed.intersection(&declared).cloned().collect(),
                None => declared,
            });
        }
        self
    }
    fn permits(&self, op: &str) -> bool {
        self.allowed
            .as_ref()
            .is_none_or(|allowed| allowed.contains(op))
    }
}
struct ContextData {
    runtime: Runtime,
    scope: String,
    def_hash: String,
    occurrence: i64,
    limits: StoreLimits,
    pure: bool,
    effects: EffectContext,
}
impl loom::host::effects::Host for ContextData {
    async fn perform(&mut self, desc: Vec<u8>) -> std::result::Result<Vec<u8>, String> {
        let result = async {
            if self.pure {
                return Err("fold cannot perform effects".into());
            }
            if desc.len() > loom_proto::TRACE_MAX_BLOB_BYTES {
                return Err("effect exceeds trace byte limit".into());
            }
            let mut desc = loom_proto::decode::<Value>(&desc)?;
            resolve_self(&mut desc, &self.def_hash);
            let occurrence = self.occurrence;
            self.occurrence += 1;
            self.runtime
                .dispatch_root(desc, &self.scope, occurrence, self.effects.clone())
                .await
                .map(|output| output.bytes)
                .map_err(|error| format!("{error:#}"))
        }
        .await;
        let bytes = match &result {
            Ok(bytes) => bytes.len(),
            Err(message) => message.len(),
        };
        self.runtime
            .inner
            .effect_wire_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
        result
    }
}
#[derive(Debug, Clone)]
struct EffectOutput {
    bytes: Vec<u8>,
}
impl EffectOutput {
    fn from_guest(bytes: Vec<u8>) -> Result<Self> {
        anyhow::ensure!(
            bytes.len() <= loom_proto::TRACE_MAX_BLOB_BYTES,
            "guest result exceeds trace byte limit"
        );
        // Guest bytes cross the admission boundary before hashing or recording success.
        decode(&bytes)?;
        Ok(Self { bytes })
    }
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
fn decode(bytes: &[u8]) -> Result<Value> {
    loom_proto::decode(bytes).map_err(anyhow::Error::msg)
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
struct Instance {
    timing: RuntimeTiming,
    bindings: Handler,
    store: wasmtime::Store<ContextData>,
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
    pub fn with_resolver(store: Store, resolver: Arc<dyn ComponentResolver>) -> Result<Self> {
        Self::create(store, Some(resolver))
    }
    fn create(store: Store, resolver: Option<Arc<dyn ComponentResolver>>) -> Result<Self> {
        let mut config = Config::new();
        config
            .epoch_interruption(true)
            .wasm_component_model(true)
            .memory_init_cow(true)
            .async_stack_size(512 * 1024);
        config.allocation_strategy(wasmtime::InstanceAllocationStrategy::Pooling(
            Default::default(),
        ));
        let engine = Engine::new(&config)?;
        let runtime = Self {
            inner: Arc::new(Inner {
                model: loom_model::Model::from_env(store.clone())?,
                processes: loom_process::Supervisor::new(store.clone())?,
                resolver,
                store,
                engine,
                core_engine: sharedcore::engine()?,
                core_executor: futures::executor::ThreadPoolBuilder::new()
                    .pool_size(8)
                    .name_prefix("loom-guest-")
                    .create()?,
                core_modules: Mutex::new(HashMap::new()),
                components: Mutex::new(HashMap::new()),
                component_locks: Mutex::new(HashMap::new()),
                actor_locks: Mutex::new(HashMap::new()),
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
                inner.engine.increment_epoch();
                inner.core_engine.increment_epoch();
            }
        });
        Ok(runtime)
    }
    async fn instance(&self, hash: &str, scope: &str, pure: bool) -> Result<Instance> {
        self.instance_delegated(hash, scope, pure, &EffectContext::default())
            .await
    }
    async fn instance_delegated(
        &self,
        hash: &str,
        scope: &str,
        pure: bool,
        parent: &EffectContext,
    ) -> Result<Instance> {
        let resolve_start = Instant::now();
        let def = self
            .inner
            .store
            .executable_definition(hash)?
            .context("definition not found")?;
        let effects = parent
            .delegated(hash, def.allowed_effects.as_deref())
            .with_declared(def.sig.effects.declared.as_deref());
        let component_hash = match def.component_hash {
            Some(hash) => hash,
            None => {
                self.inner
                    .resolver
                    .as_ref()
                    .context("definition has no built component and no builder configured")?
                    .ensure_built(hash)
                    .await?;
                self.inner
                    .store
                    .executable_definition(hash)?
                    .and_then(|d| d.component_hash)
                    .context("builder did not publish component")?
            }
        };
        let mut timing = RuntimeTiming {
            component_hash: component_hash.clone(),
            resolve_ms: elapsed_ms(resolve_start),
            ..Default::default()
        };
        let component_lock = self
            .inner
            .component_locks
            .lock()
            .unwrap()
            .entry(component_hash.clone())
            .or_default()
            .clone();
        let wait_start = Instant::now();
        let compile_guard = component_lock.lock().await;
        timing.compile_wait_ms = elapsed_ms(wait_start);
        let cached = self
            .inner
            .components
            .lock()
            .unwrap()
            .get(&component_hash)
            .cloned();
        let component = match cached {
            Some(c) => {
                timing.cache_hit = true;
                c
            }
            None => {
                let load_start = Instant::now();
                let bytes = self
                    .inner
                    .store
                    .get(&component_hash)?
                    .context("component missing from CAS")?;
                anyhow::ensure!(
                    loom_proto::component_protocol::is_current(&bytes),
                    "component uses an obsolete host protocol; migrate fs.list consumers to typed DirEntry and redefine the definition before execution"
                );
                timing.load_ms = elapsed_ms(load_start);
                let engine = self.inner.engine.clone();
                let compile_start = Instant::now();
                let c =
                    tokio::task::spawn_blocking(move || Component::new(&engine, bytes)).await??;
                timing.compile_ms = elapsed_ms(compile_start);
                let link_start = Instant::now();
                let mut linker = Linker::new(&self.inner.engine);
                Handler::add_to_linker::<_, wasmtime::component::HasSelf<_>>(
                    &mut linker,
                    |state| state,
                )?;
                linker.define_unknown_imports_as_traps(&c)?;
                let pre = HandlerPre::new(linker.instantiate_pre(&c)?)?;
                timing.link_ms = elapsed_ms(link_start);
                self.inner
                    .components
                    .lock()
                    .unwrap()
                    .insert(component_hash, pre.clone());
                pre
            }
        };
        drop(compile_guard);
        let instantiate_start = Instant::now();
        let mut store = wasmtime::Store::new(
            &self.inner.engine,
            ContextData {
                runtime: self.clone(),
                scope: scope.into(),
                def_hash: hash.into(),
                occurrence: 0,
                limits: StoreLimitsBuilder::new()
                    .memory_size(256 * 1024 * 1024)
                    .instances(16)
                    .build(),
                pure,
                effects,
            },
        );
        store.limiter(|s| &mut s.limits);
        store.set_epoch_deadline(1);
        store.epoch_deadline_async_yield_and_update(1);
        let bindings = component.instantiate_async(&mut store).await?;
        timing.instantiate_ms = elapsed_ms(instantiate_start);
        Ok(Instance {
            bindings,
            store,
            timing,
        })
    }
    pub async fn call_def(&self, hash: &str, args: Value) -> Result<Value> {
        Ok(self.call_def_timed(hash, args).await?.value)
    }
    pub async fn call_def_timed(&self, hash: &str, args: Value) -> Result<TimedCall> {
        self.call_scoped_timed(hash, args, &format!("call:{}", uuid::Uuid::new_v4()))
            .await
    }
    async fn call_scoped(
        &self,
        hash: &str,
        args: Value,
        scope: &str,
        effects: EffectContext,
    ) -> Result<EffectOutput> {
        Ok(self
            .call_scoped_delegated(hash, args, scope, effects)
            .await?
            .output)
    }
    async fn call_scoped_timed(&self, hash: &str, args: Value, scope: &str) -> Result<TimedCall> {
        self.call_traced_timed(hash, args, scope, trace::ExecutionTrace::fresh(scope))
            .await
    }
    pub async fn replay_def_timed(
        &self,
        hash: &str,
        args: Value,
        scope: &str,
    ) -> Result<TimedCall> {
        let lock = self.trace_lock(scope);
        let _guard = lock.lock().await;
        let bundle = self
            .inner
            .store
            .load_call_trace(scope)?
            .context("call trace not found")?;
        anyhow::ensure!(
            bundle.trace.outcome.is_some(),
            "cannot explicitly replay an incomplete call trace"
        );
        anyhow::ensure!(
            !matches!(
                bundle.trace.outcome,
                Some(loom_proto::TraceOutcome::Cancelled)
            ),
            "recorded execution was cancelled before completion"
        );
        let execution = trace::ExecutionTrace::loaded(bundle)?;
        self.call_traced_timed(hash, args, scope, execution).await
    }
    async fn call_traced_timed(
        &self,
        hash: &str,
        args: Value,
        scope: &str,
        execution: Arc<trace::ExecutionTrace>,
    ) -> Result<TimedCall> {
        execution.identity(hash, &args)?;
        let session = trace::TraceSession::new(self.inner.store.clone(), execution.clone());
        let effects = EffectContext {
            trace: Some(execution),
            ..EffectContext::default()
        };
        let result = self.call_scoped_delegated(hash, args, scope, effects).await;
        let outcome = match &result {
            Ok(call) => Ok(call.output.clone()),
            Err(error) => Err(anyhow::anyhow!("{error:#}")),
        };
        session.finish(&outcome)?;
        let call = result?;
        Ok(TimedCall {
            scope: scope.into(),
            value: call.output.decode()?,
            timing: call.timing,
        })
    }
    async fn call_scoped_delegated(
        &self,
        hash: &str,
        args: Value,
        scope: &str,
        effects: EffectContext,
    ) -> Result<EncodedCall> {
        let core = self.core_call(hash, &args, scope, &effects).await;
        if !matches!(core, Ok(None)) {
            return core?.context("core dispatch lost its result");
        }
        let call_start = Instant::now();
        let mut instance = self
            .instance_delegated(hash, scope, false, &effects)
            .await?;
        let run_start = Instant::now();
        let result = instance
            .bindings
            .call_call(&mut instance.store, &encode(&json!(hash))?, &encode(&args)?)
            .await;
        let result = result?.map_err(anyhow::Error::msg)?;
        let output = EffectOutput::from_guest(result)?;
        instance.timing.run_ms = elapsed_ms(run_start);
        instance.timing.total_ms = elapsed_ms(call_start);
        Ok(EncodedCall {
            output,
            timing: instance.timing,
        })
    }
    pub async fn spawn(&self, hash: &str, initial: Value) -> Result<Actor> {
        self.spawn_identified(hash, initial, uuid::Uuid::new_v4().to_string())
            .await
    }
    async fn spawn_identified(&self, hash: &str, initial: Value, id: String) -> Result<Actor> {
        if let Some(actor) = self.inner.store.actor(&id)? {
            return Ok(actor);
        }
        // Instantiate before publishing an actor so missing imports/components fail immediately.
        if self
            .core_execute(
                hash,
                "actor.spawn",
                &EffectContext::default(),
                true,
                sharedcore::Entry::Validate,
            )
            .await?
            .is_none()
        {
            self.instance(hash, "actor.spawn", true).await?;
        }
        let def = self
            .inner
            .store
            .definition(hash)?
            .context("built definition disappeared")?;
        let actor = Actor {
            id,
            behavior_hash: hash.into(),
            lang: def.lang,
            component_hash: def.component_hash,
            last_seq: 0,
            created_seq: 0,
            parent: None,
        };
        self.inner.store.create_initialized_actor(&actor, &initial)
    }
    pub async fn state(&self, actor: &str) -> Result<Value> {
        let actor = self.inner.store.actor(actor)?.context("actor not found")?;
        let snapshot = self
            .inner
            .store
            .latest_snapshot(&actor.id, &actor.behavior_hash)?;
        let mut seq = snapshot.as_ref().map_or(0, |s| s.seq);
        let mut state = snapshot.map_or(Value::Null, |s| s.state);
        let mut instance = None;
        loop {
            let events = self.inner.store.events(Some(&actor.id), seq, 1000)?;
            if events.is_empty() {
                break;
            }
            for event in events {
                seq = event.seq;
                if event.handler_seq == 0
                    && let Some(initial) = event.event.get("__loom_init")
                {
                    state = initial.clone();
                    continue;
                }
                if let Some(folded) = self
                    .core_execute(
                        &actor.behavior_hash,
                        "fold",
                        &EffectContext::default(),
                        true,
                        sharedcore::Entry::Fold {
                            state: &state,
                            event: &event.event,
                        },
                    )
                    .await?
                {
                    state = folded.output.decode()?;
                    continue;
                }
                if instance.is_none() {
                    instance = Some(self.instance(&actor.behavior_hash, "fold", true).await?);
                }
                let i = instance.as_mut().unwrap();
                state = decode(
                    &i.bindings
                        .call_fold(&mut i.store, &encode(&state)?, &encode(&event.event)?)
                        .await?,
                )?;
            }
        }
        self.inner
            .store
            .snapshot(&actor.id, &actor.behavior_hash, seq, &state)?;
        Ok(state)
    }
    pub async fn send(&self, actor: &str, msg: Value) -> Result<Value> {
        let message = self.inner.store.enqueue(actor, &msg)?;
        self.drain_actor(actor, Some(message.handler_seq)).await?;
        self.state(actor).await
    }
    pub fn enqueue(&self, actor: &str, msg: Value) -> Result<Value> {
        let message = self.inner.store.enqueue(actor, &msg)?;
        self.schedule_message(message)
    }
    fn schedule_message(&self, message: loom_store::PendingMessage) -> Result<Value> {
        let runtime = self.clone();
        let actor = message.actor.clone();
        tokio::spawn(async move {
            if let Err(error) = runtime.drain_actor(&actor, None).await {
                let _ = runtime.inner.store.append(
                    "system",
                    &json!({"type":"handler_failed","actor":actor,"error":format!("{error:#}")}),
                    0,
                );
            }
        });
        Ok(json!({"actor":message.actor,"handler_seq":message.handler_seq}))
    }
    async fn drain_actor(&self, actor: &str, until: Option<i64>) -> Result<()> {
        let lock = self
            .inner
            .actor_locks
            .lock()
            .unwrap()
            .entry(actor.into())
            .or_default()
            .clone();
        let _guard = lock.lock().await;
        loop {
            let Some(message) = self.inner.store.pending(actor)? else {
                break;
            };
            if until.is_some_and(|seq| message.handler_seq > seq) {
                break;
            }
            let sequence = message.handler_seq;
            self.deliver(message).await?;
            if until == Some(sequence) {
                break;
            }
        }
        Ok(())
    }
    async fn deliver(&self, message: loom_store::PendingMessage) -> Result<()> {
        let actor = &message.actor;
        let metadata = self.inner.store.actor(actor)?.context("actor not found")?;
        let state = self.state(actor).await?;
        let handler_seq = message.handler_seq;
        let scope = format!("{actor}:{handler_seq}");
        let execution = match self.inner.store.load_call_trace(&scope)? {
            Some(bundle) => trace::ExecutionTrace::loaded(bundle)?,
            None => trace::ExecutionTrace::fresh(&scope),
        };
        execution.identity(
            &metadata.behavior_hash,
            &json!({"state":state,"message":message.msg}),
        )?;
        let session =
            trace::TraceSession::new(self.inner.store.clone(), execution.clone()).recoverable();
        let effects = EffectContext {
            trace: Some(execution),
            actor_id: Some(actor.clone()),
            ..EffectContext::default()
        };
        let bytes = match self
            .core_execute(
                &metadata.behavior_hash,
                &scope,
                &effects,
                false,
                sharedcore::Entry::Run {
                    state: &state,
                    message: &message.msg,
                },
            )
            .await
        {
            Ok(Some(call)) => Ok(call.output.bytes),
            Ok(None) => {
                let mut instance = self
                    .instance_delegated(&metadata.behavior_hash, &scope, false, &effects)
                    .await?;
                instance
                    .bindings
                    .call_run(
                        &mut instance.store,
                        &encode(&state)?,
                        &encode(&message.msg)?,
                    )
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|result| result.map_err(anyhow::Error::msg))
            }
            Err(error) => Err(error),
        };
        let bytes = bytes?;
        let events = decode(&bytes)?
            .as_array()
            .context("run must return array of events")?
            .clone();
        session.finish(&Ok(EffectOutput { bytes }))?;
        self.inner
            .store
            .complete_message(actor, handler_seq, &events)?;
        Ok(())
    }
    pub async fn recover_pending(&self) -> Result<usize> {
        let pending = self.inner.store.pending_messages()?;
        let count = pending.len();
        let actors = pending
            .into_iter()
            .map(|message| message.actor)
            .collect::<std::collections::BTreeSet<_>>();
        for actor in actors {
            self.drain_actor(&actor, None).await?;
        }
        Ok(count)
    }
    pub async fn fork_actor(&self, actor: &str) -> Result<Actor> {
        let lock = self
            .inner
            .actor_locks
            .lock()
            .unwrap()
            .entry(actor.into())
            .or_default()
            .clone();
        let _guard = lock.lock().await;
        let original = self.inner.store.actor(actor)?.context("actor not found")?;
        let mut initial = Value::Null;
        let mut history = Vec::new();
        let mut after = 0;
        loop {
            let events = self.inner.store.events(Some(actor), after, 1000)?;
            if events.is_empty() {
                break;
            }
            for event in events {
                after = event.seq;
                if event.handler_seq == 0
                    && let Some(state) = event.event.get("__loom_init")
                {
                    initial = state.clone();
                } else {
                    history.push(event.event);
                }
            }
        }
        let mut fork = self.spawn(&original.behavior_hash, initial).await?;
        fork.parent = Some(actor.into());
        self.inner.store.update_actor(&fork)?;
        self.inner.store.append_batch(&fork.id, &history, 1)?;
        self.state(&fork.id).await?;
        self.inner
            .store
            .actor(&fork.id)?
            .context("fork disappeared")
    }
    pub async fn upgrade(&self, actor: &str, hash: &str) -> Result<Value> {
        let lock = self
            .inner
            .actor_locks
            .lock()
            .unwrap()
            .entry(actor.into())
            .or_default()
            .clone();
        let _guard = lock.lock().await;
        if self.inner.store.pending(actor)?.is_some() {
            bail!("cannot upgrade an actor with a pending message");
        }
        let mut actor = self.inner.store.actor(actor)?.context("actor not found")?;
        let def = self
            .inner
            .store
            .definition(hash)?
            .context("definition not found")?;
        if actor.lang != def.lang {
            bail!("cross-language upgrade requires a migration");
        }
        if self
            .core_execute(
                hash,
                "upgrade",
                &EffectContext::default(),
                true,
                sharedcore::Entry::Validate,
            )
            .await?
            .is_none()
        {
            self.instance(hash, "upgrade", true).await?;
        }
        let def = self
            .inner
            .store
            .definition(hash)?
            .context("built definition disappeared")?;
        actor.behavior_hash = hash.into();
        actor.component_hash = def.component_hash;
        self.inner.store.update_actor(&actor)?;
        self.state(&actor.id).await
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
    if matches!(
        value.get("op").and_then(Value::as_str),
        Some("call" | "actor.spawn")
    ) && value.pointer("/args/def").and_then(Value::as_str) == Some("$self")
    {
        value["args"]["def"] = json!(hash);
    }
}

impl Runtime {
    pub fn model(&self) -> &loom_model::Model {
        &self.inner.model
    }
    pub fn processes(&self) -> &loom_process::Supervisor {
        &self.inner.processes
    }
    async fn execute_command(
        &self,
        command: tokio::process::Command,
        capture_paths: Vec<std::path::PathBuf>,
    ) -> Result<Value> {
        let native = command.as_std();
        let cwd = native
            .get_current_dir()
            .map(std::path::Path::to_path_buf)
            .unwrap_or(std::env::current_dir()?);
        let mut env = std::collections::BTreeMap::new();
        env.insert(
            "PATH".into(),
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
        );
        let spec = loom_process::ProcessSpec {
            machine: "local".into(),
            capture_paths,
            program: native
                .get_program()
                .to_str()
                .context("program must be UTF8")?
                .into(),
            args: native
                .get_args()
                .map(|arg| {
                    arg.to_str()
                        .map(str::to_owned)
                        .context("argument must be UTF8")
                })
                .collect::<Result<Vec<_>>>()?,
            root: cwd.clone(),
            cwd,
            env,
        };
        let process = self.inner.processes.start(spec).await?;
        let _cancel = self.inner.processes.cancel_on_drop(&process.id);
        let completed = tokio::time::timeout(
            Duration::from_secs(300),
            self.inner.processes.wait(&process.id),
        )
        .await??;
        if let Some(error) = completed.error {
            bail!("process failed: {error}");
        }
        if completed.phase != loom_process::Phase::Completed {
            bail!("process did not complete: {:?}", completed.phase);
        }
        Ok(
            json!({"code":completed.code,"stdout":completed.stdout,"stderr":completed.stderr,"filesystem_changes":completed.filesystem_changes,"filesystem_capture":completed.filesystem_capture}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn padded_cid() -> Vec<u8> {
        let mut bytes = vec![0xd8, 0x2a, 0x58, 0x26, 0, 1, 0x71, 0x1e, 0x20];
        bytes.extend_from_slice(&[0; 32]);
        bytes.push(0);
        bytes
    }

    #[test]
    fn resolve_self_rewrites_actor_spawn_but_leaves_actor_send_and_other_defs_alone() {
        let hash = "definition-hash";
        let mut spawn = json!({"op":"actor.spawn","args":{"def":"$self","state":0}});
        resolve_self(&mut spawn, hash);
        assert_eq!(spawn["args"]["def"], json!(hash));
        let mut spawn_other = json!({"op":"actor.spawn","args":{"def":"other-hash","state":0}});
        resolve_self(&mut spawn_other, hash);
        assert_eq!(spawn_other["args"]["def"], json!("other-hash"));
        let mut send = json!({"op":"actor.send","args":{"def":"$self"}});
        resolve_self(&mut send, hash);
        assert_eq!(
            send["args"]["def"],
            json!("$self"),
            "actor.send must not resolve $self"
        );
    }
    #[test]
    fn guest_results_require_strict_admission() -> Result<()> {
        let reference = loom_proto::reference(&"00".repeat(32), loom_proto::DAG_CBOR_CODEC)
            .map_err(anyhow::Error::msg)?;
        let canonical = encode(&reference)?;
        assert_eq!(EffectOutput::from_guest(canonical)?.decode()?, reference);
        assert!(EffectOutput::from_guest(padded_cid()).is_err());
        assert!(EffectOutput::from_guest(vec![0, 0]).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn guest_effects_are_validated_before_effect_occurrence() -> Result<()> {
        let runtime = Runtime::new(Store::memory()?)?;
        let mut context = ContextData {
            runtime,
            scope: "guest-admission-control".into(),
            def_hash: "00".repeat(32),
            occurrence: 0,
            limits: StoreLimitsBuilder::new().build(),
            pure: false,
            effects: EffectContext::default(),
        };
        // Canonical map order, with a padded CID nested in otherwise valid sleep args.
        let mut padded = b"\xa2\x62op\x65sleep\x64args\xa2\x61x".to_vec();
        padded.extend(padded_cid());
        padded.extend_from_slice(b"\x62ms\x00");
        let canonical = encode(&json!({"op":"sleep", "args":{"ms":0}}))?;
        let mut trailing = canonical.clone();
        trailing.push(0);
        for invalid in [padded, trailing] {
            assert!(
                loom::host::effects::Host::perform(&mut context, invalid)
                    .await
                    .is_err()
            );
            assert_eq!(context.occurrence, 0);
        }
        loom::host::effects::Host::perform(&mut context, canonical)
            .await
            .map_err(anyhow::Error::msg)?;
        assert_eq!(context.occurrence, 1);
        Ok(())
    }
    #[tokio::test]
    async fn scoped_children_record_independently_and_replay_in_reverse_order() -> Result<()> {
        let runtime = Runtime::new(Store::memory()?)?;
        let execution = trace::ExecutionTrace::fresh("root");
        let effects = EffectContext {
            trace: Some(execution.clone()),
            ..Default::default()
        };
        let descriptor = json!({"op":"random"});
        let first = runtime.dispatch_root(descriptor.clone(), "root/spawn:0", 0, effects.clone());
        let second = runtime.dispatch_root(descriptor.clone(), "root/spawn:1", 0, effects);
        let outputs = futures::future::try_join_all([first, second]).await?;
        let values = outputs
            .iter()
            .map(EffectOutput::decode)
            .collect::<Result<Vec<_>>>()?;
        let result = EffectOutput::value(&json!(values));
        let bundle = execution.snapshot(Some(&result), true)?;
        assert_eq!(bundle.trace.entries.len(), 2);
        assert_eq!(bundle.trace.entries[0].key.scope, "root/spawn:0");
        assert_eq!(bundle.trace.entries[1].key.scope, "root/spawn:1");
        let replay = trace::ExecutionTrace::loaded(bundle)?;
        for index in [1, 0] {
            let output = runtime
                .dispatch_root(
                    descriptor.clone(),
                    &format!("root/spawn:{index}"),
                    0,
                    EffectContext {
                        trace: Some(replay.clone()),
                        ..Default::default()
                    },
                )
                .await?;
            assert_eq!(output.decode()?, values[index]);
        }
        replay.snapshot(Some(&result), true)?;
        Ok(())
    }
    #[tokio::test]
    async fn observational_effect_replays_across_runtime_restart() -> Result<()> {
        let store = Store::memory()?;
        let runtime = Runtime::new(store.clone())?;
        let descriptor = json!({"op":"random","args":{}});
        let first = runtime.perform(descriptor.clone(), "handler-1", 0).await?;
        drop(runtime);
        let runtime = Runtime::new(store)?;
        assert_eq!(
            first,
            runtime.perform(descriptor.clone(), "handler-1", 0).await?
        );
        assert_ne!(first, runtime.perform(descriptor, "handler-2", 0).await?);
        Ok(())
    }
    #[tokio::test]
    async fn guest_cannot_claim_observation_is_hermetic() -> Result<()> {
        let runtime = Runtime::new(Store::memory()?)?;
        let descriptor = json!({"op":"random","args":{},"class":"hermetic"});
        assert_ne!(
            runtime.perform(descriptor.clone(), "a", 0).await?,
            runtime.perform(descriptor, "b", 0).await?
        );
        Ok(())
    }
    #[tokio::test]
    async fn keyed_exec_runs_once_across_actors() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("count");
        let runtime = Runtime::new(Store::memory()?)?;
        let descriptor = json!({"op":"exec","args":{"program":"sh","args":["-c","printf x >> \"$1\"","loom",path],"key":"once"}});
        let results = futures::future::try_join_all([
            runtime.perform(descriptor.clone(), "actor-one", 0),
            runtime.perform(descriptor, "actor-two", 0),
        ])
        .await?;
        assert_eq!(results[0]["code"], json!(0));
        assert_eq!(results[0], results[1]);
        assert_eq!(std::fs::read_to_string(path)?, "x");
        Ok(())
    }
    #[tokio::test]
    async fn exec_capture_preserves_actual_before_after_cas_bytes() -> Result<()> {
        let root = tempfile::tempdir()?;
        std::fs::write(root.path().join("note"), b"before\n")?;
        let runtime = Runtime::new(Store::memory()?)?;
        let mut command = tokio::process::Command::new("sh");
        command
            .current_dir(root.path())
            .args(["-c", "printf 'after\n' > note"]);
        let result = runtime
            .execute_command(command, vec!["note".into()])
            .await?;
        let changes = result["filesystem_changes"].as_array().context("changes")?;
        assert_eq!(changes.len(), 1, "{result}");
        assert_eq!(
            runtime
                .inner
                .store
                .get(changes[0]["before"].as_str().context("before CID")?)?,
            Some(b"before\n".to_vec())
        );
        assert_eq!(
            runtime
                .inner
                .store
                .get(changes[0]["after"].as_str().context("after CID")?)?,
            Some(b"after\n".to_vec())
        );
        Ok(())
    }
    #[tokio::test]
    async fn policy_denies_dynamic_cached_and_delegated_requests() -> Result<()> {
        let runtime = Runtime::new(Store::memory()?)?;
        let desc = json!({"op":"cas.put","args":{"secret":42}});
        runtime.perform(desc.clone(), "warm", 0).await?;
        let denied = EffectContext::default().delegated("restricted", Some(&[]));
        let error = runtime
            .dispatch_root(desc.clone(), "denied", 0, denied.clone())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not allowed"));
        let parent =
            EffectContext::default().delegated("parent", Some(&["sleep".into(), "call".into()]));
        let child = parent.delegated("child", Some(&["sleep".into(), "cas.put".into()]));
        assert!(!child.permits("cas.put"));
        assert!(child.permits("sleep"));
        assert!(
            !parent
                .delegated("unrestricted-child", None)
                .permits("cas.put")
        );
        assert!(
            runtime
                .dispatch_root(desc, "delegated", 0, child)
                .await
                .is_err()
        );
        Ok(())
    }
    #[tokio::test]
    async fn cancellation_persists_an_explicit_cancelled_trace() -> Result<()> {
        let store = Store::memory()?;
        let runtime = Runtime::new(store.clone())?;
        assert!(
            tokio::time::timeout(
                Duration::from_millis(10),
                runtime.perform(
                    json!({"op":"sleep","args":{"ms":5000}}),
                    "cancelled-root",
                    0
                )
            )
            .await
            .is_err()
        );
        let bundle = store
            .load_call_trace("cancelled-root")?
            .context("cancelled trace missing")?;
        assert!(matches!(
            bundle.trace.outcome,
            Some(loom_proto::TraceOutcome::Cancelled)
        ));
        assert_eq!(bundle.trace.entries.len(), 1);
        assert!(matches!(
            bundle.trace.entries[0].outcome,
            loom_proto::TraceOutcome::Cancelled
        ));
        Ok(())
    }
    #[tokio::test]
    async fn effect_observations_follow_delegated_definition_and_exclude_denials() -> Result<()> {
        let runtime = Runtime::new(Store::memory()?)?;
        let execution = trace::ExecutionTrace::fresh("root");
        let parent = EffectContext {
            def_hash: Some("parent".into()),
            trace: Some(execution.clone()),
            ..EffectContext::default()
        };
        runtime
            .dispatch_root(
                json!({"op":"sleep","args":{"ms":0}}),
                "root",
                0,
                parent.clone(),
            )
            .await?;
        runtime
            .dispatch_root(
                json!({"op":"sleep","args":{"ms":0}}),
                "root/child",
                0,
                parent.delegated("child", None),
            )
            .await?;
        assert!(
            runtime
                .dispatch_root(
                    json!({"op":"now"}),
                    "root/denied",
                    0,
                    parent.delegated("denied", Some(&[]))
                )
                .await
                .is_err()
        );
        let bundle = execution.snapshot(None, true)?;
        assert_eq!(
            bundle.observations,
            vec![
                loom_proto::TraceObservation {
                    definition_hash: "child".into(),
                    op: "sleep".into()
                },
                loom_proto::TraceObservation {
                    definition_hash: "parent".into(),
                    op: "sleep".into()
                },
            ]
        );
        Ok(())
    }
    #[tokio::test]
    async fn concurrent_execution_of_one_scope_publishes_one_observation() -> Result<()> {
        let store = Store::memory()?;
        let runtime = Runtime::new(store.clone())?;
        let descriptor = json!({"op":"random"});
        let outputs = futures::future::try_join_all(
            (0..8).map(|_| runtime.perform(descriptor.clone(), "same-scope", 0)),
        )
        .await?;
        assert!(outputs.iter().all(|output| output == &outputs[0]));
        let events = store.events(Some("system"), 0, 100)?;
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event["type"] == "call_completed")
                .count(),
            1
        );
        Ok(())
    }
    #[tokio::test]
    async fn fresh_observations_do_not_accumulate_global_locks_or_effect_rows() -> Result<()> {
        let store = Store::memory()?;
        let runtime = Runtime::new(store.clone())?;
        for index in 0..8 {
            runtime
                .perform(json!({"op":"random"}), &format!("fresh-{index}"), 0)
                .await?;
        }
        assert!(runtime.inner.effect_locks.lock().unwrap().is_empty());
        let events = store.events(Some("system"), 0, 1000)?;
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event["type"] == "call_completed")
                .count(),
            8
        );
        assert!(
            events
                .iter()
                .all(|event| event.event["type"] != "effect_recorded")
        );
        Ok(())
    }
    #[test]
    fn shared_memory_is_rejected_by_the_execution_engine() -> Result<()> {
        let runtime = Runtime::new(Store::memory()?)?;
        Component::new(
            &runtime.inner.engine,
            "(component (core module (memory 1)))",
        )?;
        assert!(
            Component::new(
                &runtime.inner.engine,
                "(component (core module (memory 1 1 shared)))"
            )
            .is_err()
        );
        Ok(())
    }
    #[test]
    fn real_component_compiles_and_instantiates() -> Result<()> {
        let runtime = Runtime::new(Store::memory()?)?;
        let component = Component::new(&runtime.inner.engine, "(component)")?;
        let mut store = wasmtime::Store::new(&runtime.inner.engine, ());
        let linker = Linker::new(&runtime.inner.engine);
        let execution = tokio::runtime::Runtime::new()?;
        execution.block_on(linker.instantiate_async(&mut store, &component))?;
        Ok(())
    }
}
