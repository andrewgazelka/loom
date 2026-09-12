use super::*;

impl Execution {
    pub(super) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        // Parked Stores are not executing. Drop them synchronously to break
        // Execution -> cached Store -> Execution ownership cycles.
        self.handler_instances.lock().unwrap().clear();
        self.cancellation.notify_waiters();
        self.runtime.inner.core_engine.increment_epoch();
    }
    pub(super) fn check(&self) -> Result<()> {
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
    pub(super) fn schedule(
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
                if outcome.is_err()
                    && let Some(execution) = execution.upgrade()
                {
                    execution.cancel();
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
    pub(super) async fn drain(&self) {
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
    pub(super) async fn permit(&self) -> Result<OwnedSemaphorePermit> {
        let cancelled = self.cancellation.notified();
        self.check()?;
        tokio::select! {
            result = self.permits.clone().acquire_owned() => Ok(result?),
            _ = cancelled => bail!("shared execution cancelled"),
            _ = tokio::time::sleep_until(self.deadline.into()) => bail!("shared execution deadline exceeded"),
        }
    }
    pub(super) async fn instantiate(
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

impl Execution {
    pub(super) async fn invoke(
        self: &Arc<Self>,
        scope: String,
        invocation: Invocation,
    ) -> Result<EffectOutput> {
        let mut running = self.instantiate(scope, None, Vec::new()).await?;
        let packed = match invocation {
            Invocation::Schema => {
                if running
                    .instance
                    .get_export(&mut running.store, "loom_schema")
                    .is_none()
                {
                    return EffectOutput::value(&json!(""));
                }
                running
                    .instance
                    .get_typed_func::<(), i64>(&mut running.store, "loom_schema")
                    .map_err(error)?
                    .call_async(&mut running.store, ())
                    .await
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
        let output = envelope
            .get("ok")
            .ok_or_else(|| GuestFailure::new("invalid core result envelope"))?;
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

impl Running {
    pub(super) fn error_context(&self, cause: wasmtime::Error) -> anyhow::Error {
        let cause = error(cause);
        match &self.store.data().last_effect_error {
            Some(message) => cause.context(format!("last returned guest effect error: {message}")),
            None => cause,
        }
    }
    pub(super) async fn input_encoded(&mut self, bytes: &[u8]) -> Result<Buffer> {
        let length = bytes.len().try_into()?;
        let pointer = self
            .instance
            .get_typed_func::<(i32, i32), i32>(&mut self.store, "loom_alloc")
            .map_err(error)?
            .call_async(&mut self.store, (length, 1))
            .await
            .map_err(error)?;
        anyhow::ensure!(
            bytes.is_empty() || pointer != 0,
            "guest input allocation failed"
        );
        copy_in(&self.store.data().execution.memory, pointer as u32, bytes)?;
        Ok(Buffer { pointer, length })
    }
}
