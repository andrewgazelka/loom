mod machine;
use anyhow::{Context, Result, bail};
use loom_proto::{Actor, Value};
use loom_store::Store;
use serde_json::json;
use std::{
    collections::{BTreeSet, HashMap},
    sync::{Arc, Mutex},
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
    components: Mutex<HashMap<String, HandlerPre<ContextData>>>,
    component_locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    actor_locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    effect_locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    fibers: Mutex<HashMap<String, FiberTask>>,
}
struct FiberTask {
    scope: String,
    task: tokio::task::JoinHandle<Result<Value>>,
}
impl Drop for FiberTask {
    fn drop(&mut self) {
        self.task.abort();
    }
}
#[derive(Clone, Default)]
struct EffectContext {
    def_hash: Option<String>,
    actor_id: Option<String>,
    allowed: Option<BTreeSet<String>>,
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
        }
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
impl Drop for ContextData {
    fn drop(&mut self) {
        if let Ok(mut fibers) = self.runtime.inner.fibers.lock() {
            let prefix = format!("{}/", self.scope);
            fibers
                .retain(|_, fiber| fiber.scope != self.scope && !fiber.scope.starts_with(&prefix));
        }
    }
}
impl loom::host::abilities::Host for ContextData {
    async fn perform(&mut self, desc: Vec<u8>) -> std::result::Result<Vec<u8>, String> {
        if self.pure {
            return Err("fold cannot perform effects".into());
        }
        let mut desc = decode(&desc).map_err(|e| e.to_string())?;
        resolve_self(&mut desc, &self.def_hash);
        let occurrence = self.occurrence;
        self.occurrence += 1;
        let result = self
            .runtime
            .perform_contextual(desc, &self.scope, occurrence, self.effects.clone())
            .await
            .map_err(|e| format!("{e:#}"))?;
        encode(&result).map_err(|e| e.to_string())
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
    pub value: Value,
    pub timing: RuntimeTiming,
}
fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}
struct Instance {
    timing: RuntimeTiming,
    bindings: Handler,
    store: wasmtime::Store<ContextData>,
}
impl Runtime {
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
                components: Mutex::new(HashMap::new()),
                component_locks: Mutex::new(HashMap::new()),
                actor_locks: Mutex::new(HashMap::new()),
                effect_locks: Mutex::new(HashMap::new()),
                fibers: Mutex::new(HashMap::new()),
            }),
        };
        let weak = Arc::downgrade(&runtime.inner);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(10));
                let Some(inner) = weak.upgrade() else { break };
                inner.engine.increment_epoch();
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
            .definition(hash)?
            .context("definition not found")?;
        let effects = parent.delegated(hash, def.allowed_effects.as_deref());
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
                    .definition(hash)?
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
    ) -> Result<Value> {
        Ok(self
            .call_scoped_delegated(hash, args, scope, effects)
            .await?
            .value)
    }
    async fn call_scoped_timed(&self, hash: &str, args: Value, scope: &str) -> Result<TimedCall> {
        self.call_scoped_delegated(hash, args, scope, EffectContext::default())
            .await
    }
    async fn call_scoped_delegated(
        &self,
        hash: &str,
        args: Value,
        scope: &str,
        effects: EffectContext,
    ) -> Result<TimedCall> {
        let call_start = Instant::now();
        let mut instance = self
            .instance_delegated(hash, scope, false, &effects)
            .await?;
        let run_start = Instant::now();
        let result = instance
            .bindings
            .call_call(&mut instance.store, &encode(&json!(hash))?, &encode(&args)?)
            .await?
            .map_err(anyhow::Error::msg)?;
        let value = decode(&result)?;
        instance.timing.run_ms = elapsed_ms(run_start);
        instance.timing.total_ms = elapsed_ms(call_start);
        Ok(TimedCall {
            value,
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
        self.instance(hash, "spawn", true).await?;
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
        let mut instance = self
            .instance(
                &metadata.behavior_hash,
                &format!("{actor}:{handler_seq}"),
                false,
            )
            .await?;
        instance.store.data_mut().effects.actor_id = Some(actor.clone());
        let bytes = instance
            .bindings
            .call_run(
                &mut instance.store,
                &encode(&state)?,
                &encode(&message.msg)?,
            )
            .await?
            .map_err(anyhow::Error::msg)?;
        let events = decode(&bytes)?
            .as_array()
            .context("run must return array of events")?
            .clone();
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
        self.instance(hash, "upgrade", true).await?;
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
        self.perform_contextual(desc, scope, occurrence, EffectContext::default())
    }
    fn perform_contextual<'a>(
        &'a self,
        desc: Value,
        scope: &'a str,
        occurrence: i64,
        effects: EffectContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value>> + Send + 'a>> {
        Box::pin(async move {
            let op = desc
                .get("op")
                .and_then(Value::as_str)
                .context("descriptor op required")?;
            let hash = self.inner.store.put_value("desc", &desc)?;
            if !effects.permits(op) {
                self.inner.store.append("system", &json!({"type":"effect_denied","desc_hash":hash,"def_hash":effects.def_hash,"actor_id":effects.actor_id,"op":op,"scope":scope,"occurrence":occurrence}), 0)?;
                bail!(
                    "effect {op} is not allowed for definition {}",
                    effects.def_hash.as_deref().unwrap_or("<host>")
                );
            }
            let args = desc.get("args").cloned().unwrap_or(Value::Null);
            self.inner.store.append("system", &json!({"type":"effect_invoked","def_hash":effects.def_hash,"actor_id":effects.actor_id,"op":op,"desc_hash":hash,"scope":scope,"occurrence":occurrence}), 0)?;
            let mut cached = false;
            let outcome: Result<Value> = async {
            match op {
                "all" | "race" => {
                    let descs = args
                        .get("descs")
                        .and_then(Value::as_array)
                        .context("descs required")?;
                    let child_scope = format!("{scope}/{op}:{occurrence}");
                    let futures = descs
                        .iter()
                        .enumerate()
                        .map(|(index, desc)| {
                            self.perform_contextual(
                                desc.clone(),
                                &child_scope,
                                index as i64,
                                effects.clone(),
                            )
                        })
                        .collect::<Vec<_>>();
                    if op == "all" {
                        return Ok(Value::Array(futures::future::try_join_all(futures).await?));
                    }
                    if futures.is_empty() {
                        bail!("race requires at least one descriptor");
                    }
                    return futures::future::select_all(futures).await.0;
                }
                "call" => {
                    return self
                        .call_scoped(
                            required_str(&args, "def")?,
                            args.get("args").cloned().unwrap_or(Value::Null),
                            &format!("{scope}/call:{occurrence}"),
                            effects.clone(),
                        )
                        .await;
                }
                "fork" => {
                    let hash = required_str(&args, "def")?.to_owned();
                    let args = args.get("args").cloned().unwrap_or(Value::Null);
                    let runtime = self.clone();
                    let id = uuid::Uuid::new_v4().to_string();
                    let child_scope = format!("{scope}/fork:{occurrence}");
                    let child_effects = effects.clone();
                    let task = tokio::spawn(async move {
                        runtime
                            .call_scoped(&hash, args, &child_scope, child_effects)
                            .await
                    });
                    self.inner.fibers.lock().unwrap().insert(
                        id.clone(),
                        FiberTask {
                            scope: scope.into(),
                            task,
                        },
                    );
                    return Ok(json!(id));
                }
                "join" => {
                    let mut out = Vec::new();
                    for id in args
                        .get("fibers")
                        .and_then(Value::as_array)
                        .context("fibers required")?
                    {
                        let mut task = self
                            .inner
                            .fibers
                            .lock()
                            .unwrap()
                            .remove(id.as_str().context("fiber id must be string")?)
                            .context("unknown or already joined fiber")?;
                        out.push((&mut task.task).await??);
                    }
                    return Ok(json!(out));
                }
                "send" => {
                    let key = format!("{scope}:{occurrence}:{hash}");
                    let message = self.inner.store.enqueue_once(
                        required_str(&args, "actor")?,
                        &args.get("msg").cloned().unwrap_or(Value::Null),
                        &key,
                    )?;
                    return self.schedule_message(message);
                }
                "spawn" => {
                    let key = format!("spawn:{scope}:{occurrence}:{hash}");
                    let lock = self
                        .inner
                        .effect_locks
                        .lock()
                        .unwrap()
                        .entry(key.clone())
                        .or_default()
                        .clone();
                    let _guard = lock.lock().await;
                    if let Some(result) = self.inner.store.effect_get(&hash, scope, occurrence)? {
                        cached = true;
                        return Ok(result);
                    }
                    let actor_id = blake3::hash(key.as_bytes()).to_hex().to_string();
                    let actor = self
                        .spawn_identified(
                            required_str(&args, "def")?,
                            args.get("state").cloned().unwrap_or(Value::Null),
                            actor_id,
                        )
                        .await?;
                    let result = serde_json::to_value(actor)?;
                    self.inner
                        .store
                        .effect_put(&hash, scope, occurrence, &result)?;
                    return Ok(result);
                }
                _ => {}
            }
            let class = match op {
                "cas.get" | "cas.put" => "hermetic",
                "exec" if args.get("tree").and_then(Value::as_str).is_some() => "hermetic",
                "exec" if args.get("key").and_then(Value::as_str).is_some() => "keyed",
                _ => "observational",
            };
            let cache_scope = if class == "hermetic" || class == "keyed" {
                "global"
            } else {
                scope
            };
            let cache_occurrence = if class == "hermetic" || class == "keyed" {
                0
            } else {
                occurrence
            };
            let key = format!("{hash}:{cache_scope}:{cache_occurrence}");
            let lock = self
                .inner
                .effect_locks
                .lock()
                .unwrap()
                .entry(key)
                .or_default()
                .clone();
            let _guard = lock.lock().await;
            if let Some(result) =
                self.inner
                    .store
                    .effect_get(&hash, cache_scope, cache_occurrence)?
            {
                cached = true;
                return Ok(result);
            }
            let result = match op {
                "llm" => serde_json::to_value(
                    self.inner
                        .model
                        .complete(serde_json::from_value(args.clone())?)
                        .await?,
                )?,
                "sleep" => {
                    let ms = args
                        .get("ms")
                        .and_then(Value::as_u64)
                        .context("ms required")?;
                    if ms > 86_400_000 {
                        bail!("sleep exceeds one day limit");
                    }
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    Value::Null
                }
                "now" => json!(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_millis()
                ),
                "random" => json!(
                    (uuid::Uuid::new_v4().as_u128() as u64 & ((1u64 << 53) - 1)) as f64
                        / ((1u64 << 53) as f64)
                ),
                "cas.put" => {
                    let hash = self.inner.store.put_value("blob", &args)?;
                    self.inner
                        .store
                        .reference(&hash, loom_proto::DAG_CBOR_CODEC)?
                }
                "cas.get" => self
                    .inner
                    .store
                    .get_value(required_str(&args, "hash")?)?
                    .context("CAS value not found")?,
                "exec" if args.get("tree").is_some() => self.hermetic_exec(&args).await?,
                "exec" => {
                    let program = required_str(&args, "program")?;
                    let mut command = tokio::process::Command::new(program);
                    if let Some(arguments) = args.get("args").and_then(Value::as_array) {
                        for argument in arguments {
                            command.arg(
                                argument
                                    .as_str()
                                    .context("exec arguments must be strings")?,
                            );
                        }
                    }
                    self.execute_command(command, capture_paths(&args)?).await?
                }
                "fs.snapshot" => self.snapshot_tree(&args).await?,
                "fs.read" => self.read_machine_file(&args).await?,
                "fs.stat" => {
                    let metadata = tokio::fs::metadata(self.machine_path(&args)?).await?;
                    json!({"size":metadata.len(),"is_dir":metadata.is_dir(),"is_file":metadata.is_file()})
                }
                "fs.list" => self.list_machine_directory(&args).await?,
                _ => bail!("unsupported ability: {op}"),
            };
            self.inner
                .store
                .effect_put(&hash, cache_scope, cache_occurrence, &result)?;
            Ok(result)
            }.await;
            let result_hash = outcome
                .as_ref()
                .ok()
                .map(|result| self.inner.store.put_value("result", result))
                .transpose()?;
            self.inner.store.append("system", &json!({"type":"effect_completed","def_hash":effects.def_hash,"actor_id":effects.actor_id,"op":op,"desc_hash":hash,"scope":scope,"occurrence":occurrence,"cached":cached,"result_hash":result_hash,"error":outcome.as_ref().err().map(|error|format!("{error:#}"))}), 0)?;
            outcome
        })
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
    match value.get("op").and_then(Value::as_str) {
        Some("call" | "fork" | "spawn") => {
            if value.pointer("/args/def").and_then(Value::as_str) == Some("$self") {
                value["args"]["def"] = json!(hash);
            }
        }
        Some("all" | "race") => {
            if let Some(descs) = value
                .pointer_mut("/args/descs")
                .and_then(Value::as_array_mut)
            {
                for desc in descs {
                    resolve_self(desc, hash);
                }
            }
        }
        _ => {}
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
        assert_ne!(first, runtime.perform(descriptor, "handler-1", 1).await?);
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
    async fn all_records_independent_occurrences() -> Result<()> {
        let runtime = Runtime::new(Store::memory()?)?;
        let descriptor = json!({"op":"all","args":{"descs":[{"op":"random"},{"op":"random"}]}});
        let first = runtime.perform(descriptor.clone(), "a", 0).await?;
        assert_ne!(first[0], first[1]);
        assert_eq!(first, runtime.perform(descriptor, "a", 0).await?);
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
    async fn race_returns_first_failure_and_cancels_sleeper() -> Result<()> {
        let runtime = Runtime::new(Store::memory()?)?;
        let result=tokio::time::timeout(Duration::from_millis(500),runtime.perform(json!({"op":"race","args":{"descs":[{"op":"unsupported"},{"op":"sleep","args":{"ms":5000}}]}}),"race",0)).await?;
        assert!(result.is_err());
        Ok(())
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn race_cancellation_terminates_process_descendants() -> Result<()> {
        let root = tempfile::tempdir()?;
        let ready = root.path().join("ready");
        let orphan = root.path().join("orphan");
        let runtime = Runtime::new(Store::memory()?)?;
        let desc = json!({"op":"race","args":{"descs":[{"op":"exec","args":{"program":"sh","args":["-c","printf ready > \"$1\"; (sleep 1; printf orphan > \"$2\") & wait","loom",ready,orphan]}},{"op":"sleep","args":{"ms":200}}]}});
        assert_eq!(runtime.perform(desc, "cancel", 0).await?, Value::Null);
        assert!(
            ready.exists(),
            "process never started; cancellation control invalid"
        );
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(!orphan.exists(), "descendant survived canceled effect");
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
    async fn policy_denies_dynamic_and_cached_requests_and_nested_combinators() -> Result<()> {
        let runtime = Runtime::new(Store::memory()?)?;
        let desc = json!({"op":"cas.put","args":{"secret":42}});
        runtime.perform(desc.clone(), "warm", 0).await?;
        let denied = EffectContext::default().delegated("restricted", Some(&[]));
        let error = runtime
            .perform_contextual(desc.clone(), "denied", 0, denied.clone())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not allowed"));
        let parent =
            EffectContext::default().delegated("parent", Some(&["all".into(), "call".into()]));
        let child = parent.delegated("child", Some(&["all".into(), "cas.put".into()]));
        assert!(!child.permits("cas.put"));
        assert!(child.permits("all"));
        assert!(
            !parent
                .delegated("unrestricted-child", None)
                .permits("cas.put")
        );
        let nested = json!({"op":"all","args":{"descs":[desc]}});
        assert!(
            runtime
                .perform_contextual(nested, "nested", 0, child)
                .await
                .is_err()
        );
        let events = runtime.inner.store.events(Some("system"), 0, 1000)?;
        assert!(
            events
                .iter()
                .any(|event| event.event["type"] == "effect_denied")
        );
        assert!(
            !events
                .iter()
                .any(|event| event.event["type"] == "effect_invoked"
                    && event.event["def_hash"] == "restricted")
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
