//! Root combinators re-enter the same dynamic guest handler stack through an
//! independent Store. No concurrent task ever borrows the performer's Caller.
use super::*;

pub(super) struct Bridge {
    execution: Arc<Execution>,
    handlers: Vec<Arc<HandlerFrame>>,
    control: AsyncMutex<Option<Running>>,
    allocations: Mutex<Vec<OwnedAllocation>>,
    active: AtomicU64,
    changed: Notify,
}
struct OwnedAllocation { pointer: u32, size: u32, align: u32 }
struct Active { bridge: Arc<Bridge> }
impl Drop for Active {
    fn drop(&mut self) {
        self.bridge.active.fetch_sub(1, Ordering::AcqRel);
        self.bridge.changed.notify_waiters();
    }
}
struct AbortOnDrop { abort: AbortHandle }
impl Drop for AbortOnDrop { fn drop(&mut self) { self.abort.abort(); } }

impl Bridge {
    pub(super) async fn create(caller: &mut Caller<'_, Guest>) -> Result<Arc<Self>> {
        let execution = caller.data().execution.clone();
        let stack = allocate(caller, STACK_BYTES, 16).await?;
        let tls_size = caller.get_export("__tls_size").and_then(|export| export.into_global()).context("missing TLS size")?.get(&mut *caller).i32().context("invalid TLS size")? as u32;
        let tls_align = caller.get_export("__tls_align").and_then(|export| export.into_global()).context("missing TLS alignment")?.get(&mut *caller).i32().context("invalid TLS alignment")? as u32;
        let tls_align = tls_alignment(tls_size, tls_align)?;
        let tls = if tls_size == 0 { 0 } else { allocate(caller, tls_size, tls_align).await? };
        caller.data_mut().permit.take();
        let mut control = execution.instantiate(format!("{}/effect-control", caller.data().scope), Some(Allocation { stack: stack + STACK_BYTES, tls }), Vec::new()).await?;
        control.store.data_mut().permit.take();
        Ok(Arc::new(Self {
            execution, handlers: caller.data().handlers.clone(), control: AsyncMutex::new(Some(control)),
            allocations: Mutex::new(vec![OwnedAllocation { pointer: stack, size: STACK_BYTES, align: 16 }, OwnedAllocation { pointer: tls, size: tls_size, align: tls_align }]),
            active: AtomicU64::new(0), changed: Notify::new(),
        }))
    }
    async fn allocation(&self) -> Result<Allocation> {
        let mut guard = self.control.lock().await;
        let control = guard.as_mut().context("effect bridge closed")?;
        control.store.data_mut().permit = Some(self.execution.permit().await?);
        let tls_size = control.instance.get_global(&mut control.store, "__tls_size").context("missing TLS size")?.get(&mut control.store).i32().context("invalid TLS size")? as u32;
        let tls_align = control.instance.get_global(&mut control.store, "__tls_align").context("missing TLS alignment")?.get(&mut control.store).i32().context("invalid TLS alignment")? as u32;
        let tls_align = tls_alignment(tls_size, tls_align)?;
        let allocator = control.instance.get_typed_func::<(i32, i32), i32>(&mut control.store, "loom_alloc").map_err(error)?;
        let stack = allocator.call_async(&mut control.store, (STACK_BYTES as i32, 16)).await.map_err(error)? as u32;
        anyhow::ensure!(stack != 0, "effect bridge stack allocation failed");
        let tls = if tls_size == 0 { 0 } else { allocator.call_async(&mut control.store, (tls_size as i32, tls_align as i32)).await.map_err(error)? as u32 };
        anyhow::ensure!(tls_size == 0 || tls != 0, "effect bridge TLS allocation failed");
        anyhow::ensure!(stack % 16 == 0 && stack as u64 + STACK_BYTES as u64 <= self.execution.memory.data_size() as u64, "invalid effect bridge stack allocation");
        anyhow::ensure!(tls % tls_align == 0 && tls as u64 + tls_size as u64 <= self.execution.memory.data_size() as u64, "invalid effect bridge TLS allocation");
        self.allocations.lock().unwrap().extend([OwnedAllocation { pointer: stack, size: STACK_BYTES, align: 16 }, OwnedAllocation { pointer: tls, size: tls_size, align: tls_align }]);
        control.store.data_mut().permit.take();
        Ok(Allocation { stack: stack + STACK_BYTES, tls })
    }
    pub(super) async fn close(&self, caller: &mut Caller<'_, Guest>) -> Result<()> {
        caller.data_mut().permit.take();
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            self.execution.check()?;
            if self.active.load(Ordering::Acquire) == 0 { break; }
            tokio::select! {
                _ = changed => {},
                _ = self.execution.cancellation.notified() => self.execution.check()?,
                _ = tokio::time::sleep_until(self.execution.deadline.into()) => bail!("effect bridge deadline exceeded"),
            }
        }
        // Active is decremented only after each scheduled guest future/Store
        // is destroyed. Race losers are aborted by their dispatch future guard.
        self.control.lock().await.take();
        caller.data_mut().permit = Some(self.execution.permit().await?);
        let free = caller.get_export("loom_dealloc").and_then(|export| export.into_func()).context("missing loom_dealloc")?.typed::<(i32, i32, i32), ()>(&*caller).map_err(error)?;
        let allocations = std::mem::take(&mut *self.allocations.lock().unwrap());
        for allocation in allocations {
            if allocation.size != 0 { free.call_async(&mut *caller, (allocation.pointer as i32, allocation.size as i32, allocation.align as i32)).await.map_err(error)?; }
        }
        Ok(())
    }
}
impl RootDispatch for Arc<Bridge> {
    fn dispatch<'a>(&'a self, desc: Value, scope: &'a str, occurrence: i64, _effects: EffectContext) -> futures::future::BoxFuture<'a, Result<EffectOutput>> {
        Box::pin(async move {
            self.execution.job_count.fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_JOBS as u64).then_some(count + 1)
            }).map_err(|_| anyhow::anyhow!("shared execution lifetime job limit exceeded"))?;
            let bridge = self.clone();
            let scope = scope.to_owned();
            let encoded = encode(&desc)?;
            let (abort, registration) = AbortHandle::new_pair();
            let abort_guard = AbortOnDrop { abort };
            let (sender, receiver) = tokio::sync::oneshot::channel();
            self.active.fetch_add(1, Ordering::AcqRel);
            let active = Active { bridge: bridge.clone() };
            self.execution.schedule(async move {
                let mut operation = Box::pin(Abortable::new(async {
                    let allocation = bridge.allocation().await?;
                    let mut running = bridge.execution.instantiate(scope, Some(allocation), bridge.handlers.clone()).await?;
                    running.store.data_mut().occurrence = occurrence;
                    let input = running.input_encoded(&encoded).await?;
                    let entry = running.instance.get_typed_func::<(i32, i32), i64>(&mut running.store, "loom_effect_run").map_err(error)?;
                    let packed = entry.call_async(&mut running.store, (input.pointer, input.length)).await.map_err(|cause| running.error_context(cause))? as u64;
                    let bytes = copy_out(&bridge.execution.memory, packed as u32, (packed >> 32) as u32)?;
                    bridge.allocations.lock().unwrap().extend([OwnedAllocation { pointer: input.pointer as u32, size: input.length as u32, align: 1 }, OwnedAllocation { pointer: packed as u32, size: (packed >> 32) as u32, align: 1 }]);
                    let envelope: Value = loom_proto::decode(&bytes).map_err(anyhow::Error::msg)?;
                    if let Some(message) = envelope.get("error").and_then(Value::as_str) { bail!("{message}"); }
                    EffectOutput::value(envelope.get("ok").context("invalid bridge effect response")?)
                }, registration));
                let result = (&mut operation).await;
                drop(operation);
                drop(active);
                let _ = sender.send(result.unwrap_or_else(|_| Err(anyhow::anyhow!("effect bridge cancelled"))));
            })?;
            let result = receiver.await.context("effect bridge task ended without result")?;
            drop(abort_guard);
            result
        })
    }
}
