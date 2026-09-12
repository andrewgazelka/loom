//! Dynamic handler frames live only in their execution's shared memory. Callback
//! Stores see the outer stack; the suspended performer retains its deep stack.
use super::*;

pub(super) struct HandlerFrame {
    pub(super) id: u64,
    owner: String,
    function: i32,
    data: i32,
    labels: Option<Vec<String>>,
    serial: AsyncMutex<()>,
}
pub(super) struct ContinuationState {
    result: Mutex<Option<std::result::Result<Value, String>>>,
    changed: Notify,
}
impl ContinuationState {
    fn finish(&self, result: std::result::Result<Value, String>) -> Result<()> {
        let mut slot = self.result.lock().unwrap();
        anyhow::ensure!(slot.is_none(), "continuation already consumed");
        *slot = Some(result);
        self.changed.notify_waiters();
        Ok(())
    }
    async fn wait(&self, execution: &Execution) -> Result<std::result::Result<Value, String>> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let cancelled = execution.cancellation.notified();
            execution.check()?;
            if let Some(result) = self.result.lock().unwrap().clone() {
                return Ok(result);
            }
            tokio::select! {
                _ = changed => {},
                _ = cancelled => execution.check()?,
                _ = tokio::time::sleep_until(execution.deadline.into()) => bail!("shared execution deadline exceeded"),
            }
        }
    }
}

pub(super) fn link(linker: &mut wasmtime::Linker<Guest>) -> Result<()> {
    linker.func_wrap("loom", "handle_push", |mut caller: Caller<'_, Guest>, function: i32, data: i32, pointer: i32, length: i32| -> std::result::Result<i64, wasmtime::Error> {
        let result: Result<i64> = (|| {
            let execution = caller.data().execution.clone();
            execution.check()?;
            anyhow::ensure!(caller.data().handlers.len() < MAX_JOBS, "handler stack limit exceeded");
            let bytes = copy_out(&execution.memory, pointer as u32, length as u32)?;
            let value: Value = loom_proto::decode(&bytes).map_err(anyhow::Error::msg)?;
            let labels = if value.is_null() { None } else {
                Some(value.as_array().context("handler labels must be an array or null")?.iter()
                    .map(|label| label.as_str().map(str::to_owned).context("handler label must be a string"))
                    .collect::<Result<Vec<_>>>()?)
            };
            let id = execution.handlers_next.fetch_add(1, Ordering::Relaxed);
            anyhow::ensure!(id != 0 && id <= i64::MAX as u64, "handler identity exhausted");
            let frame = Arc::new(HandlerFrame { id, owner: caller.data().scope.clone(), function, data, labels, serial: AsyncMutex::new(()) });
            caller.data_mut().handlers.push(frame);
            Ok(id as i64)
        })();
        result.map_err(host_error)
    }).map_err(error)?;
    linker.func_wrap_async("loom", "handle_pop", |mut caller: Caller<'_, Guest>, (id,): (i64,)| Box::new(async move {
        let result: Result<i32> = async {
            let execution = caller.data().execution.clone();
            let frame = caller.data().handlers.last().cloned().context("empty handler stack")?;
            anyhow::ensure!(frame.id == id as u64 && frame.owner == caller.data().scope, "handler pop must match owned top frame");
            caller.data_mut().permit.take();
            // Descendants can still enter this frame until their Stores finish.
            // SDK storage must remain live until this barrier succeeds.
            loop {
                execution.check()?;
                let pending = execution.jobs.lock().unwrap().values()
                    .find(|job| job.handlers.contains(&frame.id) && job.result.lock().unwrap().is_none()).cloned();
                let Some(job) = pending else { break; };
                let done = job.done.notified();
                tokio::pin!(done);
                done.as_mut().enable();
                if job.result.lock().unwrap().is_some() { continue; }
                tokio::select! {
                    _ = done => {},
                    _ = execution.cancellation.notified() => execution.check()?,
                    _ = tokio::time::sleep_until(execution.deadline.into()) => bail!("shared execution deadline exceeded"),
                }
            }
            caller.data_mut().permit = Some(execution.permit().await?);
            caller.data_mut().handlers.pop();
            Ok(0)
        }.await;
        result.map_err(host_error)
    })).map_err(error)?;
    linker.func_wrap("loom", "resume", |caller: Caller<'_, Guest>, id: i64, pointer: i32, length: i32| -> std::result::Result<i32, wasmtime::Error> {
        let result: Result<i32> = (|| {
            let execution = &caller.data().execution;
            let state = execution.continuations.lock().unwrap().get(&(id as u64)).cloned().context("unknown or consumed continuation")?;
            let bytes = copy_out(&execution.memory, pointer as u32, length as u32)?;
            let value: Value = loom_proto::decode(&bytes).map_err(anyhow::Error::msg)?;
            state.finish(Ok(value))?;
            Ok(0)
        })();
        result.map_err(host_error)
    }).map_err(error)?;
    for name in ["abandon", "continuation_drop"] {
        let reason = if name == "abandon" { "continuation abandoned" } else { "continuation dropped" };
        linker.func_wrap("loom", name, move |caller: Caller<'_, Guest>, id: i64| -> std::result::Result<i32, wasmtime::Error> {
            let result: Result<i32> = (|| {
                let execution = &caller.data().execution;
                let state = execution.continuations.lock().unwrap().get(&(id as u64)).cloned().context("unknown or consumed continuation")?;
                state.finish(Err(reason.into()))?;
                Ok(0)
            })();
            result.map_err(host_error)
        }).map_err(error)?;
    }
    Ok(())
}

pub(super) fn dispatch<'a>(caller: &'a mut Caller<'_, Guest>, descriptor: &'a Value, occurrence: i64) -> futures::future::BoxFuture<'a, Result<Option<Vec<u8>>>> {
    Box::pin(async move {
    let execution = caller.data().execution.clone();
    let label = descriptor.get("op").and_then(Value::as_str).context("effect op required")?;
    let frames = caller.data().handlers.clone();
    for index in (0..frames.len()).rev() {
        let frame = frames[index].clone();
        if frame.labels.as_ref().is_some_and(|labels| !labels.iter().any(|candidate| candidate == label)) { continue; }
        caller.data_mut().permit.take();
        // Never occupy an execution permit while queued on an FnMut handler.
        let serial = tokio::select! {
            guard = frame.serial.lock() => guard,
            _ = execution.cancellation.notified() => bail!("shared execution cancelled"),
            _ = tokio::time::sleep_until(execution.deadline.into()) => bail!("shared execution deadline exceeded"),
        };
        execution.check()?;
        caller.data_mut().permit = Some(execution.permit().await?);
        let id = execution.handlers_next.fetch_add(1, Ordering::Relaxed);
        let state = Arc::new(ContinuationState { result: Mutex::new(None), changed: Notify::new() });
        execution.continuations.lock().unwrap().insert(id, state.clone());
        // Opaque frame/continuation IDs are scheduling-dependent. Replay keys
        // instead use the performer's lexical scope, occurrence and stack depth.
        let callback_scope = format!("{}/effect:{occurrence}/handler:{index}", caller.data().scope);
        let result = invoke(caller, &frame, id, &frames[..index], descriptor, callback_scope).await;
        drop(serial);
        let result: Result<Option<std::result::Result<Value, String>>> = async {
            let reply = result?;
            anyhow::ensure!(reply.as_object().is_some_and(|object| object.len() == 1), "invalid handler reply");
            if let Some(value) = reply.get("resume") {
                state.finish(Ok(value.clone()))?;
            } else if reply.get("forward") == Some(&Value::Null) {
                anyhow::ensure!(frame.labels.is_none(), "selected handler label cannot forward");
                anyhow::ensure!(state.result.lock().unwrap().is_none(), "consumed continuation cannot forward");
                return Ok(None);
            } else {
                anyhow::ensure!(reply.get("deferred") == Some(&Value::Null), "invalid handler reply");
            }
            Ok(Some(state.wait(&execution).await?))
        }.await;
        execution.continuations.lock().unwrap().remove(&id);
        match result {
            Ok(Some(Ok(value))) => {
                caller.data_mut().permit = Some(execution.permit().await?);
                let bytes = encode(&json!({"ok": value}))?;
                return Ok(Some(bytes));
            }
            Ok(Some(Err(message))) if message == "continuation dropped" => {
                // Dropping a continuation fails the suspended perform, which
                // returns the SDK's ordinary EffectError. Explicit abandon is
                // stronger: it cancels the body and drains its scoped children.
                caller.data_mut().permit = Some(execution.permit().await?);
                caller.data_mut().last_effect_error = Some(format!("handler frame {}: {message}", frame.id));
                return Ok(Some(encode(&json!({"error": message}))?));
            }
            Ok(Some(Err(message))) => {
                let failure = execution.original_failure().unwrap_or_else(|| ExecutionFailure::new(
                    GuestFailure::new(format!("handler frame {}: {message}", frame.id)).into()
                ));
                *execution.handler_failure.lock().unwrap() = Some(failure.clone());
                execution.cancel();
                return Err(failure.into_error());
            }
            Ok(None) => { caller.data_mut().permit = Some(execution.permit().await?); }
            Err(error) => {
                let failure = execution.original_failure().unwrap_or_else(|| ExecutionFailure::new(
                    error.context(format!("handler frame {}", frame.id))
                ));
                *execution.handler_failure.lock().unwrap() = Some(failure.clone());
                execution.cancel();
                return Err(failure.into_error());
            }
        }
    }
    Ok(None)
    })
}

const MAX_CACHED_INSTANCES: usize = 8;

pub(super) struct HandlerInstance {
    pub(super) running: Running,
    pub(super) stack: u32,
    pub(super) tls: u32,
    pub(super) tls_size: u32,
    pub(super) tls_align: u32,
}
impl HandlerInstance {
    async fn reset(&mut self, scope: String, outer: &[Arc<HandlerFrame>]) -> Result<()> {
        let execution = self.running.store.data().execution.clone();
        self.running.store.data_mut().permit = Some(execution.permit().await?);
        let guest = self.running.store.data_mut();
        guest.scope = scope;
        guest.occurrence = 0;
        guest.last_effect_error = None;
        guest.handlers = outer.to_vec();
        self.running.store.set_epoch_deadline(1);
        let instance = self.running.instance;
        instance.get_global(&mut self.running.store, "__loom_stack_low").context("missing stack lower bound")?
            .set(&mut self.running.store, wasmtime::Val::I32(self.stack as i32)).map_err(error)?;
        instance.get_global(&mut self.running.store, "__loom_stack_high").context("missing stack upper bound")?
            .set(&mut self.running.store, wasmtime::Val::I32((self.stack + STACK_BYTES) as i32)).map_err(error)?;
        instance.get_global(&mut self.running.store, "__stack_pointer").context("missing stack pointer")?
            .set(&mut self.running.store, wasmtime::Val::I32((self.stack + STACK_BYTES) as i32)).map_err(error)?;
        instance.get_typed_func::<i32, ()>(&mut self.running.store, "__wasm_init_tls").map_err(error)?
            .call_async(&mut self.running.store, self.tls as i32).await.map_err(error)?;
        Ok(())
    }
    fn park(&mut self) {
        let guest = self.running.store.data_mut();
        guest.permit.take();
        // Borrowed handler captures must never be retained by the cache after
        // handle_pop. The next checkout supplies a fresh dynamic context.
        guest.handlers.clear();
        guest.scope.clear();
        guest.occurrence = 0;
        guest.last_effect_error = None;
    }
}

async fn invoke(caller: &mut Caller<'_, Guest>, frame: &HandlerFrame, id: u64, outer: &[Arc<HandlerFrame>], descriptor: &Value, scope: String) -> Result<Value> {
    let execution = caller.data().execution.clone();
    let cached = execution.handler_instances.lock().unwrap().pop();
    let mut cached = if let Some(mut cached) = cached {
        caller.data_mut().permit.take();
        cached.reset(scope, outer).await?;
        execution.handler_instance_reuses.fetch_add(1, Ordering::Relaxed);
        cached
    } else {
        let stack = allocate(caller, STACK_BYTES, 16).await?;
        let tls_size = caller.get_export("__tls_size").and_then(|export| export.into_global()).context("missing TLS size")?.get(&mut *caller).i32().context("invalid TLS size")? as u32;
        let tls_align = caller.get_export("__tls_align").and_then(|export| export.into_global()).context("missing TLS alignment")?.get(&mut *caller).i32().context("invalid TLS alignment")? as u32;
        let tls_align = tls_alignment(tls_size, tls_align)?;
        let tls = if tls_size == 0 { 0 } else { allocate(caller, tls_size, tls_align).await? };
        caller.data_mut().permit.take();
        let running = execution.instantiate(scope, Some(Allocation { stack: stack + STACK_BYTES, tls }), outer.to_vec()).await?;
        HandlerInstance { running, stack, tls, tls_size, tls_align }
    };
    let running = &mut cached.running;
    let op = encode(&json!({"name": descriptor.get("op"), "args": descriptor.get("args").unwrap_or(&Value::Null)}))?;
    let input = running.input_encoded(&op).await?;
    let function = running.instance.get_typed_func::<(i32, i32, i64, i32, i32), i64>(&mut running.store, "loom_handler_run").map_err(error)?;
    let packed = function.call_async(&mut running.store, (frame.function, frame.data, id as i64, input.pointer, input.length)).await.map_err(|cause| running.error_context(cause))? as u64;
    let bytes = copy_out(&execution.memory, packed as u32, (packed >> 32) as u32)?;
    let reply: Value = loom_proto::decode(&bytes).map_err(anyhow::Error::msg)?;
    // Reclaim transferred byte buffers while this Store still has its permit.
    // Its own stack/TLS remain allocated for reuse within this execution only.
    let free = running.instance.get_typed_func::<(i32, i32, i32), ()>(&mut running.store, "loom_dealloc").map_err(error)?;
    free.call_async(&mut running.store, (input.pointer, input.length, 1)).await.map_err(error)?;
    if packed >> 32 != 0 {
        free.call_async(&mut running.store, (packed as u32 as i32, (packed >> 32) as i32, 1)).await.map_err(error)?;
    }
    cached.park();
    let mut unused = Some(cached);
    {
        let mut pool = execution.handler_instances.lock().unwrap();
        if pool.len() < MAX_CACHED_INSTANCES && !execution.cancelled.load(Ordering::Acquire) {
            pool.push(unused.take().unwrap());
        }
    }
    if let Some(unused) = unused {
        let stack = unused.stack;
        let tls = unused.tls;
        let tls_size = unused.tls_size;
        let tls_align = unused.tls_align;
        // Destroy the Store before freeing its stack through another Store.
        drop(unused);
        caller.data_mut().permit = Some(execution.permit().await?);
        let free = caller.get_export("loom_dealloc").and_then(|export| export.into_func()).context("missing loom_dealloc")?.typed::<(i32, i32, i32), ()>(&*caller).map_err(error)?;
        free.call_async(&mut *caller, (stack as i32, STACK_BYTES as i32, 16)).await.map_err(error)?;
        if tls_size != 0 { free.call_async(&mut *caller, (tls as i32, tls_size as i32, tls_align as i32)).await.map_err(error)?; }
        caller.data_mut().permit.take();
    }
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cached_instance_resets_tls_stack_context_and_releases_execution_cycle() -> Result<()> {
        let runtime = Runtime::new(Store::memory()?)?;
        let module = Module::new(&runtime.inner.core_engine, r#"(module
            (import "env" "memory" (memory 8 8 shared))
            (global (export "__stack_pointer") (mut i32) (i32.const 65536))
            (global (export "__loom_stack_low") (mut i32) (i32.const 1024))
            (global (export "__loom_stack_high") (mut i32) (i32.const 65536))
            (func (export "__wasm_init_tls") (param i32)
                local.get 0 i32.const 7 i32.atomic.store8))"#).map_err(error)?;
        let memory = SharedMemory::new(&runtime.inner.core_engine, wasmtime::MemoryType::shared(8, 8)).map_err(error)?;
        let execution = Arc::new(Execution {
            handler_instances: Mutex::new(Vec::new()), handler_instance_reuses: AtomicU64::new(0),
            handler_round_trip_us: Mutex::new(Vec::new()), handlers_next: AtomicU64::new(1),
            continuations: Mutex::new(HashMap::new()), handler_failure: Mutex::new(None),
            runtime, module, memory, effects: EffectContext::default(), pure: true,
            jobs: Mutex::new(HashMap::new()), tasks: Mutex::new(Vec::new()), job_count: AtomicU64::new(0),
            initialization: AsyncMutex::new(()), permits: Arc::new(Semaphore::new(8)),
            cancelled: AtomicBool::new(false), cancellation: Notify::new(),
            deadline: Instant::now() + Duration::from_secs(3),
        });
        let frame = Arc::new(HandlerFrame { id: 1, owner: "old".into(), function: 0, data: 0, labels: None, serial: AsyncMutex::new(()) });
        let running = execution.instantiate("old".into(), Some(Allocation { stack: 4096 + STACK_BYTES, tls: 400_000 }), vec![frame.clone()]).await?;
        let mut cached = HandlerInstance { running, stack: 4096, tls: 400_000, tls_size: 16, tls_align: 16 };
        copy_in(&execution.memory, cached.tls, &[99])?;
        cached.running.store.data_mut().occurrence = 99;
        cached.running.store.data_mut().last_effect_error = Some("old error".into());
        cached.running.instance.get_global(&mut cached.running.store, "__stack_pointer").unwrap()
            .set(&mut cached.running.store, wasmtime::Val::I32(5000)).map_err(error)?;
        cached.park();
        assert_eq!(Arc::strong_count(&frame), 1, "parked Store retained a borrowed frame");
        assert_eq!(execution.permits.available_permits(), 8);
        cached.reset("new".into(), &[]).await?;
        assert_eq!(copy_out(&execution.memory, cached.tls, 1)?, vec![7], "TLS initializer did not run on reuse");
        assert_eq!(cached.running.store.data().scope, "new");
        assert_eq!(cached.running.store.data().occurrence, 0);
        assert!(cached.running.store.data().last_effect_error.is_none());
        assert_eq!(cached.running.instance.get_global(&mut cached.running.store, "__stack_pointer").unwrap().get(&mut cached.running.store).i32(), Some((4096 + STACK_BYTES) as i32));
        cached.park();
        execution.handler_instances.lock().unwrap().push(cached);
        assert!(Arc::strong_count(&execution) > 1);
        execution.cancel();
        execution.drain().await;
        assert!(execution.handler_instances.lock().unwrap().is_empty());
        assert_eq!(Arc::strong_count(&execution), 1, "cached Store leaked the execution");
        Ok(())
    }

    #[test]
    fn continuation_is_one_shot_even_when_results_race() {
        let state = Arc::new(ContinuationState { result: Mutex::new(None), changed: Notify::new() });
        let first = state.clone();
        let second = state.clone();
        let first = std::thread::spawn(move || first.finish(Ok(json!(1))).is_ok());
        let second = std::thread::spawn(move || second.finish(Ok(json!(2))).is_ok());
        assert_ne!(first.join().unwrap(), second.join().unwrap());
        assert!(state.finish(Err("continuation dropped".into())).is_err());
    }

    #[test]
    fn drop_and_abandon_remain_distinguishable_errors() {
        for reason in ["continuation dropped", "continuation abandoned"] {
            let state = ContinuationState { result: Mutex::new(None), changed: Notify::new() };
            state.finish(Err(reason.into())).unwrap();
            assert_eq!(state.result.lock().unwrap().as_ref().unwrap().as_ref().unwrap_err(), reason);
        }
    }
}
