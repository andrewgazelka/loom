//! Shared linear memory belongs to one execution. Stores never borrow host slices
//! of that memory, and aborted guest stacks remain allocated until every task ends.
use super::*;
use futures::{
    FutureExt,
    future::{AbortHandle, Abortable, RemoteHandle},
    task::SpawnExt,
};
use std::sync::atomic::{AtomicBool, AtomicU8};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use wasmtime::{
    Caller, ExternType, Instance as CoreInstance, Module, SharedMemory, UpdateDeadline,
};

mod cancellation;
mod timing;
mod handlers;
use handlers::{HandlerFrame, ContinuationState, HandlerInstance};

const MAX_JOBS: usize = 512;
const STACK_BYTES: u32 = 256 * 1024;
const MAX_MEMORY: u64 = 256 * 1024 * 1024;
const EXECUTION_SECONDS: u64 = 30;

pub(super) fn engine() -> Result<Engine> {
    let mut config = Config::new();
    config
        .wasm_threads(true)
        .shared_memory(true)
        .epoch_interruption(true);
    Engine::new(&config).map_err(|e| anyhow::anyhow!("{e:#}"))
}
// Keep the classification while sharing a failure with cancelled sibling tasks.
#[derive(Clone)]
struct ExecutionFailure {
    message: String,
    guest: bool,
}
impl ExecutionFailure {
    fn new(error: anyhow::Error) -> Self {
        Self { guest: error.is::<GuestFailure>(), message: format!("{error:#}") }
    }
    fn into_error(self) -> anyhow::Error {
        if self.guest { GuestFailure::new(self.message).into() }
        else { anyhow::anyhow!(self.message) }
    }
}
struct Job {
    handlers: Vec<u64>,
    parent: String,
    detached: bool,
    result: Mutex<Option<std::result::Result<(), ExecutionFailure>>>,
    done: Notify,
}
struct ScheduledTask {
    abort: AbortHandle,
    completion: RemoteHandle<()>,
}
struct Execution {
    handler_instances: Mutex<Vec<HandlerInstance>>,
    handler_instance_reuses: AtomicU64,
    handler_round_trip_us: Mutex<Vec<f64>>,
    handlers_next: AtomicU64,
    continuations: Mutex<HashMap<u64, Arc<ContinuationState>>>,
    handler_failure: Mutex<Option<ExecutionFailure>>,
    runtime: Runtime,
    module: Module,
    memory: SharedMemory,
    effects: EffectContext,
    pure: bool,
    jobs: Mutex<HashMap<u64, Arc<Job>>>,
    tasks: Mutex<Vec<ScheduledTask>>,
    job_count: AtomicU64,
    initialization: AsyncMutex<()>,
    permits: Arc<Semaphore>,
    cancelled: AtomicBool,
    cancellation: Notify,
    deadline: Instant,
}
struct Guest {
    execution: Arc<Execution>,
    scope: String,
    handlers: Vec<Arc<HandlerFrame>>,
    occurrence: i64,
    last_effect_error: Option<String>,
    permit: Option<OwnedSemaphorePermit>,
}
struct Allocation {
    stack: u32,
    tls: u32,
}
struct Running {
    store: wasmtime::Store<Guest>,
    instance: CoreInstance,
}
struct Cleanup {
    execution: Option<Arc<Execution>>,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        let Some(execution) = self.execution.take() else {
            return;
        };
        execution.cancel();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                execution.drain().await;
            });
        }
    }
}
impl Execution {
    fn original_failure(&self) -> Option<ExecutionFailure> {
        self.handler_failure.lock().unwrap().clone().or_else(|| {
            self.jobs.lock().unwrap().values().find_map(|job| {
                if job.detached { return None; }
                job.result.lock().unwrap().as_ref().and_then(|result| result.as_ref().err()).cloned()
            })
        })
    }
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        // Parked Stores are not executing. Drop them synchronously to break
        // Execution -> cached Store -> Execution ownership cycles.
        self.handler_instances.lock().unwrap().clear();
        self.cancellation.notify_waiters();
        self.runtime.inner.core_engine.increment_epoch();
    }
    fn check(&self) -> Result<()> {
        anyhow::ensure!(
            !self.cancelled.load(Ordering::Acquire),
            "shared execution cancelled"
        );
        anyhow::ensure!(
            Instant::now() < self.deadline,
            "shared execution deadline exceeded"
        );
        Ok(())
    }
    fn schedule(
        self: &Arc<Self>,
        future: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Result<()> {
        let runtime = tokio::runtime::Handle::current();
        let (abort, registration) = AbortHandle::new_pair();
        let mut future = Box::pin(future);
        let entered = futures::future::poll_fn(move |context| {
            let _entered = runtime.enter();
            future.as_mut().poll(context)
        });
        let execution = Arc::downgrade(self);
        let completion = self
            .runtime
            .inner
            .core_executor
            .spawn_with_handle(async move {
                let mut abortable = Box::pin(Abortable::new(entered, registration));
                let outcome = std::panic::AssertUnwindSafe(abortable.as_mut())
                    .catch_unwind()
                    .await;
                // Destruction precedes completion publication, including abort paths.
                drop(abortable);
                if outcome.is_err() {
                    if let Some(execution) = execution.upgrade() {
                        execution.cancel();
                    }
                }
            })?;
        if self.cancelled.load(Ordering::Acquire) {
            abort.abort();
        }
        self.tasks
            .lock()
            .unwrap()
            .push(ScheduledTask { abort, completion });
        Ok(())
    }
    async fn drain(&self) {
        loop {
            let tasks = std::mem::take(&mut *self.tasks.lock().unwrap());
            if tasks.is_empty() {
                break;
            }
            for task in &tasks {
                task.abort.abort();
            }
            // Remote completion acknowledges destruction of the actual guest
            // future and its Store, including a currently executing Wasm call.
            for task in tasks {
                task.completion.await;
            }
        }
        // Also covers a callback that raced cancellation before parking.
        self.handler_instances.lock().unwrap().clear();
    }
    async fn permit(&self) -> Result<OwnedSemaphorePermit> {
        let cancelled = self.cancellation.notified();
        self.check()?;
        tokio::select! {
            result = self.permits.clone().acquire_owned() => Ok(result?),
            _ = cancelled => bail!("shared execution cancelled"),
            _ = tokio::time::sleep_until(self.deadline.into()) => bail!("shared execution deadline exceeded"),
        }
    }
    async fn instantiate(
        self: &Arc<Self>,
        scope: String,
        allocation: Option<Allocation>,
        handlers: Vec<Arc<HandlerFrame>>,
    ) -> Result<Running> {
        let permit = self.permit().await?;
        let mut store = wasmtime::Store::new(
            &self.runtime.inner.core_engine,
            Guest {
                execution: self.clone(),
                scope,
                handlers,
                occurrence: 0,
                last_effect_error: None,
                permit: Some(permit),
            },
        );
        store.set_epoch_deadline(1);
        store.epoch_deadline_callback(|context| {
            let execution = &context.data().execution;
            Ok(if execution.check().is_err() {
                UpdateDeadline::Interrupt
            } else {
                UpdateDeadline::Continue(1)
            })
        });
        let linker = linker(self, &store)?;
        let initialization = self.initialization.lock().await;
        let instance = linker
            .instantiate_async(&mut store, &self.module)
            .await
            .map_err(error)?;
        if let Some(allocation) = allocation {
            instance
                .get_global(&mut store, "__loom_stack_low")
                .context("missing stack lower bound")?
                .set(
                    &mut store,
                    wasmtime::Val::I32((allocation.stack - STACK_BYTES) as i32),
                )
                .map_err(error)?;
            instance
                .get_global(&mut store, "__loom_stack_high")
                .context("missing stack upper bound")?
                .set(&mut store, wasmtime::Val::I32(allocation.stack as i32))
                .map_err(error)?;
            instance
                .get_global(&mut store, "__stack_pointer")
                .context("missing mutable guest stack pointer")?
                .set(&mut store, wasmtime::Val::I32(allocation.stack as i32))
                .map_err(error)?;
            instance
                .get_typed_func::<i32, ()>(&mut store, "__wasm_init_tls")
                .map_err(error)?
                .call_async(&mut store, allocation.tls as i32)
                .await
                .map_err(error)?;
        }
        let low = instance
            .get_global(&mut store, "__loom_stack_low")
            .context("core lacks instrumented stack lower bound")?
            .get(&mut store)
            .i32()
            .context("invalid stack lower bound")? as u32;
        let high = instance
            .get_global(&mut store, "__loom_stack_high")
            .context("core lacks instrumented stack upper bound")?
            .get(&mut store)
            .i32()
            .context("invalid stack upper bound")? as u32;
        let pointer = instance
            .get_global(&mut store, "__stack_pointer")
            .context("core lacks stack pointer")?
            .get(&mut store)
            .i32()
            .context("invalid stack pointer")? as u32;
        anyhow::ensure!(
            low <= pointer && pointer <= high && high as usize <= self.memory.data_size(),
            "invalid core stack bounds"
        );
        drop(initialization);
        Ok(Running { store, instance })
    }
}
enum Invocation {
    Call { args: Vec<u8> },
    Run { state: Vec<u8>, message: Vec<u8> },
    Fold { state: Vec<u8>, event: Vec<u8> },
    Validate,
    Schema,
}
impl Entry<'_> {
    fn prepare(self) -> Result<Invocation> {
        Ok(match self {
            Self::Call { args } => Invocation::Call {
                args: encode(args)?,
            },
            Self::Run { state, message } => Invocation::Run {
                state: encode(state)?,
                message: encode(message)?,
            },
            Self::Fold { state, event } => Invocation::Fold {
                state: encode(state)?,
                event: encode(event)?,
            },
            Self::Validate => Invocation::Validate,
            Self::Schema => Invocation::Schema,
        })
    }
}
impl Execution {
    async fn invoke(
        self: &Arc<Self>,
        scope: String,
        invocation: Invocation,
    ) -> Result<EffectOutput> {
        let mut running = self.instantiate(scope, None, Vec::new()).await?;
        let is_run = matches!(invocation, Invocation::Run { .. });
        let packed = match invocation {
            Invocation::Validate => return EffectOutput::value(&Value::Null),
            Invocation::Schema => {
                if running.instance.get_export(&mut running.store, "loom_schema").is_none() {
                    return EffectOutput::value(&json!(""));
                }
                running.instance.get_typed_func::<(), i64>(&mut running.store, "loom_schema")
                    .map_err(error)?.call_async(&mut running.store, ()).await
                    .map_err(|cause| running.error_context(cause))? as u64
            }
            Invocation::Call { args } => {
                let buffer = running.input_encoded(&args).await?;
                running
                    .instance
                    .get_typed_func::<(i32, i32), i64>(&mut running.store, "loom_call")
                    .map_err(|cause| running.error_context(cause))?
                    .call_async(&mut running.store, (buffer.pointer, buffer.length))
                    .await
                    .map_err(|cause| running.error_context(cause))? as u64
            }
            Invocation::Run { state, message }
            | Invocation::Fold {
                state,
                event: message,
            } => {
                let name = if is_run { "loom_run" } else { "loom_fold" };
                let state = running.input_encoded(&state).await?;
                let message = running.input_encoded(&message).await?;
                running
                    .instance
                    .get_typed_func::<(i32, i32, i32, i32), i64>(&mut running.store, name)
                    .map_err(|cause| running.error_context(cause))?
                    .call_async(
                        &mut running.store,
                        (state.pointer, state.length, message.pointer, message.length),
                    )
                    .await
                    .map_err(|cause| running.error_context(cause))? as u64
            }
        };
        let bytes = copy_out(&self.memory, packed as u32, (packed >> 32) as u32)
            .map_err(|error| GuestFailure::new(format!("{error:#}")))?;
        let envelope: Value = loom_proto::decode(&bytes).map_err(GuestFailure::new)?;
        if !envelope.as_object().is_some_and(|object| object.len() == 1) {
            return Err(GuestFailure::new("invalid core result envelope").into());
        }
        if let Some(error) = envelope.get("error").and_then(Value::as_str) {
            return Err(GuestFailure::new(error).into());
        }
        self.check()?;
        let output = envelope.get("ok").ok_or_else(|| GuestFailure::new("invalid core result envelope"))?;
        anyhow::ensure!(
            self.jobs
                .lock()
                .unwrap()
                .values()
                .all(|job| job.detached || job.result.lock().unwrap().is_some()),
            "core returned with unfinished scoped jobs"
        );
        EffectOutput::value(output)
    }
}
fn error(error: wasmtime::Error) -> anyhow::Error {
    call::wasm_error(error)
}
fn host_error(error: anyhow::Error) -> wasmtime::Error {
    wasmtime::Error::from_anyhow(error)
}

fn copy_out(memory: &SharedMemory, pointer: u32, length: u32) -> Result<Vec<u8>> {
    anyhow::ensure!(
        length as usize <= loom_proto::TRACE_MAX_BLOB_BYTES,
        "guest message exceeds byte limit"
    );
    let start = pointer as usize;
    let end = start
        .checked_add(length as usize)
        .context("guest range overflow")?;
    let data = memory.data();
    anyhow::ensure!(end <= data.len(), "guest message outside shared memory");
    Ok(data[start..end]
        .iter()
        .map(|cell| {
            // Atomic accesses are required even when the guest claims exclusive ownership.
            unsafe { AtomicU8::from_ptr(cell.get()).load(Ordering::Relaxed) }
        })
        .collect())
}
fn copy_in(memory: &SharedMemory, pointer: u32, bytes: &[u8]) -> Result<()> {
    let start = pointer as usize;
    let end = start
        .checked_add(bytes.len())
        .context("guest range overflow")?;
    anyhow::ensure!(
        bytes.len() <= loom_proto::TRACE_MAX_BLOB_BYTES,
        "guest message exceeds byte limit"
    );
    let data = memory.data();
    anyhow::ensure!(end <= data.len(), "guest allocation outside shared memory");
    for (cell, byte) in data[start..end].iter().zip(bytes) {
        unsafe {
            AtomicU8::from_ptr(cell.get()).store(*byte, Ordering::Relaxed);
        }
    }
    Ok(())
}
/// Wasm exports alignment zero when there is no TLS block. No allocation is
/// made in that case; use alignment one for bookkeeping and checked arithmetic.
fn tls_alignment(size: u32, alignment: u32) -> Result<u32> {
    if size == 0 { return Ok(1); }
    anyhow::ensure!(alignment.is_power_of_two() && alignment <= 65536, "invalid guest TLS alignment");
    Ok(alignment)
}
async fn allocate(caller: &mut Caller<'_, Guest>, size: u32, align: u32) -> Result<u32> {
    let function = caller
        .get_export("loom_alloc")
        .and_then(|e| e.into_func())
        .context("missing loom_alloc")?
        .typed::<(i32, i32), i32>(&*caller)
        .map_err(error)?;
    let pointer = function
        .call_async(&mut *caller, (size as i32, align as i32))
        .await
        .map_err(error)? as u32;
    anyhow::ensure!(size == 0 || pointer != 0, "guest allocation failed");
    anyhow::ensure!(
        align.is_power_of_two() && pointer % align == 0,
        "guest allocation has invalid alignment"
    );
    anyhow::ensure!(
        (pointer as u64 + size as u64) <= caller.data().execution.memory.data_size() as u64,
        "guest allocation outside memory"
    );
    Ok(pointer)
}
async fn respond(caller: &mut Caller<'_, Guest>, bytes: Vec<u8>) -> Result<i64> {
    let pointer = allocate(caller, bytes.len().try_into()?, 1).await?;
    copy_in(&caller.data().execution.memory, pointer, &bytes)?;
    Ok(((bytes.len() as u64) << 32 | pointer as u64) as i64)
}
fn linker(
    execution: &Arc<Execution>,
    store: &wasmtime::Store<Guest>,
) -> Result<wasmtime::Linker<Guest>> {
    let mut linker = wasmtime::Linker::new(&execution.runtime.inner.core_engine);
    linker
        .define(store, "env", "memory", execution.memory.clone())
        .map_err(error)?;
    linker
        .func_wrap_async(
            "loom",
            "perform",
            |mut caller: Caller<'_, Guest>, (pointer, length): (i32, i32)| {
                Box::new(async move {
                    let result: Result<i64> = async {
                        let started = Instant::now();
                        caller.data_mut().last_effect_error = None;
                        let execution = caller.data().execution.clone();
                        let bytes = copy_out(&execution.memory, pointer as u32, length as u32)?;
                        let mut descriptor =
                            loom_proto::decode(&bytes).map_err(anyhow::Error::msg)?;
                        if let Some(definition) = &execution.effects.def_hash {
                            resolve_self(&mut descriptor, definition);
                        }
                        let occurrence = caller.data().occurrence;
                        caller.data_mut().occurrence += 1;
                        if let Some(bytes) = handlers::dispatch(&mut caller, &descriptor, occurrence).await? {
                            let response = respond(&mut caller, bytes).await?;
                            let mut samples = execution.handler_round_trip_us.lock().unwrap();
                            if samples.len() == 100_000 { samples.drain(..50_000); }
                            samples.push(started.elapsed().as_secs_f64() * 1_000_000.0);
                            return Ok(response);
                        }
                        anyhow::ensure!(!execution.pure, "effects forbidden in pure core execution");
                        let scope = caller.data().scope.clone();
                        let effects = execution.effects.clone();
                        caller.data_mut().permit.take();
                        let output = execution
                            .runtime
                            .dispatch_root(
                                descriptor,
                                &scope,
                                occurrence,
                                effects,
                            )
                            .await;
                        caller.data_mut().permit.take();
                        caller.data_mut().permit = Some(execution.permit().await?);
                        let bytes = match output {
                            Ok(output) => {
                                // The effect owner already produced canonical bytes.
                                let mut envelope = Vec::with_capacity(4 + output.bytes.len());
                                envelope.extend_from_slice(&[0xa1, 0x62, b'o', b'k']);
                                envelope.extend_from_slice(&output.bytes);
                                envelope
                            }
                            Err(error) => encode(&json!({"error":format!("{error:#}")}))?,
                        };
                        execution
                            .runtime
                            .inner
                            .effect_wire_bytes
                            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                        respond(&mut caller, bytes).await
                    }
                    .await;
                    result.map_err(host_error)
                })
            },
        )
        .map_err(error)?;
    linker
        .func_wrap_async(
            "loom",
            "spawn",
            |mut caller: Caller<'_, Guest>, (function, data, detached): (i32, i32, i32)| {
                Box::new(async move { spawn(&mut caller, function, data, detached).await.map_err(host_error) })
            },
        )
        .map_err(error)?;
    linker.func_wrap_async("loom", "join", |mut caller: Caller<'_, Guest>, (id,): (i64,)| Box::new(async move {
        let result: Result<i32> = async {
            let execution = caller.data().execution.clone();
            let job = execution.jobs.lock().unwrap().get(&(id as u64)).cloned().context("unknown shared job")?;
            anyhow::ensure!(job.detached || job.parent == caller.data().scope, "shared job belongs to another scope");
            caller.data_mut().permit.take();
            let status = loop {
                let notification = job.done.notified();
                let cancelled = execution.cancellation.notified();
                execution.check()?;
                let result = job.result.lock().unwrap().clone();
                if let Some(result) = result {
                    match result {
                        Ok(()) => break 0,
                        Err(_) if job.detached => break 1,
                        Err(failure) => return Err(failure.into_error()),
                    }
                }
                tokio::select! { _ = notification => {}, _ = cancelled => bail!("shared execution cancelled"), _ = tokio::time::sleep_until(execution.deadline.into()) => bail!("shared execution deadline exceeded") }
            };
            caller.data_mut().permit = Some(execution.permit().await?);
            Ok(status)
        }.await;
        result.map_err(host_error)
    })).map_err(error)?;
    linker.func_wrap_async("loom", "join_error", |mut caller: Caller<'_, Guest>, (id,): (i64,)| Box::new(async move {
        let result: Result<i64> = async {
            let job = caller.data().execution.jobs.lock().unwrap()
                .get(&(id as u64)).cloned().context("unknown shared job")?;
            anyhow::ensure!(job.detached, "join_error requires a detached job");
            let message = match job.result.lock().unwrap().as_ref() {
                Some(Err(failure)) => failure.message.clone(),
                _ => bail!("shared job has no error"),
            };
            respond(&mut caller, message.into_bytes()).await
        }.await;
        result.map_err(host_error)
    })).map_err(error)?;
    handlers::link(&mut linker)?;
    Ok(linker)
}
async fn spawn(caller: &mut Caller<'_, Guest>, function: i32, data: i32, detached: i32) -> Result<i64> {
    anyhow::ensure!(matches!(detached, 0 | 1), "invalid shared job detached flag");
    let detached = detached == 1;
    let execution = caller.data().execution.clone();
    execution.check()?;
    if execution
        .job_count
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            (count < MAX_JOBS as u64).then_some(count + 1)
        })
        .is_err()
    {
        return Ok(0);
    }
    let occurrence = caller.data().occurrence;
    caller.data_mut().occurrence += 1;
    let parent = caller.data().scope.clone();
    let scope = format!("{parent}/spawn:{occurrence}");
    let digest = blake3::hash(scope.as_bytes());
    let id = u64::from_le_bytes(digest.as_bytes()[..8].try_into().unwrap()) & i64::MAX as u64;
    {
        let jobs = execution.jobs.lock().unwrap();
        if jobs.len() >= MAX_JOBS {
            return Ok(0);
        }
        anyhow::ensure!(
            id != 0 && !jobs.contains_key(&id),
            "shared job identity collision"
        );
    }
    let stack = allocate(caller, STACK_BYTES, 16).await?;
    let tls_size = caller
        .get_export("__tls_size")
        .and_then(|e| e.into_global())
        .context("missing TLS size")?
        .get(&mut *caller)
        .i32()
        .context("invalid TLS size")? as u32;
    let tls_align = caller
        .get_export("__tls_align")
        .and_then(|e| e.into_global())
        .context("missing TLS alignment")?
        .get(&mut *caller)
        .i32()
        .context("invalid TLS alignment")? as u32;
    let tls_align = tls_alignment(tls_size, tls_align)?;
    let tls = if tls_size == 0 { 0 } else { allocate(caller, tls_size, tls_align).await? };
    // Handler frames can borrow the spawning stack. handle_pop drains every
    // inheriting job, including detached jobs, before freeing the frame data.
    let handlers = caller.data().handlers.clone();
    let job = Arc::new(Job {
        handlers: handlers.iter().map(|frame| frame.id).collect(),
        parent,
        detached,
        result: Mutex::new(None),
        done: Notify::new(),
    });
    execution.jobs.lock().unwrap().insert(id, job.clone());
    let child = execution.clone();
    let child_job = job.clone();
    execution.schedule(async move {
        let result: Result<()> = async {
            let mut running = child
                .instantiate(
                    scope.clone(),
                    Some(Allocation {
                        stack: stack + STACK_BYTES,
                        tls,
                    }),
                    handlers,
                )
                .await?;
            let task = running
                .instance
                .get_typed_func::<(i32, i32), ()>(&mut running.store, "loom_task_run")
                .map_err(error)?;
            task.call_async(&mut running.store, (function, data))
                .await
                .map_err(|cause| running.error_context(cause))?;
            Ok(())
        }
        .await;
        let result = result.map_err(|error| child.original_failure().unwrap_or_else(|| {
            ExecutionFailure::new(error.context(format!("shared job {scope}")))
        }));
        let cancel = result.is_err() && !child_job.detached;
        // Publish the cause before waking siblings through cancellation.
        *child_job.result.lock().unwrap() = Some(result);
        if cancel {
            child.cancel();
        }
        child_job.done.notify_waiters();
    })?;
    Ok(id as i64)
}

pub(super) enum Entry<'a> {
    Schema,
    Call {
        args: &'a Value,
    },
    Run {
        state: &'a Value,
        message: &'a Value,
    },
    Fold {
        state: &'a Value,
        event: &'a Value,
    },
    Validate,
}
impl Runtime {
    pub(super) async fn core_call(
        &self,
        hash: &str,
        args: &Value,
        scope: &str,
        effects: &EffectContext,
    ) -> Result<Option<EncodedCall>> {
        self.core_execute(hash, scope, effects, false, Entry::Call { args })
            .await
    }
    pub(super) async fn core_execute(
        &self,
        hash: &str,
        scope: &str,
        effects: &EffectContext,
        pure: bool,
        entry: Entry<'_>,
    ) -> Result<Option<EncodedCall>> {
        let start = Instant::now();
        let mut definition = self
            .inner
            .store
            .executable_definition(hash)?
            .context("definition not found")?;
        if definition.component_hash.is_none() {
            self.inner
                .resolver
                .as_ref()
                .context("definition has no built artifact")?
                .ensure_built(hash)
                .await?;
            definition = self
                .inner
                .store
                .executable_definition(hash)?
                .context("built definition disappeared")?;
        }
        let artifact = definition
            .component_hash
            .context("definition has no built artifact")?;
        if self
            .inner
            .components
            .lock()
            .unwrap()
            .contains_key(&artifact)
        {
            return Ok(None);
        }
        let compile_lock = self
            .inner
            .component_locks
            .lock()
            .unwrap()
            .entry(artifact.clone())
            .or_default()
            .clone();
        let compile_guard = compile_lock.lock().await;
        let cached = self
            .inner
            .core_modules
            .lock()
            .unwrap()
            .get(&artifact)
            .cloned();
        let module = if let Some(module) = cached {
            module
        } else {
            let bytes = self
                .inner
                .store
                .get(&artifact)?
                .context("artifact missing")?;
            if !loom_proto::component_protocol::is_core_current(&bytes) {
                return Ok(None);
            }
            let engine = self.inner.core_engine.clone();
            let module =
                tokio::task::spawn_blocking(move || Module::new(&engine, &bytes).map_err(error))
                    .await??;
            self.inner
                .core_modules
                .lock()
                .unwrap()
                .insert(artifact.clone(), module.clone());
            module
        };
        drop(compile_guard);
        let mut memory_type = None;
        for import in module.imports() {
            if let ExternType::Memory(ty) = import.ty() {
                anyhow::ensure!(
                    import.module() == "env" && import.name() == "memory" && memory_type.is_none(),
                    "core must import exactly env.memory"
                );
                anyhow::ensure!(
                    ty.is_shared()
                        && !ty.is_64()
                        && ty
                            .maximum()
                            .is_some_and(|pages| pages * 65536 <= MAX_MEMORY),
                    "core memory must be shared wasm32 with maximum 256MiB"
                );
                memory_type = Some(ty);
            }
        }
        let memory = SharedMemory::new(
            &self.inner.core_engine,
            memory_type.context("core has no shared memory import")?,
        )
        .map_err(error)?;
        let execution = Arc::new(Execution {
            handler_instances: Mutex::new(Vec::new()),
            handler_instance_reuses: AtomicU64::new(0),
            handler_round_trip_us: Mutex::new(Vec::new()),
            handlers_next: AtomicU64::new(1),
            continuations: Mutex::new(HashMap::new()),
            handler_failure: Mutex::new(None),
            runtime: self.clone(),
            module,
            memory,
            effects: effects.delegated(hash, definition.allowed_effects.as_deref()).with_declared(definition.sig.effects.declared.as_deref()),
            pure,
            jobs: Mutex::new(HashMap::new()),
            tasks: Mutex::new(Vec::new()),
            job_count: AtomicU64::new(0),
            initialization: AsyncMutex::new(()),
            permits: Arc::new(Semaphore::new(8)),
            cancelled: AtomicBool::new(false),
            cancellation: Notify::new(),
            deadline: Instant::now() + Duration::from_secs(EXECUTION_SECONDS),
        });
        let mut cleanup = Cleanup {
            execution: Some(execution.clone()),
        };
        let invocation = entry.prepare()?;
        let task_execution = execution.clone();
        let task_scope = scope.to_owned();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        execution.schedule(async move {
            let result = task_execution.invoke(task_scope, invocation).await;
            let _ = sender.send(result);
        })?;
        let result = receiver
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("shared root task ended without a result")));
        execution.cancel();
        execution.drain().await;
        if let Some(trace) = &execution.effects.trace {
            trace.finish_scope(scope);
        }
        cleanup.execution.take();
        *self.inner.handler_round_trip_us.lock().unwrap() = HandlerMeasurements {
            scope: scope.to_owned(),
            samples: std::mem::take(&mut *execution.handler_round_trip_us.lock().unwrap()),
        };
        let result = result.map_err(|error| {
            let failure = execution.original_failure();
            match failure {
                Some(failure) => failure.into_error(),
                None => error.context(format!("shared execution {scope}")),
            }
        });
        Ok(Some(EncodedCall {
            output: result?,
            timing: RuntimeTiming {
                component_hash: artifact,
                total_ms: elapsed_ms(start),
                run_ms: elapsed_ms(start),
                ..Default::default()
            },
        }))
    }
}

struct Buffer {
    pointer: i32,
    length: i32,
}
impl Running {
    fn error_context(&self, cause: wasmtime::Error) -> anyhow::Error {
        let cause = error(cause);
        match &self.store.data().last_effect_error {
            Some(message) => cause.context(format!("last returned guest effect error: {message}")),
            None => cause,
        }
    }
    async fn input_encoded(&mut self, bytes: &[u8]) -> Result<Buffer> {
        let length = bytes.len().try_into()?;
        let pointer = self
            .instance
            .get_typed_func::<(i32, i32), i32>(&mut self.store, "loom_alloc")
            .map_err(error)?
            .call_async(&mut self.store, (length, 1))
            .await
            .map_err(error)?;
        anyhow::ensure!(bytes.is_empty() || pointer != 0, "guest input allocation failed");
        copy_in(&self.store.data().execution.memory, pointer as u32, bytes)?;
        Ok(Buffer { pointer, length })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_tls_exports_need_no_alignment_but_nonempty_tls_is_checked() {
        assert_eq!(tls_alignment(0, 0).unwrap(), 1);
        assert_eq!(tls_alignment(16, 16).unwrap(), 16);
        assert!(tls_alignment(16, 0).is_err());
        assert!(tls_alignment(16, 3).is_err());
        assert!(tls_alignment(16, 131072).is_err());
    }
    #[test]
    fn shared_copy_checks_ranges_and_preserves_concurrent_atomic_access() {
        let engine = engine().unwrap();
        let memory = SharedMemory::new(&engine, wasmtime::MemoryType::shared(1, 2)).unwrap();
        copy_in(&memory, 32, &[1, 2, 3]).unwrap();
        assert_eq!(copy_out(&memory, 32, 3).unwrap(), vec![1, 2, 3]);
        assert!(copy_out(&memory, u32::MAX, 2).is_err());
        assert!(copy_in(&memory, 65535, &[1, 2]).is_err());
        let worker_memory = memory.clone();
        let worker = std::thread::spawn(move || {
            for _ in 0..1000 {
                copy_in(&worker_memory, 32, &[4, 5, 6]).unwrap();
            }
        });
        for _ in 0..1000 {
            assert_eq!(copy_out(&memory, 32, 3).unwrap().len(), 3);
        }
        worker.join().unwrap();
        assert_eq!(copy_out(&memory, 32, 3).unwrap(), vec![4, 5, 6]);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancellation_drains_saturated_guest_workers_before_releasing_execution() -> Result<()>
    {
        let runtime = Runtime::new(Store::memory()?)?;
        let module = Module::new(
            &runtime.inner.core_engine,
            r#"(module
            (import "env" "memory" (memory 1 1 shared))
            (global (export "__stack_pointer") (mut i32) (i32.const 65536))
            (global (export "__loom_stack_low") (mut i32) (i32.const 1024))
            (global (export "__loom_stack_high") (mut i32) (i32.const 65536))
            (func (export "spin") (param i32)
                local.get 0 i32.const 1 i32.atomic.store8
                (loop br 0)))"#,
        )
        .map_err(error)?;
        let memory = SharedMemory::new(
            &runtime.inner.core_engine,
            wasmtime::MemoryType::shared(1, 1),
        )
        .map_err(error)?;
        let execution = Arc::new(Execution {
            handler_instances: Mutex::new(Vec::new()),
            handler_instance_reuses: AtomicU64::new(0),
            handler_round_trip_us: Mutex::new(Vec::new()),
            handlers_next: AtomicU64::new(1),
            continuations: Mutex::new(HashMap::new()),
            handler_failure: Mutex::new(None),
            runtime,
            module,
            memory,
            effects: EffectContext::default(),
            pure: false,
            jobs: Mutex::new(HashMap::new()),
            tasks: Mutex::new(Vec::new()),
            job_count: AtomicU64::new(8),
            initialization: AsyncMutex::new(()),
            permits: Arc::new(Semaphore::new(8)),
            cancelled: AtomicBool::new(false),
            cancellation: Notify::new(),
            deadline: Instant::now() + Duration::from_secs(2),
        });
        for index in 0..8 {
            let child = execution.clone();
            execution.schedule(async move {
                let mut guest = child
                    .instantiate(format!("root:{index}"), None, Vec::new())
                    .await
                    .unwrap();
                let spin = guest
                    .instance
                    .get_typed_func::<i32, ()>(&mut guest.store, "spin")
                    .unwrap();
                let _ = spin.call_async(&mut guest.store, index).await;
            })?;
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            while copy_out(&execution.memory, 0, 8).unwrap() != [1; 8] {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        execution.cancel();
        tokio::time::timeout(Duration::from_secs(1), execution.drain()).await?;
        assert!(execution.tasks.lock().unwrap().is_empty());
        assert_eq!(
            Arc::strong_count(&execution),
            1,
            "guest Store still owns execution after drain"
        );
        Ok(())
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    #[ignore = "requires LOOM_SHARED_TEST_MODULE compiled production SDK module"]
    async fn actual_sdk_module_rendezvous() -> Result<()> {
        let bytes = std::fs::read(std::env::var("LOOM_SHARED_TEST_MODULE")?)?;
        let runtime = Runtime::new(Store::memory()?)?;
        let module = Module::new(&runtime.inner.core_engine, bytes).map_err(error)?;
        let memory_type = module
            .imports()
            .find_map(|import| match import.ty() {
                ExternType::Memory(memory) => Some(memory),
                _ => None,
            })
            .context("memory")?;
        let memory = SharedMemory::new(&runtime.inner.core_engine, memory_type).map_err(error)?;
        let execution = Arc::new(Execution {
            handler_instances: Mutex::new(Vec::new()),
            handler_instance_reuses: AtomicU64::new(0),
            handler_round_trip_us: Mutex::new(Vec::new()),
            handlers_next: AtomicU64::new(1),
            continuations: Mutex::new(HashMap::new()),
            handler_failure: Mutex::new(None),
            runtime,
            module,
            memory,
            effects: EffectContext::default(),
            pure: false,
            jobs: Mutex::new(HashMap::new()),
            tasks: Mutex::new(Vec::new()),
            job_count: AtomicU64::new(0),
            initialization: AsyncMutex::new(()),
            permits: Arc::new(Semaphore::new(8)),
            cancelled: AtomicBool::new(false),
            cancellation: Notify::new(),
            deadline: Instant::now() + Duration::from_secs(3),
        });
        let mut cleanup = Cleanup {
            execution: Some(execution.clone()),
        };
        let child = execution.clone();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        execution.schedule(async move {
            let result = child
                .invoke(
                    "diagnostic".into(),
                    Invocation::Call {
                        args: encode(&json!([])).unwrap(),
                    },
                )
                .await;
            let _ = sender.send(result);
        })?;
        let result = receiver.await?;
        execution.cancel();
        execution.drain().await;
        cleanup.execution.take();
        assert_eq!(result?.decode()?, json!([10, 24, 2]));
        Ok(())
    }
    #[test]
    fn separate_execution_memories_do_not_alias() {
        let engine = engine().unwrap();
        let first = SharedMemory::new(&engine, wasmtime::MemoryType::shared(1, 1)).unwrap();
        let second = SharedMemory::new(&engine, wasmtime::MemoryType::shared(1, 1)).unwrap();
        copy_in(&first, 64, &[42]).unwrap();
        assert_eq!(copy_out(&second, 64, 1).unwrap(), vec![0]);
    }
}
